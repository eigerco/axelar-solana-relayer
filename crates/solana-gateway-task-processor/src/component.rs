use core::future::Future;
use core::pin::Pin;
use core::task::Poll;
use std::collections::VecDeque;
use std::sync::Arc;

use amplifier_api::chrono::DateTime;
use amplifier_api::types::{
    BigInt, CannotExecuteMessageEventV2, CannotExecuteMessageEventV2Metadata,
    CannotExecuteMessageReason, Event, EventBase, EventId, EventMetadata, MessageExecutedEvent,
    MessageExecutedEventMetadata, MessageExecutionStatus, PublishEventsRequest, TaskItem,
    TaskItemId, Token, TxEvent,
};
use axelar_executable::AxelarMessagePayload;
use axelar_solana_encoding::borsh::BorshDeserialize as _;
use axelar_solana_encoding::types::execute_data::{ExecuteData, MerkleisedPayload};
use axelar_solana_encoding::types::messages::{CrossChainId, Message};
use axelar_solana_gateway::error::GatewayError;
use axelar_solana_gateway::state::incoming_message::command_id;
use effective_tx_sender::ComputeBudgetError;
use eyre::{Context as _, OptionExt as _};
use futures::stream::{FusedStream as _, FuturesOrdered, FuturesUnordered};
use futures::{SinkExt as _, StreamExt as _};
use num_traits::FromPrimitive as _;
use relayer_amplifier_api_integration::AmplifierCommand;
use relayer_amplifier_state::State;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_client::rpc_response::RpcSimulateTransactionResult;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::{Instruction, InstructionError};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::TransactionError;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding, UiTransactionStatusMeta,
};
use tracing::{info_span, instrument, Instrument as _};

use crate::config;

mod message_payload;

/// A component that pushes transactions over to the Solana blockchain.
/// The transactions to push are dependant on the events that the Amplifier API will provide
pub struct SolanaTxPusher<S: State> {
    config: config::Config,
    name_on_amplifier: String,
    rpc_client: Arc<RpcClient>,
    task_receiver: relayer_amplifier_api_integration::AmplifierTaskReceiver,
    amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
    state: S,
}

impl<S: State> relayer_engine::RelayerComponent for SolanaTxPusher<S> {
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

impl<S: State> SolanaTxPusher<S> {
    /// Create a new [`SolanaTxPusher`] component
    #[must_use]
    pub const fn new(
        config: config::Config,
        name_on_amplifier: String,
        rpc_client: Arc<RpcClient>,
        task_receiver: relayer_amplifier_api_integration::AmplifierTaskReceiver,
        amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
        state: S,
    ) -> Self {
        Self {
            config,
            name_on_amplifier,
            rpc_client,
            task_receiver,
            amplifier_client,
            state,
        }
    }

    async fn process_internal(self) -> eyre::Result<()> {
        let config_metadata = Arc::new(self.get_config_metadata());
        let state = self.state.clone();

        let keypair = Arc::new(self.config.signing_keypair.insecure_clone());
        let mut futures_ordered = FuturesOrdered::new();
        let mut rx = self.task_receiver.receiver.fuse();
        let mut task_stream = futures::stream::poll_fn(move |cx| {
            // check if we have new requests to add to the join set
            match rx.poll_next_unpin(cx) {
                Poll::Ready(Some(task)) => {
                    // spawn the task on the joinset, returning the error
                    tracing::info!(?task, "received task from amplifier API");
                    futures_ordered.push_back({
                        let solana_rpc_client = Arc::clone(&self.rpc_client);
                        let keypair = Arc::clone(&keypair);
                        let config_metadata = Arc::clone(&config_metadata);
                        let amplifier_client = self.amplifier_client.clone();
                        async move {
                            let command_id = task.id.clone();
                            let res = process_task(
                                &keypair,
                                &solana_rpc_client,
                                amplifier_client,
                                task,
                                &config_metadata,
                            )
                            .await;
                            (command_id, res)
                        }
                    });
                }
                Poll::Pending => (),
                Poll::Ready(None) => {
                    tracing::error!("receiver channel closed");
                }
            }
            // check if any background tasks are done
            match futures_ordered.poll_next_unpin(cx) {
                Poll::Ready(Some(res)) => Poll::Ready(Some(res)),
                // futures unordered returns `Poll::Ready(None)` when it's empty
                Poll::Ready(None) => {
                    if rx.is_terminated() {
                        return Poll::Ready(None)
                    }
                    Poll::Pending
                }
                Poll::Pending => Poll::Pending,
            }
        });

        while let Some((task_item_id, task_result)) = task_stream.next().await {
            state.set_latest_processed_task_id(task_item_id)?;
            let Err(err) = task_result else {
                continue;
            };

            tracing::error!(?err, "background task returned an error");
        }

        eyre::bail!("fatal error")
    }

    fn get_config_metadata(&self) -> ConfigMetadata {
        let gateway_root_pda = axelar_solana_gateway::get_gateway_root_config_pda().0;
        ConfigMetadata {
            gateway_root_pda,
            name_of_the_solana_chain: self.name_on_amplifier.clone(),
        }
    }
}

struct ConfigMetadata {
    name_of_the_solana_chain: String,
    gateway_root_pda: Pubkey,
}

#[instrument(skip_all)]
async fn process_task(
    keypair: &Keypair,
    solana_rpc_client: &RpcClient,
    mut amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
    task_item: TaskItem,
    metadata: &ConfigMetadata,
) -> eyre::Result<()> {
    use amplifier_api::types::Task::{Execute, GatewayTx, Refund, Verify};
    let signer = keypair.pubkey();
    let gateway_root_pda = metadata.gateway_root_pda;

    match task_item.task {
        Verify(_verify_task) => {
            tracing::warn!("solana blockchain is not supposed to receive the `verify_task`");
        }
        GatewayTx(task) => {
            gateway_tx_task(task, gateway_root_pda, signer, solana_rpc_client, keypair).await?;
        }
        Execute(task) => {
            let source_chain = task.message.source_chain.clone();
            let message_id = task.message.message_id.clone();

            // communicate with the destination program
            if let Err(error) = execute_task(task, metadata, signer, solana_rpc_client, keypair)
                .instrument(info_span!("execute task"))
                .in_current_span()
                .await
            {
                let event = match error.downcast_ref::<ComputeBudgetError>() {
                    Some(&ComputeBudgetError::TransactionError {
                        source: ref _source,
                        signature,
                    }) => {
                        let (meta, maybe_block_time) =
                            get_confirmed_transaction_metadata(solana_rpc_client, &signature)
                                .await?;

                        message_executed_event(
                            signature,
                            source_chain,
                            message_id,
                            MessageExecutionStatus::Reverted,
                            maybe_block_time,
                            Token {
                                token_id: None,
                                amount: BigInt::from_u64(meta.fee),
                            },
                        )
                    }
                    _ => {
                        // Any other error, probably happening before execution: Simulation error,
                        // error building an instruction, parsing pubkey, rpc transport error,
                        // etc.
                        cannot_execute_message_event(
                            task_item.id,
                            source_chain,
                            message_id,
                            CannotExecuteMessageReason::Error,
                            error.to_string(),
                        )
                    }
                };

                let command = AmplifierCommand::PublishEvents(PublishEventsRequest {
                    events: vec![event],
                });
                amplifier_client.sender.send(command).await?;
            };
        }
        Refund(_refund_task) => {
            tracing::error!("refund task not implemented");
        }
    };

    Ok(())
}

async fn get_confirmed_transaction_metadata(
    solana_rpc_client: &RpcClient,
    signature: &Signature,
) -> Result<(UiTransactionStatusMeta, Option<i64>), eyre::Error> {
    let config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Binary),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };

    let EncodedConfirmedTransactionWithStatusMeta {
        transaction: transaction_with_meta,
        block_time,
        ..
    } = solana_rpc_client
        .get_transaction_with_config(signature, config)
        .await?;

    let meta = transaction_with_meta
        .meta
        .ok_or_eyre("transaction metadata not available")?;

    Ok((meta, block_time))
}

fn message_executed_event(
    tx_signature: Signature,
    source_chain: String,
    message_id: TxEvent,
    status: MessageExecutionStatus,
    block_time: Option<i64>,
    cost: Token,
) -> Event {
    let event_id = EventId::tx_reverted_event_id(&tx_signature.to_string());
    let metadata = MessageExecutedEventMetadata::builder().build();
    let event_metadata = EventMetadata::builder()
        .timestamp(block_time.and_then(|secs| DateTime::from_timestamp(secs, 0)))
        .extra(metadata)
        .build();
    let event_base = EventBase::builder()
        .event_id(event_id)
        .meta(Some(event_metadata))
        .build();
    Event::MessageExecuted(MessageExecutedEvent {
        base: event_base,
        message_id,
        source_chain,
        status,
        cost,
    })
}

fn cannot_execute_message_event(
    task_item_id: TaskItemId,
    source_chain: String,
    message_id: TxEvent,
    reason: CannotExecuteMessageReason,
    details: String,
) -> Event {
    let event_id = EventId::cannot_execute_task_event_id(&task_item_id);
    let metadata = CannotExecuteMessageEventV2Metadata::builder()
        .task_item_id(task_item_id)
        .build();
    let event_metadata = EventMetadata::builder().extra(metadata).build();
    let event_base = EventBase::builder()
        .meta(Some(event_metadata))
        .event_id(event_id)
        .build();
    Event::CannotExecuteMessageV2(CannotExecuteMessageEventV2 {
        base: event_base,
        reason,
        details,
        message_id,
        source_chain,
    })
}

async fn execute_task(
    execute_task: amplifier_api::types::ExecuteTask,
    metadata: &ConfigMetadata,
    signer: Pubkey,
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
) -> Result<(), eyre::Error> {
    let payload = execute_task.payload;

    // compose the message
    let message = Message {
        cc_id: CrossChainId {
            chain: execute_task.message.source_chain,
            id: execute_task.message.message_id.0,
        },
        source_address: execute_task.message.source_address,
        destination_chain: metadata.name_of_the_solana_chain.clone(),
        destination_address: execute_task.message.destination_address,
        payload_hash: execute_task
            .message
            .payload_hash
            .try_into()
            .unwrap_or_default(),
    };
    let command_id = command_id(&message.cc_id.chain, &message.cc_id.id);
    let (gateway_incoming_message_pda, ..) =
        axelar_solana_gateway::get_incoming_message_pda(&command_id);

    // Upload the message payload to a Gateway-owned PDA account and get its address back.
    let gateway_message_payload_pda = message_payload::upload(
        solana_rpc_client,
        keypair,
        metadata.gateway_root_pda,
        &message,
        &payload,
    )
    .await?;

    // For compatibility reasons with the rest of the Axelar protocol we need add custom handling
    // for ITS & Governance programs
    let destination_address = message.destination_address.parse::<Pubkey>()?;
    match destination_address {
        axelar_solana_its::ID => {
            let ix = its_instruction_builder::build_its_gmp_instruction(
                signer,
                gateway_incoming_message_pda,
                gateway_message_payload_pda,
                message.clone(),
                payload,
                solana_rpc_client,
            )
            .await?;

            send_transaction(solana_rpc_client, keypair, ix).await?;
        }
        axelar_solana_governance::ID => {
            // todo Governance specific handling
            tracing::error!("governance program not yet supported");
        }
        _ => {
            validate_relayer_not_in_payload(&payload, signer)?;

            // if security passed, we broadcast the tx
            let ix = axelar_executable::construct_axelar_executable_ix(
                &message,
                &payload,
                gateway_incoming_message_pda,
                gateway_message_payload_pda,
            )?;
            send_transaction(solana_rpc_client, keypair, ix).await?;
        }
    }

    // Close the MessagePaynload PDA account to reclaim funds
    message_payload::close(
        solana_rpc_client,
        keypair,
        metadata.gateway_root_pda,
        &message,
    )
    .await?;
    Ok(())
}

/// Validates that the relayer's signing account is not included in the transaction payload.
///
/// This is a critical security check to prevent potential account draining attacks. Since the
/// relayer acts as a transaction signer, and `AxelarMessagePayload` allows dynamic account
/// appending, a malicious actors could include an instruction to transfer relayer's funds in the
/// transaction.
///
/// # Errors
/// Returns an error if the relayer's signing account is detected in the payload's account metadata.
/// Decoding errors are ignored, as they are considered non-critical.
fn validate_relayer_not_in_payload(payload: &[u8], signer: Pubkey) -> eyre::Result<()> {
    if let Ok(decoded_payload) = AxelarMessagePayload::decode(payload) {
        eyre::ensure!(
            decoded_payload
                .account_meta()
                .iter()
                .any(|acc| acc.pubkey == signer),
            "relayer will not execute a transaction where its own key is included",
        );
    }
    Ok(())
}

async fn gateway_tx_task(
    gateway_transaction_task: amplifier_api::types::GatewayTransactionTask,
    gateway_root_pda: Pubkey,
    signer: Pubkey,
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
) -> Result<(), eyre::Error> {
    // parse the ExecuteData
    let execute_data_bytes = gateway_transaction_task.execute_data.as_slice();
    let execute_data = ExecuteData::try_from_slice(execute_data_bytes)
        .map_err(|_err| eyre::eyre!("cannot decode execute data"))?;

    // Start a signing session
    let (verification_session_tracker_pda, ..) =
        axelar_solana_gateway::get_signature_verification_pda(
            &gateway_root_pda,
            &execute_data.payload_merkle_root,
        );
    let ix = axelar_solana_gateway::instructions::initialize_payload_verification_session(
        signer,
        gateway_root_pda,
        execute_data.payload_merkle_root,
    )?;
    send_gateway_tx(solana_rpc_client, keypair, ix).await?;

    // verify each signature in the signing session
    let mut verifier_ver_future_set = execute_data
        .signing_verifier_set_leaves
        .into_iter()
        .filter_map(|verifier_info| {
            let ix = axelar_solana_gateway::instructions::verify_signature(
                gateway_root_pda,
                verification_session_tracker_pda,
                execute_data.payload_merkle_root,
                verifier_info,
            )
            .ok()?;
            Some(send_gateway_tx(solana_rpc_client, keypair, ix))
        })
        .collect::<FuturesUnordered<_>>();
    while let Some(result) = verifier_ver_future_set.next().await {
        result?;
    }

    // determine whether we should do signer rotation or message approval
    match execute_data.payload_items {
        MerkleisedPayload::VerifierSetRotation {
            new_verifier_set_merkle_root,
        } => {
            let (new_verifier_set_tracker_pda, _) =
                axelar_solana_gateway::get_verifier_set_tracker_pda(new_verifier_set_merkle_root);
            let ix = axelar_solana_gateway::instructions::rotate_signers(
                gateway_root_pda,
                verification_session_tracker_pda,
                verification_session_tracker_pda,
                new_verifier_set_tracker_pda,
                signer,
                None,
                new_verifier_set_merkle_root,
            )?;
            send_gateway_tx(solana_rpc_client, keypair, ix).await?;
        }
        MerkleisedPayload::NewMessages { messages } => {
            let mut merkelised_message_f_set = messages
                .into_iter()
                .filter_map(|merkelised_message| {
                    let command_id = command_id(
                        merkelised_message.leaf.message.cc_id.chain.as_str(),
                        merkelised_message.leaf.message.cc_id.id.as_str(),
                    );
                    let (pda, _bump) = axelar_solana_gateway::get_incoming_message_pda(&command_id);
                    let ix = axelar_solana_gateway::instructions::approve_messages(
                        merkelised_message,
                        execute_data.payload_merkle_root,
                        gateway_root_pda,
                        signer,
                        verification_session_tracker_pda,
                        pda,
                    )
                    .ok()?;
                    Some(send_gateway_tx(solana_rpc_client, keypair, ix))
                })
                .collect::<FuturesUnordered<_>>();
            while let Some(result) = merkelised_message_f_set.next().await {
                result?;
            }
        }
    };
    Ok(())
}

/// Sends a transaction to the Solana blockchain.
///
/// # Errors
///
/// In case the transaction fails and the error is not recoverable relayer will stop processing
/// and return the error.
async fn send_gateway_tx(
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    ix: Instruction,
) -> eyre::Result<()> {
    let res = send_transaction(solana_rpc_client, keypair, ix).await;

    match res {
        Ok(_) => Ok(()),
        Err(err) => {
            let should_continue = if let ComputeBudgetError::SimulationError(
                RpcSimulateTransactionResult {
                    err:
                        Some(TransactionError::InstructionError(_, InstructionError::Custom(err_code))),
                    ..
                },
            ) = err
            {
                GatewayError::from_u32(err_code)
                    .is_some_and(|gw_err| gw_err.should_relayer_proceed())
            } else {
                false
            };

            if should_continue {
                Ok(())
            } else {
                tracing::warn!(?err, "Simulation error");
                Err(err).wrap_err("irrecoverable error")
            }
        }
    }
}

#[instrument(skip_all)]
async fn send_transaction(
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    ix: Instruction,
) -> Result<Signature, ComputeBudgetError> {
    effective_tx_sender::EffectiveTxSender::new(solana_rpc_client, keypair, VecDeque::from([ix]))
        .evaluate_compute_ixs()
        .await?
        .send_tx()
        .await
}

mod message_payload {
    use axelar_solana_encoding::types::messages::Message;
    use axelar_solana_gateway::state::incoming_message::command_id;
    use eyre::Context as _;
    use solana_sdk::instruction::Instruction;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer as _;

    // TODO: implement transaction handling logic.
    #[expect(clippy::todo, reason = "wip")]
    #[expect(clippy::unused_async, reason = "wip")]
    async fn send_tx(_ixs: &[Instruction]) -> eyre::Result<()> {
        todo!()
    }

    /// Upload a message payload to the PDA account
    pub(super) async fn upload(
        payer: &Keypair,
        gateway_root_pda: Pubkey,
        message: &Message,
        payload: &[u8],
    ) -> eyre::Result<Pubkey> {
        let msg_command_id = message_to_command_id(message);

        initialize(payer, gateway_root_pda, msg_command_id, payload).await?;
        write(payer, gateway_root_pda, msg_command_id, payload).await?;
        commit(payer, gateway_root_pda, msg_command_id).await?;

        let (message_payload_pda, _bump) = axelar_solana_gateway::find_message_payload_pda(
            gateway_root_pda,
            message_to_command_id(message),
            payer.pubkey(),
        );

        Ok(message_payload_pda)
    }

    async fn initialize(
        payer: &Keypair,
        gateway_root_pda: Pubkey,
        command_id: [u8; 32],
        payload: &[u8],
    ) -> eyre::Result<()> {
        let ix = axelar_solana_gateway::instructions::initialize_message_payload(
            gateway_root_pda,
            payer.pubkey(),
            command_id,
            payload
                .len()
                .try_into()
                .context("Unexpected u64 overflow in buffer size")?,
        )
        .context("failed to construct an instruction to initialize the message payload pda")?;
        send_tx(&[ix]).await?;
        Ok(())
    }

    async fn write(
        payer: &Keypair,
        gateway_root_pda: Pubkey,
        command_id: [u8; 32],
        payload: &[u8],
    ) -> eyre::Result<()> {
        let ix = axelar_solana_gateway::instructions::write_message_payload(
            gateway_root_pda,
            payer.pubkey(),
            command_id,
            payload,
            0,
        )
        .context("failed to construct an instruction to write to the message payload pda")?;
        send_tx(&[ix]).await?;
        Ok(())
    }

    async fn commit(
        payer: &Keypair,
        gateway_root_pda: Pubkey,
        command_id: [u8; 32],
    ) -> eyre::Result<()> {
        let ix = axelar_solana_gateway::instructions::commit_message_payload(
            gateway_root_pda,
            payer.pubkey(),
            command_id,
        )
        .context("failed to construct an instruction to commit the message payload pda")?;
        send_tx(&[ix]).await?;
        Ok(())
    }

    pub(super) async fn close(
        payer: &Keypair,
        gateway_root_pda: Pubkey,
        message: &Message,
    ) -> eyre::Result<()> {
        let msg_command_id = message_to_command_id(message);
        let ix = axelar_solana_gateway::instructions::close_message_payload(
            gateway_root_pda,
            payer.pubkey(),
            msg_command_id,
        )
        .context("failed to construct an instruction to close the message payload pda")?;
        send_tx(&[ix]).await?;
        Ok(())
    }

    /// Helper fn to produce a command id from a message.
    fn message_to_command_id(message: &Message) -> [u8; 32] {
        command_id(&message.cc_id.chain, &message.cc_id.id)
    }
}

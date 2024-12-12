use core::future::Future;
use core::pin::Pin;
use core::task::Poll;
use std::collections::VecDeque;
use std::sync::Arc;

use amplifier_api::types::TaskItem;
use axelar_executable::AxelarMessagePayload;
use axelar_solana_encoding::borsh::BorshDeserialize as _;
use axelar_solana_encoding::types::execute_data::{ExecuteData, MerkleisedPayload};
use axelar_solana_encoding::types::messages::{CrossChainId, Message};
use axelar_solana_gateway::error::GatewayError;
use axelar_solana_gateway::state::incoming_message::command_id;
use effective_tx_sender::ComputeBudgetError;
use eyre::Context as _;
use futures::stream::{FusedStream as _, FuturesOrdered, FuturesUnordered};
use futures::StreamExt as _;
use num_traits::FromPrimitive as _;
use relayer_amplifier_state::State;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_response::RpcSimulateTransactionResult;
use solana_sdk::instruction::{Instruction, InstructionError};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::TransactionError;
use tracing::{info_span, instrument, Instrument as _};

use crate::config;

/// A component that pushes transactions over to the Solana blockchain.
/// The transactions to push are dependant on the events that the Amplifier API will provide
pub struct SolanaTxPusher<S: State> {
    config: config::Config,
    name_on_amplifier: String,
    rpc_client: Arc<RpcClient>,
    task_receiver: relayer_amplifier_api_integration::AmplifierTaskReceiver,
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
        state: S,
    ) -> Self {
        Self {
            config,
            name_on_amplifier,
            rpc_client,
            task_receiver,
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
                        async move {
                            let command_id = task.id.clone();
                            let res =
                                process_task(&keypair, &solana_rpc_client, task, &config_metadata)
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
    task: TaskItem,
    metadata: &ConfigMetadata,
) -> eyre::Result<()> {
    use amplifier_api::types::Task::{Execute, GatewayTx, Refund, Verify};
    let signer = keypair.pubkey();
    let gateway_root_pda = metadata.gateway_root_pda;

    match task.task {
        Verify(_verify_task) => {
            tracing::warn!("solana blockchain is not supposed to receive the `verify_task`");
        }
        GatewayTx(task) => {
            gateway_tx_task(task, gateway_root_pda, signer, solana_rpc_client, keypair).await?;
        }
        Execute(task) => {
            // communicate with the destination program
            execute_task(task, metadata, signer, solana_rpc_client, keypair)
                .instrument(info_span!("execute task"))
                .in_current_span()
                .await?;
        }
        Refund(_refund_task) => {
            tracing::error!("refund task not implemented");
        }
    };

    Ok(())
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

    // For compatibility reasons with the rest of the Axelar protocol we need add custom handling
    // for ITS & Governance programs
    let destination_address = message.destination_address.parse::<Pubkey>()?;
    match destination_address {
        axelar_solana_its::ID => {
            let ix = its_instruction_builder::build_its_gmp_instruction(
                signer,
                gateway_incoming_message_pda,
                message,
                payload,
                solana_rpc_client,
            )
            .await?;

            send_tx(solana_rpc_client, keypair, ix).await?;
        }
        axelar_solana_governance::ID => {
            // todo Governance specific handling
            tracing::error!("governance program not yet supported");
        }
        _ => {
            // this is a security check, because the relayer is a signer, we don't want to
            // sign a tx where a malicious destination contract could drain the account. This is
            // because the `AxelarMessagePayload` defines an interface where the accounts get
            // dynamically appended, thus it could also include the relayers account.
            if let Ok(decoded_payload) = AxelarMessagePayload::decode(&payload) {
                let relayer_signer_acc_included = decoded_payload
                    .account_meta()
                    .iter()
                    .any(|acc| acc.pubkey == signer);
                if relayer_signer_acc_included {
                    eyre::bail!(
                        "relayer will not execute a transaction where its own key is included"
                    );
                }
            }

            // if security passed, we broadcast the tx
            let ix = axelar_executable::construct_axelar_executable_ix(
                message,
                &payload,
                gateway_incoming_message_pda,
            )?;
            send_tx(solana_rpc_client, keypair, ix).await?;
        }
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
    send_tx(solana_rpc_client, keypair, ix).await?;

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
            Some(send_tx(solana_rpc_client, keypair, ix))
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
            send_tx(solana_rpc_client, keypair, ix).await?;
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
                    Some(send_tx(solana_rpc_client, keypair, ix))
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
async fn send_tx(
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
        .map_err(eyre::Error::from)
        .map_err(ComputeBudgetError::Generic)
}

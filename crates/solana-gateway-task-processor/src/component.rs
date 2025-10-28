use core::future::Future;
use core::pin::Pin;
use core::str::FromStr as _;
use core::task::Poll;
use std::collections::VecDeque;
use std::sync::Arc;

use amplifier_api::chrono::{DateTime, Utc};
use amplifier_api::types::{
    BigInt, CannotExecuteMessageEventV2, CannotExecuteMessageEventV2Metadata,
    CannotExecuteMessageReason, Event, EventBase, EventId, EventMetadata, MessageExecutedEvent,
    MessageExecutedEventMetadata, MessageExecutionStatus, PublishEventsRequest, TaskItem,
    TaskItemId, Token, TxEvent,
};
use axelar_solana_encoding::borsh::BorshDeserialize as _;
use axelar_solana_encoding::types::execute_data::{ExecuteData, MerkleisedPayload};
use axelar_solana_encoding::types::messages::{CrossChainId, Message};
use axelar_solana_gateway::error::GatewayError;
use axelar_solana_gateway::executable::construct_axelar_executable_ix;
use axelar_solana_gateway::state::incoming_message::{command_id, IncomingMessage};
use axelar_solana_gateway::{get_verifier_set_tracker_pda, BytemuckedPda as _};
use effective_tx_sender::ComputeBudgetError;
use eyre::{eyre, Context as _, OptionExt as _};
use futures::stream::{FusedStream as _, FuturesOrdered, FuturesUnordered};
use futures::{SinkExt as _, StreamExt as _};
use num_traits::FromPrimitive as _;
use relayer_amplifier_api_integration::AmplifierCommand;
use relayer_amplifier_state::State;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_response::RpcSimulateTransactionResult;
use solana_listener::fetch_transaction;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::{Instruction, InstructionError};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::{Transaction, TransactionError};
use tracing::{info_span, instrument, Instrument as _};

pub use crate::component::gas_estimator::PriorityFeeGasEstimator;
use crate::component::gas_estimator::{GasEstimator, InsufficientGasBalance};
use crate::config;

mod gas_estimator;

/// A component that pushes transactions over to the Solana blockchain.
/// The transactions to push are dependant on the events that the Amplifier API will provide
pub struct SolanaTxPusher<S: State, G: GasEstimator> {
    config: Arc<config::Config>,
    name_on_amplifier: String,
    rpc_client: Arc<RpcClient>,
    task_receiver: relayer_amplifier_api_integration::AmplifierTaskReceiver,
    amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
    state: S,
    gas_estimator: Arc<G>,
}

impl<S: State, G: GasEstimator + 'static> relayer_engine::RelayerComponent
    for SolanaTxPusher<S, G>
{
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

impl<S: State, G: GasEstimator> SolanaTxPusher<S, G> {
    /// Create a new [`SolanaTxPusher`] component
    #[must_use]
    pub fn new(
        config: Arc<config::Config>,
        name_on_amplifier: String,
        rpc_client: Arc<RpcClient>,
        task_receiver: relayer_amplifier_api_integration::AmplifierTaskReceiver,
        amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
        state: S,
        gas_estimator: G,
    ) -> Self {
        Self {
            config,
            name_on_amplifier,
            rpc_client,
            task_receiver,
            amplifier_client,
            state,
            gas_estimator: Arc::new(gas_estimator),
        }
    }

    async fn process_internal(self) -> eyre::Result<()> {
        let config_metadata = Arc::new(self.get_config_metadata());
        let state = self.state.clone();
        let keypair = Arc::new(self.config.signing_keypair());

        ensure_gas_service_authority(&keypair.pubkey(), &self.rpc_client, &config_metadata).await?;

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
                        let config = Arc::clone(&self.config);
                        let gas_estimator = Arc::clone(&self.gas_estimator);
                        async move {
                            let command_id = task.id.clone();
                            let res = process_task(
                                &keypair,
                                &solana_rpc_client,
                                amplifier_client,
                                task,
                                &config_metadata,
                                config,
                                gas_estimator,
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
            gas_service_config_pda: self.config.gas_service_config_pda,
            gas_service_program_id: self.config.gas_service_program_address,
            commitment: self.config.commitment,
        }
    }
}

async fn ensure_gas_service_authority(
    key: &Pubkey,
    solana_rpc_client: &RpcClient,
    metadata: &ConfigMetadata,
) -> eyre::Result<()> {
    let account = solana_rpc_client
        .get_account(&metadata.gas_service_config_pda)
        .await?;
    if account.owner != metadata.gas_service_program_id {
        eyre::bail!(
            "gas service program id is not the owner of the provided gas service config PDA"
        )
    }
    let config = axelar_solana_gas_service::state::Config::read(&account.data)
        .ok_or_eyre("gas service config PDA account not initialized")?;

    if config.operator != *key {
        eyre::bail!("relayer is not the gas service operator")
    }

    Ok(())
}

struct ConfigMetadata {
    name_of_the_solana_chain: String,
    gateway_root_pda: Pubkey,
    gas_service_config_pda: Pubkey,
    commitment: CommitmentConfig,
    gas_service_program_id: Pubkey,
}

#[instrument(skip_all)]
async fn process_task<G: GasEstimator>(
    keypair: &Keypair,
    solana_rpc_client: &RpcClient,
    mut amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
    task_item: TaskItem,
    metadata: &ConfigMetadata,
    config: Arc<config::Config>,
    gas_estimator: Arc<G>,
) -> eyre::Result<()> {
    use amplifier_api::types::Task;
    let signer = keypair.pubkey();
    let gateway_root_pda = metadata.gateway_root_pda;

    match task_item.task {
        Task::GatewayTx(task) => {
            gateway_tx_task(task, gateway_root_pda, signer, solana_rpc_client, keypair).await?;
        }
        Task::Execute(task) => {
            let source_chain = task.message.source_chain.clone();
            let message_id = task.message.message_id.clone();

            // communicate with the destination program
            let Err(error) = execute_task(
                task,
                metadata,
                signer,
                solana_rpc_client,
                keypair,
                config,
                gas_estimator,
            )
            .instrument(info_span!("execute task"))
            .in_current_span()
            .await
            else {
                return Ok(());
            };

            tracing::error!(failed_to_execute = ?error, "failed to execute task");

            let event = if let Some(&ComputeBudgetError::TransactionError {
                source: ref _source,
                signature,
            }) = error.downcast_ref::<ComputeBudgetError>()
            {
                let tx =
                    fetch_transaction(metadata.commitment, signature, solana_rpc_client).await?;

                let tx = tx.tx();
                let total_fee = gateway_gas_computation::compute_total_gas(
                    axelar_solana_gateway::id(),
                    tx,
                    solana_rpc_client,
                    metadata.commitment,
                )
                .await
                .unwrap_or(tx.cost_in_lamports);

                message_executed_event(
                    &signature.to_string(),
                    source_chain,
                    message_id,
                    MessageExecutionStatus::Reverted,
                    tx.timestamp,
                    Token {
                        token_id: None,
                        amount: BigInt::from_u64(total_fee),
                    },
                )
            } else if let Some(InsufficientGasBalance { .. }) = error.downcast_ref() {
                cannot_execute_message_event(
                    task_item.id,
                    source_chain,
                    message_id,
                    CannotExecuteMessageReason::InsufficientGas,
                    error.to_string(),
                )
            } else {
                // Any other error, probably happening before execution: Simulation error,
                // error building an instruction, parsing pubkey, rpc transport error,
                // etc.
                //
                // The amplifier API is being updated to accept a list of
                // the transactions hashes with their aggregated costs, but for now we just use
                // meta.txID = null and pass the costs.
                //
                // In case no lamports were spent, no transaction could actually be executed in
                // the execute_task flow.

                cannot_execute_message_event(
                    task_item.id,
                    source_chain,
                    message_id,
                    CannotExecuteMessageReason::Error,
                    error.to_string(),
                )
            };

            let command = AmplifierCommand::PublishEvents(PublishEventsRequest {
                events: vec![event],
            });
            amplifier_client.sender.send(command).await?;
        }
        Task::Refund(task) => {
            refund_task(task, solana_rpc_client, keypair).await?;
        }
        Task::Verify(_verify_task) => {
            tracing::warn!("solana blockchain is not supposed to receive the `verify_task`");
        }
        Task::ConstructProof(_) => {
            // no op
        }
    };

    Ok(())
}

fn message_executed_event(
    id: &str,
    source_chain: String,
    message_id: TxEvent,
    status: MessageExecutionStatus,
    block_time: Option<DateTime<Utc>>,
    cost: Token,
) -> Event {
    let event_id = EventId::tx_reverted_event_id(id);
    let metadata = MessageExecutedEventMetadata::builder().build();
    let event_metadata = EventMetadata::builder()
        .timestamp(block_time)
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

async fn execute_task<G: GasEstimator>(
    execute_task: amplifier_api::types::ExecuteTask,
    metadata: &ConfigMetadata,
    signer: Pubkey,
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    config: Arc<config::Config>,
    gas_estimator: Arc<G>,
) -> Result<(), eyre::Error> {
    execute_task_with_estimator(
        execute_task,
        metadata,
        signer,
        solana_rpc_client,
        keypair,
        config,
        gas_estimator,
    )
    .await
}

async fn execute_task_with_estimator<G: GasEstimator>(
    execute_task: amplifier_api::types::ExecuteTask,
    metadata: &ConfigMetadata,
    signer: Pubkey,
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    config: Arc<config::Config>,
    gas_estimator: Arc<G>,
) -> Result<(), eyre::Error> {
    let payload = execute_task.payload;
    let available_gas_balance = execute_task.available_gas_balance.amount;

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

    if incoming_message_already_executed(solana_rpc_client, &gateway_incoming_message_pda).await? {
        tracing::warn!("incoming message already executed");
        return Ok(());
    }

    // Parse destination address
    let destination_address = message
        .destination_address
        .parse::<Pubkey>()
        .context("Failed to parse destination address")?;

    // Verify destination and communicate with the destination program
    verify_destination(destination_address, config.allow_third_party_contract_calls)?;

    let execute_ix = build_execute_instruction(
        signer,
        &message,
        &payload,
        destination_address,
        solana_rpc_client,
    )
    .await?;

    // Estimate gas cost and ensure enough gas balance
    let gas_result = gas_estimator
        .ensure_enough_gas(
            vec![execute_ix.clone()],
            keypair,
            available_gas_balance.0.try_into().map_err(|_err| {
                eyre::eyre!("available gas balance is too large to fit into u64")
            })?,
        )
        .await?;

    let mut all_ixs = Vec::with_capacity(3);

    all_ixs.extend(gas_result.priority_fee_ixs);
    all_ixs.push(execute_ix);

    let blockhash = solana_rpc_client
        .get_latest_blockhash()
        .await
        .map_err(|err| eyre::eyre!("Failed to get blockhash: {}", err))?;

    let tx = Transaction::new_signed_with_payer(
        &all_ixs,
        Some(&keypair.pubkey()),
        &[keypair],
        blockhash,
    );

    solana_rpc_client
        .send_and_confirm_transaction(&tx)
        .await
        .map_err(|err| eyre::eyre!("Failed to send and confirm transaction: {}", err))?;

    Ok(())
}

pub(crate) async fn build_execute_instruction(
    signer: Pubkey,
    message: &Message,
    payload: &[u8],
    destination_address: Pubkey,
    rpc_client: &RpcClient,
) -> eyre::Result<Instruction> {
    let (gateway_incoming_message_pda, _) = axelar_solana_gateway::get_incoming_message_pda(
        &command_id(&message.cc_id.chain, &message.cc_id.id),
    );

    match destination_address {
        axelar_solana_its::ID => Ok(its_instruction_builder::build_execute_instruction(
            signer,
            gateway_incoming_message_pda,
            message.clone(),
            payload.to_vec(),
            rpc_client,
        )
        .await
        .map_err(|err| eyre::eyre!("Failed to build ITS instruction: {:?}", err))?),
        axelar_solana_governance::ID => Ok(
            axelar_solana_governance::instructions::builder::calculate_gmp_ix(
                signer,
                gateway_incoming_message_pda,
                message,
                payload,
            )?,
        ),
        _ => Ok(construct_axelar_executable_ix(
            message.clone(),
            payload,
            gateway_incoming_message_pda,
        )?),
    }
}

fn verify_destination(
    destination_address: Pubkey,
    allow_third_party_contract_call: bool,
) -> eyre::Result<()> {
    if allow_third_party_contract_call {
        return Ok(());
    }

    let valid_destination_addresses = [
        axelar_solana_its::ID,
        axelar_solana_governance::ID,
        axelar_solana_gateway::ID,
        axelar_solana_gas_service::ID,
        axelar_solana_memo_program::ID,
    ];
    if !valid_destination_addresses.contains(&destination_address) {
        return Err(eyre!(
            "Destination address {:#?} is not valid",
            destination_address
        ));
    }

    Ok(())
}

/// Checks if the incoming message has already been executed.
async fn incoming_message_already_executed(
    solana_rpc_client: &RpcClient,
    incoming_message_pda: &Pubkey,
) -> eyre::Result<bool> {
    let raw_incoming_message = solana_rpc_client
        .get_account_data(incoming_message_pda)
        .await?;
    let incoming_message = IncomingMessage::read(&raw_incoming_message)
        .ok_or_eyre("failed to read incoming message")?;

    Ok(incoming_message.status.is_executed())
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
            &execute_data.payload_merkle_root,
            &execute_data.signing_verifier_set_merkle_root,
        );
    let ix = axelar_solana_gateway::instructions::initialize_payload_verification_session(
        signer,
        gateway_root_pda,
        execute_data.payload_merkle_root,
        execute_data.signing_verifier_set_merkle_root,
    )?;
    send_gateway_tx(solana_rpc_client, keypair, ix).await?;

    let verifier_set_tracker_pda =
        get_verifier_set_tracker_pda(execute_data.signing_verifier_set_merkle_root).0;

    // verify each signature in the signing session
    let mut verifier_ver_future_set = execute_data
        .signing_verifier_set_leaves
        .into_iter()
        .filter_map(|verifier_info| {
            let ix = axelar_solana_gateway::instructions::verify_signature(
                gateway_root_pda,
                verifier_set_tracker_pda,
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
                verifier_set_tracker_pda,
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
                    let ix = axelar_solana_gateway::instructions::approve_message(
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

async fn refund_task(
    task: amplifier_api::types::RefundTask,
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
) -> eyre::Result<()> {
    let receiver = Pubkey::from_str(&task.refund_recipient_address)?;

    if task.remaining_gas_balance.token_id.is_some() {
        eyre::bail!("non-native token refunds are not supported");
    } else {
        let instruction = axelar_solana_gas_service::instructions::refund_fees_instruction(
            &keypair.pubkey(),
            &receiver,
            task.message.message_id.0,
            task.remaining_gas_balance
                .amount
                .0
                .try_into()
                .map_err(|_err| eyre::eyre!("refund amount is too large"))?,
        )?;

        send_transaction(solana_rpc_client, keypair, VecDeque::from([instruction])).await?;
    }

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
    let res = send_transaction(solana_rpc_client, keypair, VecDeque::from([ix])).await;

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
    ix: VecDeque<Instruction>,
) -> Result<Signature, ComputeBudgetError> {
    effective_tx_sender::EffectiveTxSender::new(solana_rpc_client, keypair, ix)
        .evaluate_compute_ixs()
        .await?
        .send_tx()
        .await
}

#[cfg(test)]
#[expect(clippy::unimplemented, reason = "needed for the test")]
#[expect(clippy::indexing_slicing, reason = "simpler code")]
mod tests {
    use core::str::FromStr as _;
    use core::time::Duration;
    use std::path::PathBuf;
    use std::sync::Arc;

    use amplifier_api::types::{TaskItem, TaskItemId};
    use axelar_solana_gateway_test_fixtures::base::TestFixture;
    use axelar_solana_gateway_test_fixtures::gas_service::GasServiceUtils;
    use axelar_solana_gateway_test_fixtures::gateway::make_verifiers_with_quorum;
    use axelar_solana_gateway_test_fixtures::SolanaAxelarIntegrationMetadata;
    use axelar_solana_governance::state::GovernanceConfig;
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
    use relayer_amplifier_api_integration::{
        AmplifierCommand, AmplifierCommandClient, AmplifierTaskReceiver,
    };
    use relayer_amplifier_state::State;
    use solana_client::nonblocking::rpc_client::RpcClient;
    use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
    use solana_client::rpc_config::RpcTransactionConfig;
    use solana_rpc::rpc::JsonRpcConfig;
    use solana_rpc::rpc_pubsub_service::PubSubConfig;
    use solana_sdk::account::AccountSharedData;
    use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};
    use solana_sdk::keccak::Hash;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::{Keypair, Signature};
    use solana_sdk::signer::Signer as _;
    use solana_sdk::{bpf_loader_upgradeable, keccak, system_program};
    use solana_test_validator::UpgradeableProgramInfo;
    use solana_transaction_status::option_serializer::OptionSerializer;
    use solana_transaction_status::{
        EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding,
    };
    use tokio::task::JoinHandle;

    use super::gas_estimator::MockGasEstimator;
    use super::SolanaTxPusher;
    use crate::component::gas_estimator::GasEstimatorResult;
    use crate::config;

    mod unit_tests {
        use core::str::FromStr as _;

        use solana_sdk::pubkey::Pubkey;

        use crate::component::verify_destination;

        #[test]
        fn test_verify_destination() {
            // Whitelisted addresses are still reachable when allow_third_party_contract_calls is
            // true
            verify_destination(axelar_solana_memo_program::ID, true).unwrap();
            // Check that the whitelisted addresses are reachable when
            // allow_third_party_contract_calls is false
            verify_destination(axelar_solana_its::ID, false).unwrap();
            verify_destination(axelar_solana_governance::ID, false).unwrap();
            verify_destination(axelar_solana_gas_service::ID, false).unwrap();
            verify_destination(axelar_solana_memo_program::ID, false).unwrap();
            verify_destination(axelar_solana_gateway::ID, false).unwrap();
            // Check that the non-whitelisted addresses (third party programs) are not reachable
            // when allow_third_party_contract_calls is false
            assert!(verify_destination(
                Pubkey::from_str("its2RSrgfKfQDkuxFhov4nPRw4Wy9i6e757befoobar").unwrap(),
                false
            )
            .is_err());
            verify_destination(
                Pubkey::from_str("its2RSrgfKfQDkuxFhov4nPRw4Wy9i6e757befoobar").unwrap(),
                true,
            )
            .unwrap();
        }
    }

    mod integration_tests {

        use amplifier_api::types::{ExecuteTask, GatewayV2Message, MessageId, Token};
        use axelar_solana_encoding::types::messages::{CrossChainId, Message};
        use pretty_assertions::assert_eq;
        use solana_sdk::signature::Signature;

        use super::*;
        use crate::component::gas_estimator::{GasEstimatorResult, MockGasEstimator};
        use crate::component::tests::{setup, setup_aux_contracts};
        use crate::component::{config, execute_task_with_estimator, ConfigMetadata};

        #[test_log::test(tokio::test)]
        async fn test_allow_third_party_contract_calls_config() {
            let mut fixture = setup().await;
            let (
                gas_config,
                _gas_init_sig,
                _counter_pda,
                _init_memo_sig,
                _init_its_sig,
                _gov_config_pda,
            ) = setup_aux_contracts(&mut fixture).await;

            let (_, _, _, rpc_client) = setup_tx_pusher(&fixture, &gas_config);

            let destination_address = Pubkey::new_unique();

            let payload = vec![1, 2, 3];
            let payload_hash = keccak::hashv(&[&payload]).to_bytes();

            let message_v2 = GatewayV2Message::builder()
                .message_id(MessageId::new(&Signature::new_unique().to_string(), 1))
                .destination_address(destination_address.to_string())
                .source_chain("solana".to_owned())
                .source_address(fixture.payer.pubkey().to_string())
                .payload_hash(payload_hash.to_vec())
                .build();

            let message = Message {
                cc_id: CrossChainId {
                    chain: message_v2.source_chain.clone(),
                    id: message_v2.message_id.0.clone(),
                },
                source_address: message_v2.source_address.clone(),
                destination_chain: "solana".to_owned(),
                destination_address: message_v2.destination_address.clone(),
                payload_hash: message_v2
                    .payload_hash
                    .clone()
                    .as_slice()
                    .try_into()
                    .unwrap(),
            };

            fixture
                .sign_session_and_approve_messages(&fixture.signers.clone(), &[message])
                .await
                .unwrap();

            let task = ExecuteTask {
                message: message_v2,
                payload: vec![1, 2, 3],
                available_gas_balance: Token::builder()
                    .amount(amplifier_api::types::BigInt(100_i32.into()))
                    .build(),
            };

            let metadata = ConfigMetadata {
                name_of_the_solana_chain: "solana".to_owned(),
                gateway_root_pda: axelar_solana_gateway::get_gateway_root_config_pda().0,
                gas_service_config_pda: gas_config.config_pda,
                commitment: CommitmentConfig::confirmed(),
                gas_service_program_id: axelar_solana_gas_service::id(),
            };

            let tx_pusher_config = Arc::new(config::Config {
                gateway_program_address: axelar_solana_gateway::id(),
                gas_service_program_address: axelar_solana_gas_service::id(),
                gas_service_config_pda: gas_config.config_pda,
                signing_keypair: fixture.payer.insecure_clone().to_base58_string(),
                commitment: CommitmentConfig::confirmed(),
                allow_third_party_contract_calls: false, /* Disallow third party contract
                                                          * calls */
                estimation_node_rpc_url: "http://127.0.0.1:8899".to_owned(),
            });

            // Create a mock gas estimator that returns a low cost
            let mut mock_estimator = MockGasEstimator::new();

            mock_estimator
                .expect_ensure_enough_gas()
                .returning(|_, _, _| {
                    Ok(GasEstimatorResult {
                        required_gas: 50,
                        available_gas: 100,
                        priority_fee_ixs: vec![],
                    })
                });

            let result = execute_task_with_estimator(
                task,
                &metadata,
                fixture.payer.pubkey(),
                &rpc_client,
                &fixture.payer.insecure_clone(),
                tx_pusher_config,
                Arc::new(mock_estimator),
            )
            .await;

            match result {
                Ok(()) => panic!("Expected an error, but got Ok"),
                Err(err) => {
                    let expected_log =
                        format!("Destination address {destination_address} is not valid");
                    assert_eq!(err.to_string(), expected_log);
                }
            }
        }
    }

    mod its_tests {

        use amplifier_api::chrono::DateTime;
        use amplifier_api::types::uuid::Uuid;
        use amplifier_api::types::{
            Event, ExecuteTask, GatewayV2Message, MessageExecutedEvent, MessageExecutionStatus,
            MessageId, PublishEventsRequest, Task, TaskItem, TaskItemId, Token,
        };
        use axelar_solana_encoding::borsh::{self, BorshDeserialize as _};
        use axelar_solana_encoding::types::messages::{CrossChainId, Message};
        use axelar_solana_gateway::executable::{
            AxelarMessagePayload, EncodingScheme, SolanaAccountRepr,
        };
        use axelar_solana_gateway_test_fixtures::SolanaAxelarIntegrationMetadata;
        use axelar_solana_memo_program::instruction::AxelarMemoInstruction;
        use futures::channel::mpsc::UnboundedSender;
        use futures::{SinkExt as _, StreamExt as _};
        use interchain_token_transfer_gmp::{
            DeployInterchainToken, GMPPayload, InterchainTransfer, ReceiveFromHub,
        };
        use pretty_assertions::assert_eq;
        use relayer_amplifier_api_integration::AmplifierCommand;
        use solana_sdk::keccak;
        use solana_sdk::signature::Signature;

        use super::*;
        use crate::component::tests::{
            fetch_latest_tx_logs, setup, setup_aux_contracts, setup_tx_pusher,
        };

        const ITS_HUB_CHAIN_NAME: &str = "axelar";
        const ITS_HUB_SOURCE_ADDRESS: &str =
            "axelar157hl7gpuknjmhtac2qnphuazv2yerfagva7lsu9vuj2pgn32z22qa26dk4";
        const TEST_TOKEN_NAME: &str = "MyToken";
        const TEST_TOKEN_SYMBOL: &str = "MTK";

        #[test_log::test(tokio::test)]
        async fn process_successful_token_deployment() {
            let mut fixture = setup().await;
            let (gas_config, _gas_init_sig, _counter_pda, _init_memo_sig, _init_its_sig, _) =
                setup_aux_contracts(&mut fixture).await;
            let (pusher_task, mut task_sender, mut rx_amplifier, rpc_client) =
                setup_tx_pusher(&fixture, &gas_config);
            let token_id = axelar_solana_its::interchain_token_id(
                &fixture.payer.pubkey(),
                &Pubkey::new_unique().to_bytes(),
            );
            let deploy_interchain_token_message =
                GMPPayload::DeployInterchainToken(DeployInterchainToken {
                    selector: 1_u32.try_into().unwrap(),
                    token_id: token_id.into(),
                    name: TEST_TOKEN_NAME.to_owned(),
                    symbol: TEST_TOKEN_SYMBOL.to_owned(),
                    decimals: 9,
                    minter: fixture.payer.pubkey().to_bytes().into(),
                });

            send_its_message(
                deploy_interchain_token_message,
                ITS_HUB_CHAIN_NAME.to_owned(),
                ITS_HUB_SOURCE_ADDRESS.to_owned(),
                &mut fixture,
                &mut task_sender,
            )
            .await;

            task_sender.close_channel();
            let _result = pusher_task.await;

            assert_eq!(rx_amplifier.next().await, None);

            let (its_root_pda, _) = axelar_solana_its::find_its_root_pda();
            let (mint_address, _) =
                axelar_solana_its::find_interchain_token_pda(&its_root_pda, &token_id);

            let mint_account_raw_data = rpc_client.get_account_data(&mint_address).await.unwrap();
            assert!(!mint_account_raw_data.is_empty());

            let (token_manager_address, _) =
                axelar_solana_its::find_token_manager_pda(&its_root_pda, &token_id);

            let token_manager_raw_data = rpc_client
                .get_account_data(&token_manager_address)
                .await
                .unwrap();
            let token_manager = axelar_solana_its::state::token_manager::TokenManager::deserialize(
                &mut token_manager_raw_data.as_ref(),
            )
            .unwrap();

            assert_eq!(token_manager.token_id, token_id);
            assert_eq!(
                token_manager.ty,
                axelar_solana_its::state::token_manager::Type::NativeInterchainToken
            );
        }

        #[test_log::test(tokio::test)]
        async fn process_failed_token_deployment_untrusted_source_address() {
            let mut fixture = setup().await;
            let (gas_config, _gas_init_sig, _counter_pda, _init_memo_sig, _init_its_sig, _) =
                setup_aux_contracts(&mut fixture).await;
            let (pusher_task, mut task_sender, mut rx_amplifier, _) =
                setup_tx_pusher(&fixture, &gas_config);
            let token_id = axelar_solana_its::interchain_token_id(
                &fixture.payer.pubkey(),
                &Pubkey::new_unique().to_bytes(),
            );
            let deploy_interchain_token_message =
                GMPPayload::DeployInterchainToken(DeployInterchainToken {
                    selector: 1_u32.try_into().unwrap(),
                    token_id: token_id.into(),
                    name: TEST_TOKEN_NAME.to_owned(),
                    symbol: TEST_TOKEN_SYMBOL.to_owned(),
                    decimals: 9,
                    minter: fixture.payer.pubkey().to_bytes().into(),
                });

            send_its_message(
                deploy_interchain_token_message,
                ITS_HUB_CHAIN_NAME.to_owned(),
                "invalid address".to_owned(),
                &mut fixture,
                &mut task_sender,
            )
            .await;

            task_sender.close_channel();
            let _result = pusher_task.await;

            let amplifier_command = rx_amplifier
                .next()
                .await
                .expect("should have received amplifier command");
            let AmplifierCommand::PublishEvents(PublishEventsRequest { mut events }) =
                amplifier_command;
            let Some(Event::MessageExecuted(MessageExecutedEvent { status, .. })) = events.pop()
            else {
                panic!("could not find expected event");
            };

            assert_eq!(status, MessageExecutionStatus::Reverted);
        }

        #[test_log::test(tokio::test)]
        #[expect(clippy::non_ascii_literal, reason = "it's cool")]
        async fn process_successful_transfer_with_executable() {
            let mut fixture = setup().await;
            let (gas_config, _gas_init_sig, counter_pda, _init_memo_sig, _init_its_sig, _) =
                setup_aux_contracts(&mut fixture).await;
            let (pusher_task, mut task_sender, mut rx_amplifier, rpc_client) =
                setup_tx_pusher(&fixture, &gas_config);
            let token_id = axelar_solana_its::interchain_token_id(
                &fixture.payer.pubkey(),
                &Pubkey::new_unique().to_bytes(),
            );
            let deploy_interchain_token_message =
                GMPPayload::DeployInterchainToken(DeployInterchainToken {
                    selector: 1_u32.try_into().unwrap(),
                    token_id: token_id.into(),
                    name: TEST_TOKEN_NAME.to_owned(),
                    symbol: TEST_TOKEN_SYMBOL.to_owned(),
                    decimals: 9,
                    minter: fixture.payer.pubkey().to_bytes().into(),
                });

            send_its_message(
                deploy_interchain_token_message,
                ITS_HUB_CHAIN_NAME.to_owned(),
                ITS_HUB_SOURCE_ADDRESS.to_owned(),
                &mut fixture,
                &mut task_sender,
            )
            .await;

            let memo_instruction = AxelarMemoInstruction::ProcessMemo {
                memo: "🦖".to_owned(),
            };

            let (its_root_pda, _) = axelar_solana_its::find_its_root_pda();
            let (mint, _) = axelar_solana_its::find_interchain_token_pda(&its_root_pda, &token_id);
            let (token_metadata_account, _) =
                mpl_token_metadata::accounts::Metadata::find_pda(&mint);

            let data = AxelarMessagePayload::new(
                &borsh::to_vec(&memo_instruction).unwrap(),
                &[
                    SolanaAccountRepr {
                        pubkey: token_metadata_account.to_bytes().into(),
                        is_signer: false,
                        is_writable: false,
                    },
                    SolanaAccountRepr {
                        pubkey: counter_pda.0.to_bytes().into(),
                        is_signer: false,
                        is_writable: true,
                    },
                ],
                EncodingScheme::AbiEncoding,
            )
            .encode()
            .unwrap()
            .into();

            let interchain_transfer_message = GMPPayload::InterchainTransfer(InterchainTransfer {
                selector: 0_u32.try_into().unwrap(),
                token_id: token_id.into(),
                source_address: b"source wallet address".into(),
                destination_address: axelar_solana_memo_program::id().to_bytes().into(),
                amount: 5120.0_f64.try_into().unwrap(),
                data,
            });

            send_its_message(
                interchain_transfer_message,
                ITS_HUB_CHAIN_NAME.to_owned(),
                ITS_HUB_SOURCE_ADDRESS.to_owned(),
                &mut fixture,
                &mut task_sender,
            )
            .await;

            task_sender.close_channel();
            let _result = pusher_task.await;

            let logs = fetch_latest_tx_logs(&axelar_solana_memo_program::id(), &rpc_client).await;

            logs.iter()
                .find(|log| log.contains("🦖"))
                .map(std::string::String::as_str)
                .expect("could not find expected log emitted by the memo program");

            assert_eq!(rx_amplifier.next().await, None);

            let (its_root_pda, _) = axelar_solana_its::find_its_root_pda();
            let (mint_address, _) =
                axelar_solana_its::find_interchain_token_pda(&its_root_pda, &token_id);

            let mint_account_raw_data = rpc_client.get_account_data(&mint_address).await.unwrap();
            assert!(!mint_account_raw_data.is_empty());

            let (token_manager_address, _) =
                axelar_solana_its::find_token_manager_pda(&its_root_pda, &token_id);

            let token_manager_raw_data = rpc_client
                .get_account_data(&token_manager_address)
                .await
                .unwrap();

            let token_manager = axelar_solana_its::state::token_manager::TokenManager::deserialize(
                &mut token_manager_raw_data.as_ref(),
            )
            .unwrap();

            assert_eq!(token_manager.token_id, token_id);
            assert_eq!(
                token_manager.ty,
                axelar_solana_its::state::token_manager::Type::NativeInterchainToken
            );
        }

        async fn send_its_message(
            its_message: GMPPayload,
            source_chain: String,
            source_address: String,
            fixture: &mut SolanaAxelarIntegrationMetadata,
            task_sender: &mut UnboundedSender<TaskItem>,
        ) {
            let hub_payload = GMPPayload::ReceiveFromHub(ReceiveFromHub {
                selector: 4_i32.try_into().unwrap(),
                source_chain: "axelar".to_owned(),
                payload: its_message.encode().into(),
            })
            .encode();

            let fake_hash = Signature::new_unique();
            let message_id = MessageId::new(&fake_hash.to_string(), 1);
            let payload_hash = keccak::hash(&hub_payload).to_bytes();
            let message = Message {
                cc_id: CrossChainId {
                    chain: source_chain.clone(),
                    id: message_id.0.clone(),
                },
                source_address: source_address.clone(),
                destination_chain: "solana".to_owned(),
                destination_address: axelar_solana_its::id().to_string(),
                payload_hash,
            };

            fixture
                .sign_session_and_approve_messages(&fixture.signers.clone(), &[message])
                .await
                .unwrap();

            let message = GatewayV2Message::builder()
                .message_id(message_id)
                .destination_address(axelar_solana_its::id().to_string())
                .source_chain(source_chain.clone())
                .source_address(source_address.clone())
                .payload_hash(payload_hash.to_vec())
                .build();

            let task_item = TaskItem::builder()
                .id(TaskItemId(Uuid::new_v4()))
                .task(Task::Execute(
                    ExecuteTask::builder()
                        .message(message)
                        .payload(hub_payload)
                        .available_gas_balance(
                            Token::builder()
                                .amount(amplifier_api::types::BigInt(100_i32.into()))
                                .build(),
                        )
                        .build(),
                ))
                .timestamp(DateTime::default())
                .build();

            task_sender.send(task_item).await.unwrap();
        }
    }

    mod governance_tests {

        use amplifier_api::chrono::DateTime;
        use amplifier_api::types::{ExecuteTask, GatewayV2Message, MessageId, Task, Token};
        use axelar_solana_encoding::types::messages::{CrossChainId, Message};
        use axelar_solana_governance::instructions::builder::IxBuilder;
        use futures::{SinkExt as _, StreamExt as _};
        use solana_sdk::instruction::AccountMeta;
        use uuid::Uuid;

        use super::*;

        pub(super) const CHAIN_NAME_KECCAK_BASE58_HASH: &str =
            "3Hv3NpPp221k5vqEWEJ3n1NHQemHPiLuUWf7mQdBWWwQ";
        pub(super) const AXELAR_GOV_ADDRESS_KECCAK_BASE58_HASH: &str =
            "2BxSFpGc1shPid1odjZL4UPPssRNx1htoF8fCBXqrgDm";
        pub(super) const MINIMUM_PROPOSAL_ETA_DELAY: u32 = 3600;
        pub(super) const OPERATOR_PUBKEY: &str = "BunZKHSeKhdbCCAzA2yQiA92Z4VJ6DukoKRYd8y97cKq";

        #[test_log::test(tokio::test)]
        async fn test_relayer_can_communicate_with_governance_governance_via_gmp() {
            let mut fixture = setup().await;
            let (
                gas_config,
                _gas_init_sig,
                _counter_pda,
                _init_memo_sig,
                _init_its_sig,
                gov_config_pda,
            ) = setup_aux_contracts(&mut fixture).await;
            let (pusher_task, mut task_sender, mut rx_amplifier, rpc_client) =
                setup_tx_pusher(&fixture, &gas_config);

            let ix_builder = ix_builder_with_sample_proposal_data();

            let message_id = MessageId::new(&Signature::new_unique().to_string(), 1);

            let gmp_call_data = ix_builder
                .gmp_ix()
                .with_msg_metadata(gmp_sample_metadata(&message_id))
                .schedule_time_lock_proposal(&fixture.payer.pubkey(), &gov_config_pda)
                .build();

            fixture
                .sign_session_and_approve_messages(
                    &fixture.signers.clone(),
                    &[gmp_call_data.msg_meta.clone()],
                )
                .await
                .unwrap();

            let gmp_message_meta = gmp_call_data.msg_meta;

            let message = GatewayV2Message::builder()
                .message_id(message_id)
                .destination_address(axelar_solana_governance::id().to_string())
                .source_chain(gmp_message_meta.cc_id.chain)
                .source_address(gmp_message_meta.source_address)
                .payload_hash(gmp_message_meta.payload_hash.to_vec())
                .build();

            let task_item = TaskItem::builder()
                .id(TaskItemId(Uuid::new_v4()))
                .task(Task::Execute(
                    ExecuteTask::builder()
                        .message(message)
                        .payload(gmp_call_data.msg_payload)
                        .available_gas_balance(
                            Token::builder()
                                .amount(amplifier_api::types::BigInt(100_i32.into()))
                                .build(),
                        )
                        .build(),
                ))
                .timestamp(DateTime::default())
                .build();

            task_sender.send(task_item).await.unwrap();
            task_sender.close_channel();
            let _result = pusher_task.await;
            assert_eq!(rx_amplifier.next().await, None);

            let logs = fetch_latest_tx_logs(&axelar_solana_governance::id(), &rpc_client).await;

            logs.iter()
                .find(|log| log.contains("Instruction: Validate Message"))
                .map(std::string::String::as_str)
                .expect("governance should call Validate Message at gateway for validating the gmp payload. This demonstrates we can communicate with governance via GMP");
        }

        fn gmp_sample_metadata(message_id: &MessageId) -> Message {
            Message {
                cc_id: CrossChainId {
                    chain: "axelar".to_owned(),
                    id: message_id.0.clone(), //uuid::Uuid::new_v4().to_string(),
                },
                source_address: "axelar1ure22quyrl8wdyxz4jdx285hp4dwufwt0g0akl".to_owned(),
                destination_address: axelar_solana_governance::ID.to_string(),
                destination_chain: "solana".to_owned(),
                payload_hash: [0_u8; 32], // This gets overwritten later by the builder
            }
        }

        fn ix_builder_with_sample_proposal_data(
        ) -> IxBuilder<axelar_solana_governance::instructions::builder::ProposalRelated> {
            IxBuilder::new().with_proposal_data(
                Pubkey::from_str("BunZKHSeKhdbCCAzA2yQiA92Z4VJ6DukoKRYd8y97cKq").unwrap(),
                1,
                3600,
                Some(AccountMeta::new_readonly(
                    Pubkey::new_from_array([0_u8; 32]),
                    false,
                )),
                &[AccountMeta::new_readonly(
                    Pubkey::new_from_array([0_u8; 32]),
                    false,
                )],
                vec![0],
            )
        }
    }
    #[derive(Clone)]
    struct MockState;
    impl State for MockState {
        type Err = std::io::Error;

        fn latest_processed_task_id(&self) -> Option<TaskItemId> {
            None
        }

        fn latest_queried_task_id(&self) -> Option<TaskItemId> {
            None
        }

        fn set_latest_processed_task_id(&self, _task_item_id: TaskItemId) -> Result<(), Self::Err> {
            Ok(())
        }

        fn set_latest_queried_task_id(&self, _task_item_id: TaskItemId) -> Result<(), Self::Err> {
            Ok(())
        }
    }

    async fn fetch_latest_tx_logs(program: &Pubkey, rpc_client: &RpcClient) -> Vec<String> {
        let tx_signature = rpc_client
            .get_signatures_for_address_with_config(
                program,
                GetConfirmedSignaturesForAddress2Config {
                    limit: Some(1),
                    commitment: Some(CommitmentConfig {
                        commitment: CommitmentLevel::Confirmed,
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("failed to fetch transactions")
            .pop()
            .expect("no transaction found for given program")
            .signature;
        let signature =
            Signature::from_str(&tx_signature).expect("invalid signature returned from rpc");

        let EncodedConfirmedTransactionWithStatusMeta {
            transaction: transaction_with_meta,
            ..
        } = rpc_client
            .get_transaction_with_config(
                &signature,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Binary),
                    commitment: Some(CommitmentConfig {
                        commitment: CommitmentLevel::Confirmed,
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("could not get transaction");

        let meta = transaction_with_meta
            .meta
            .expect("transaction is missing metadata");

        let OptionSerializer::Some(logs) = meta.log_messages else {
            panic!("transaction contains no logs");
        };

        logs
    }

    fn setup_tx_pusher(
        fixture: &SolanaAxelarIntegrationMetadata,
        gas_config: &GasServiceUtils,
    ) -> (
        JoinHandle<eyre::Result<()>>,
        UnboundedSender<TaskItem>,
        UnboundedReceiver<AmplifierCommand>,
        Arc<RpcClient>,
    ) {
        let config = config::Config {
            gateway_program_address: axelar_solana_gateway::id(),
            gas_service_program_address: axelar_solana_gas_service::id(),
            gas_service_config_pda: gas_config.config_pda,
            signing_keypair: fixture.payer.insecure_clone().to_base58_string(),
            commitment: CommitmentConfig::confirmed(),
            allow_third_party_contract_calls: true,
            estimation_node_rpc_url: "http://127.0.0.1:8899".to_owned(),
        };
        setup_tx_pusher_with_config(fixture, config)
    }

    fn setup_tx_pusher_with_config(
        fixture: &SolanaAxelarIntegrationMetadata,
        config: config::Config,
    ) -> (
        JoinHandle<eyre::Result<()>>,
        UnboundedSender<TaskItem>,
        UnboundedReceiver<AmplifierCommand>,
        Arc<RpcClient>,
    ) {
        let (tx_amplifier, rx_amplifier) = futures::channel::mpsc::unbounded();
        let (task_sender, task_receiver) = futures::channel::mpsc::unbounded();
        let amplifier_client = AmplifierCommandClient {
            sender: tx_amplifier,
        };
        let amplifier_task_receiver = AmplifierTaskReceiver {
            receiver: task_receiver,
        };

        let rpc_client_url = match fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                ref validator,
                ..
            } => validator.rpc_url(),
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::ProgramTest { .. } => {
                unimplemented!()
            }
        };
        let rpc_client =
            retrying_solana_http_sender::new_client(&retrying_solana_http_sender::Config {
                max_concurrent_rpc_requests: 10,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });

        // Create a mock gas estimator for tests
        let mut mock_estimator = MockGasEstimator::new();
        mock_estimator
            .expect_ensure_enough_gas()
            .returning(|_, _, _| {
                Ok(GasEstimatorResult {
                    required_gas: 100_000,
                    available_gas: 1_000_000,
                    priority_fee_ixs: vec![],
                })
            });

        let solana_tx_pusher = SolanaTxPusher::new(
            Arc::new(config),
            "solana".to_owned(),
            Arc::clone(&rpc_client),
            amplifier_task_receiver,
            amplifier_client,
            MockState,
            mock_estimator,
        );
        let task = tokio::task::spawn(solana_tx_pusher.process_internal());

        (task, task_sender, rx_amplifier, rpc_client)
    }

    pub(crate) async fn setup_aux_contracts(
        fixture: &mut SolanaAxelarIntegrationMetadata,
    ) -> (
        axelar_solana_gateway_test_fixtures::gas_service::GasServiceUtils,
        Signature,
        (Pubkey, u8),
        Signature,
        Signature,
        Pubkey,
    ) {
        let (config_pda, ..) = axelar_solana_gas_service::get_config_pda();

        let gas_config = GasServiceUtils {
            upgrade_authority: fixture.payer.insecure_clone(),
            operator: fixture.payer.insecure_clone(),
            config_pda,
        };

        let ix = axelar_solana_gas_service::instructions::init_config(
            &fixture.payer.pubkey(),
            &gas_config.operator.pubkey(),
        )
        .unwrap();
        let gas_init_sig = *fixture
            .send_tx_with_signatures(&[ix])
            .await
            .unwrap()
            .0
            .first()
            .unwrap();

        // init memo program
        let counter_pda = axelar_solana_memo_program::get_counter_pda();
        let ix = axelar_solana_memo_program::instruction::initialize(
            &fixture.payer.pubkey(),
            &counter_pda,
        )
        .unwrap();
        let init_memo_sig = fixture.send_tx_with_signatures(&[ix]).await.unwrap().0[0];

        let ix = axelar_solana_its::instruction::initialize(
            fixture.upgrade_authority.pubkey(),
            fixture.payer.pubkey(),
            "solana".to_owned(),
            "axelar157hl7gpuknjmhtac2qnphuazv2yerfagva7lsu9vuj2pgn32z22qa26dk4".to_owned(),
        )
        .unwrap();

        let set_trusted_chain_ix = axelar_solana_its::instruction::set_trusted_chain(
            fixture.payer.pubkey(),
            fixture.upgrade_authority.pubkey(),
            "axelar".to_owned(),
        )
        .unwrap();
        let upgrade_authority = fixture.upgrade_authority.insecure_clone();
        let payer = fixture.payer.insecure_clone();
        let init_its_sig = fixture
            .send_tx_with_custom(
                &payer.pubkey(),
                &[ix, set_trusted_chain_ix],
                &[upgrade_authority.insecure_clone(), payer.insecure_clone()],
            )
            .await
            .unwrap()
            .0[0];

        // init governance program
        let ix_builder = axelar_solana_governance::instructions::builder::IxBuilder::new();

        let gov_config_pda = GovernanceConfig::pda().0;
        let gov_config = GovernanceConfig::new(
            Hash::from_str(governance_tests::CHAIN_NAME_KECCAK_BASE58_HASH)
                .unwrap()
                .to_bytes(),
            Hash::from_str(governance_tests::AXELAR_GOV_ADDRESS_KECCAK_BASE58_HASH)
                .unwrap()
                .to_bytes(),
            governance_tests::MINIMUM_PROPOSAL_ETA_DELAY,
            Pubkey::from_str(governance_tests::OPERATOR_PUBKEY)
                .unwrap()
                .to_bytes(),
        );

        let ix = ix_builder
            .initialize_config(
                &fixture.upgrade_authority.pubkey(),
                &gov_config_pda,
                gov_config,
            )
            .build();

        assert!(!fixture
            .send_tx_with_custom(
                &payer.pubkey(),
                &[ix],
                &[upgrade_authority.insecure_clone(), payer.insecure_clone()],
            )
            .await
            .unwrap()
            .0
            .is_empty());
        (
            gas_config,
            gas_init_sig,
            counter_pda,
            init_memo_sig,
            init_its_sig,
            gov_config_pda,
        )
    }

    /// Return the [`PathBuf`] that points to the `[repo]` folder
    #[must_use]
    pub(crate) fn workspace_root_dir() -> PathBuf {
        let dir = std::env::var("CARGO_MANIFEST_DIR")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
        PathBuf::from(dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned()
    }

    pub(crate) async fn setup() -> SolanaAxelarIntegrationMetadata {
        use solana_test_validator::TestValidatorGenesis;
        let mut validator = TestValidatorGenesis::default();

        let mut rpc_config = JsonRpcConfig::default_for_test();
        rpc_config.enable_rpc_transaction_history = true;
        rpc_config.enable_extended_tx_metadata_storage = true;
        validator.rpc_config(rpc_config);

        let mut pubsub_config = PubSubConfig::default_for_tests();
        pubsub_config.enable_block_subscription = true;
        validator.pubsub_config(pubsub_config);

        let upgrade_authority = Keypair::new();
        validator.add_account(
            upgrade_authority.pubkey(),
            AccountSharedData::new(u64::MAX / 2, 0, &system_program::ID),
        );
        validator.add_upgradeable_programs_with_path(&[
            UpgradeableProgramInfo {
                program_id: axelar_solana_gateway::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_gateway.so"),
            },
            UpgradeableProgramInfo {
                program_id: axelar_solana_gas_service::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_gas_service.so"),
            },
            UpgradeableProgramInfo {
                program_id: axelar_solana_governance::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_governance.so"),
            },
            UpgradeableProgramInfo {
                program_id: axelar_solana_memo_program::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_memo_program.so"),
            },
            UpgradeableProgramInfo {
                program_id: axelar_solana_its::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_its.so"),
            },
            UpgradeableProgramInfo {
                program_id: Pubkey::from_str("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s")
                    .unwrap(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("mpl_token_metadata.so"),
            },
        ]);

        let forced_sleep = if std::env::var("CI").is_ok() {
            Duration::from_millis(1500)
        } else {
            Duration::from_millis(500)
        };
        let mut fixture = TestFixture::new_test_validator(validator, forced_sleep).await;
        let init_payer = fixture.payer.insecure_clone();
        fixture.payer = upgrade_authority.insecure_clone();

        let operator = Keypair::new();
        let domain_separator = [42; 32];
        let initial_signers = make_verifiers_with_quorum(&[42], 0, 42, domain_separator);
        let mut fixture = SolanaAxelarIntegrationMetadata {
            domain_separator,
            upgrade_authority,
            fixture,
            signers: initial_signers,
            gateway_root_pda: axelar_solana_gateway::get_gateway_root_config_pda().0,
            operator,
            previous_signers_retention: 16,
            minimum_rotate_signers_delay_seconds: 1,
        };

        fixture.initialize_gateway_config_account().await.unwrap();
        fixture.payer = init_payer;
        fixture
    }

    mod tx_size_tests {

        use axelar_solana_encoding::types::messages::{CrossChainId, Message};
        use axelar_solana_gateway::state::incoming_message::command_id;
        use axelar_solana_its::state::token_manager::Type;
        use interchain_token_transfer_gmp::alloy_primitives::hex::FromHex;
        use interchain_token_transfer_gmp::alloy_primitives::{Bytes, FixedBytes, U256};
        use interchain_token_transfer_gmp::{
            DeployInterchainToken, GMPPayload, InterchainTransfer, LinkToken, ReceiveFromHub,
        };
        use its_instruction_builder::build_execute_instruction;
        use solana_sdk::address_lookup_table::instruction::{
            create_lookup_table, extend_lookup_table,
        };
        use solana_sdk::address_lookup_table::state::AddressLookupTable;
        use solana_sdk::address_lookup_table::AddressLookupTableAccount;
        use solana_sdk::compute_budget::ComputeBudgetInstruction;
        use solana_sdk::message::{v0, VersionedMessage};
        use solana_sdk::transaction::{Transaction, VersionedTransaction};

        use super::*;
        use crate::component::tests::setup;

        fn setup_test_rpc_client(fixture: &SolanaAxelarIntegrationMetadata) -> Arc<RpcClient> {
            let rpc_client_url = match fixture.fixture.test_node {
                axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                    ref validator,
                    ..
                } => validator.rpc_url(),
                axelar_solana_gateway_test_fixtures::base::TestNodeMode::ProgramTest { .. } => {
                    unimplemented!()
                }
            };

            retrying_solana_http_sender::new_client(&retrying_solana_http_sender::Config {
                max_concurrent_rpc_requests: 10,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            })
        }

        // Helper: deploy an interchain token using ALT-backed v0 transaction.
        // Parameters cover only the deployment-specific fields; chain/source are kept as in the
        // tests.
        async fn deploy_interchain_token_with_alt(
            fixture: &mut SolanaAxelarIntegrationMetadata,
            rpc_client: Arc<RpcClient>,
            token_id: FixedBytes<32>,
            name: String,
            symbol: String,
            decimals: u8,
            minter: Bytes,
        ) -> eyre::Result<()> {
            // Build ITS deploy message payload
            let deploy_token_payload = GMPPayload::DeployInterchainToken(DeployInterchainToken {
                selector: U256::from(DeployInterchainToken::MESSAGE_TYPE_ID),
                token_id,
                name,
                symbol,
                decimals,
                minter,
            });

            // Wrap in ReceiveFromHub since it comes from the ITS hub chain
            let abi_payload = GMPPayload::ReceiveFromHub(ReceiveFromHub {
                selector: U256::from(4),
                source_chain: "axelar".to_owned(),
                payload: deploy_token_payload.encode().into(),
            })
            .encode();

            // Prepare ITS message (hash of the ReceiveFromHub payload)
            let message = Message {
                cc_id: CrossChainId {
                    chain: "axelar".to_owned(),
                    id: "message-id".to_owned(),
                },
                source_address: "axelar157hl7gpuknjmhtac2qnphuazv2yerfagva7lsu9vuj2pgn32z22qa26dk4"
                    .to_owned(),
                destination_chain: "solana".to_owned(),
                destination_address: axelar_solana_its::ID.to_string(),
                payload_hash: solana_sdk::keccak::hash(&abi_payload).to_bytes(),
            };

            // Approve the message through the gateway first
            fixture
                .sign_session_and_approve_messages(&fixture.signers.clone(), &[message.clone()])
                .await
                .map_err(|e| eyre::eyre!("Error apporaving message at gateway: {:?}", e))?;

            // Derive the proper gateway incoming message PDA
            let cmd = command_id(&message.cc_id.chain, &message.cc_id.id);
            let (incoming_pda, _) = axelar_solana_gateway::get_incoming_message_pda(&cmd);

            // Build execute instruction
            let execute_ix: solana_sdk::instruction::Instruction = build_execute_instruction(
                fixture.payer.pubkey(),
                incoming_pda,
                message,
                abi_payload.clone(),
                rpc_client.clone(),
            )
            .await?;

            // Create ALT based on all accounts referenced by the execute instruction
            let recent_slot = rpc_client.get_slot().await?;
            let (ix_alt_create, alt_pubkey) =
                create_lookup_table(fixture.payer.pubkey(), fixture.payer.pubkey(), recent_slot);

            let alt_accounts: Vec<Pubkey> =
                execute_ix.accounts.iter().map(|acc| acc.pubkey).collect();

            let ix_alt_extend = extend_lookup_table(
                alt_pubkey,
                fixture.payer.pubkey(),
                Some(fixture.payer.pubkey()),
                alt_accounts,
            );

            // Send ALT create+extend in one tx
            rpc_client
                .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
                    &[ix_alt_create, ix_alt_extend],
                    Some(&fixture.payer.pubkey()),
                    &[&fixture.payer],
                    rpc_client.get_latest_blockhash().await?,
                ))
                .await?;

            // Compose compute budget + execute ix
            let mut all_ixs = Vec::with_capacity(3);
            all_ixs.extend([
                ComputeBudgetInstruction::set_compute_unit_price(1),
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            ]);
            all_ixs.push(execute_ix);

            // Fetch ALT content and compile a V0 message that uses it
            let blockhash = rpc_client.get_latest_blockhash().await?;
            let alt_account_data = rpc_client.get_account_data(&alt_pubkey).await?;
            let alt_state = AddressLookupTable::deserialize(&alt_account_data)?;
            let alt_ref = AddressLookupTableAccount {
                key: alt_pubkey,
                addresses: alt_state.addresses.to_vec(),
            };

            let v0_msg =
                v0::Message::try_compile(&fixture.payer.pubkey(), &all_ixs, &[alt_ref], blockhash)?;

            let message = VersionedMessage::V0(v0_msg);
            let tx = VersionedTransaction::try_new(message, &[&fixture.payer])?;

            // panic!("Transaction deploy size: {}", tx.message.serialize().len());
            // 1004 bytes out of 1232 bytes max for a single tx with ALT

            assert!(
                tx.message.serialize().len() <= 1232,
                "Transaction size {} exceeds max allowed size {}",
                tx.message.serialize().len(),
                1232
            );

            // Send the transaction
            rpc_client.send_and_confirm_transaction(&tx).await?;

            Ok(())
        }

        #[tokio::test]
        async fn test_tx_size_enough_for_its_deploy() {
            let mut fixture = setup().await;
            let (_gas_config, _gas_init_sig, _counter_pda, _init_memo_sig, _init_its_sig, _) =
                setup_aux_contracts(&mut fixture).await;
            let rpc_client = setup_test_rpc_client(&fixture);

            let token_id: FixedBytes<32> = FixedBytes::from_hex(
                "0xcccdb55f29bb017269049e59732c01ac41239e7b61e8a83be5c0ae1143ed8064",
            )
            .unwrap();
            let name = "test".to_owned();
            let symbol = "TOK".to_owned();
            let decimals = 8;
            let minter = Bytes::from(Pubkey::new_unique().to_bytes().to_vec());

            deploy_interchain_token_with_alt(
                &mut fixture,
                rpc_client.clone(),
                token_id,
                name,
                symbol,
                decimals,
                minter,
            )
            .await
            .unwrap();
        }

        #[tokio::test]
        async fn test_tx_size_enough_for_its_token_linking() {
            let mut fixture = setup().await;
            let (_gas_config, _gas_init_sig, _counter_pda, _init_memo_sig, _init_its_sig, _) =
                setup_aux_contracts(&mut fixture).await;
            let rpc_client = setup_test_rpc_client(&fixture);

            let token_id: FixedBytes<32> = FixedBytes::from_hex(
                "0xcccdb55f29bb017269049e59732c01ac41239e7b61e8a83be5c0ae1143ed8064",
            )
            .unwrap();
            let name = "test".to_owned();
            let symbol = "TOK".to_owned();
            let decimals = 8;
            let minter = Bytes::from(Pubkey::new_unique().to_bytes().to_vec());

            deploy_interchain_token_with_alt(
                &mut fixture,
                rpc_client.clone(),
                token_id,
                name,
                symbol,
                decimals,
                minter,
            )
            .await
            .unwrap();

            let mut all_ixs = Vec::with_capacity(3);

            // Prepare ITS deploy interchain token linking payload
            let destination_token_address = axelar_solana_gateway::ID;

            let abi_payload = GMPPayload::LinkToken(LinkToken {
                selector: U256::from(LinkToken::MESSAGE_TYPE_ID),
                token_id,
                token_manager_type: U256::from(Type::LockUnlock as u8),
                source_token_address: Bytes::from(Pubkey::new_unique().to_bytes().to_vec()),
                destination_token_address: destination_token_address.to_bytes().to_vec().into(),
                link_params: Bytes::from(Pubkey::new_unique().to_bytes().to_vec()),
            });

            // Prepare ITS message
            let message = Message {
                cc_id: CrossChainId {
                    chain: "solana".to_owned(),
                    id: "message-id".to_owned(),
                },
                source_address: "source-address".to_owned(),
                destination_chain: "solana".to_owned(),
                destination_address: axelar_solana_its::ID.to_string(),
                payload_hash: [1u8; 32],
            };

            // Approve the message through the gateway first
            fixture
                .sign_session_and_approve_messages(&fixture.signers.clone(), &[message.clone()])
                .await
                .unwrap();

            // Derive the proper gateway incoming message PDA
            let cmd = command_id(&message.cc_id.chain, &message.cc_id.id);
            let (gateway_incoming_message_pda, _) =
                axelar_solana_gateway::get_incoming_message_pda(&cmd);

            // Build execute instruction
            let ix = build_execute_instruction(
                fixture.payer.pubkey(),
                gateway_incoming_message_pda,
                message,
                abi_payload.encode(),
                rpc_client.clone(),
            )
            .await
            .unwrap();

            // Create ALT based on all accounts referenced by the execute instruction
            let recent_slot = rpc_client.get_slot().await.unwrap();
            let (ix_alt_create, alt_pubkey) =
                create_lookup_table(fixture.payer.pubkey(), fixture.payer.pubkey(), recent_slot);

            let alt_accounts: Vec<Pubkey> = ix.accounts.iter().map(|acc| acc.pubkey).collect();

            let ix_alt_extend = extend_lookup_table(
                alt_pubkey,
                fixture.payer.pubkey(),
                Some(fixture.payer.pubkey()),
                alt_accounts,
            );

            // Send ALT create+extend in one tx
            rpc_client
                .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
                    &[ix_alt_create, ix_alt_extend],
                    Some(&fixture.payer.pubkey()),
                    &[&fixture.payer],
                    rpc_client.get_latest_blockhash().await.unwrap(),
                ))
                .await
                .unwrap();

            // Add execute ix to all_ixs
            all_ixs.push(ix);

            // Prepare priority fee ixa in a vec
            all_ixs.extend([
                ComputeBudgetInstruction::set_compute_unit_price(1),
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            ]);

            // Build the transaction
            let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
            let alt_account_data = rpc_client.get_account_data(&alt_pubkey).await.unwrap();
            let alt_state = AddressLookupTable::deserialize(&alt_account_data).unwrap();
            let alt_ref = AddressLookupTableAccount {
                key: alt_pubkey,
                addresses: alt_state.addresses.to_vec(),
            };

            let v0_msg =
                v0::Message::try_compile(&fixture.payer.pubkey(), &all_ixs, &[alt_ref], blockhash)
                    .unwrap();

            let message = VersionedMessage::V0(v0_msg);
            let tx = VersionedTransaction::try_new(message, &[&fixture.payer]).unwrap();

            assert!(
                tx.message.serialize().len() <= 1232,
                "Transaction size {} exceeds max allowed size",
                tx.message.serialize().len()
            );
        }

        #[tokio::test]
        async fn test_tx_size_enough_for_its_token_transfer() {
            let mut fixture = setup().await;
            let (_gas_config, _gas_init_sig, _counter_pda, _init_memo_sig, _init_its_sig, _) =
                setup_aux_contracts(&mut fixture).await;
            let rpc_client = setup_test_rpc_client(&fixture);

            let token_id: FixedBytes<32> = FixedBytes::from_hex(
                "0xcccdb55f29bb017269049e59732c01ac41239e7b61e8a83be5c0ae1143ed8064",
            )
            .unwrap();
            let name = "test".to_owned();
            let symbol = "TOK".to_owned();
            let decimals = 8;
            let minter = Bytes::from(Pubkey::new_unique().to_bytes().to_vec());

            deploy_interchain_token_with_alt(
                &mut fixture,
                rpc_client.clone(),
                token_id,
                name,
                symbol,
                decimals,
                minter,
            )
            .await
            .unwrap();

            let mut all_ixs = Vec::with_capacity(3);

            // Prepare ITS deploy interchain token transfer

            let abi_payload = GMPPayload::InterchainTransfer(InterchainTransfer {
                selector: U256::from(InterchainTransfer::MESSAGE_TYPE_ID),
                token_id,
                source_address: Bytes::from(Pubkey::new_unique().to_bytes().to_vec()),
                destination_address: Bytes::from(Pubkey::new_unique().to_bytes().to_vec()),
                amount: U256::from(1_000_000u64),
                data: Bytes::new(),
            });

            // Prepare ITS message
            let message = Message {
                cc_id: CrossChainId {
                    chain: "solana".to_owned(),
                    id: "message-id".to_owned(),
                },
                source_address: "source-address".to_owned(),
                destination_chain: "solana".to_owned(),
                destination_address: axelar_solana_its::ID.to_string(),
                payload_hash: [1u8; 32],
            };

            // Approve the message through the gateway first
            fixture
                .sign_session_and_approve_messages(&fixture.signers.clone(), &[message.clone()])
                .await
                .unwrap();

            // Derive the proper gateway incoming message PDA
            let cmd = command_id(&message.cc_id.chain, &message.cc_id.id);
            let (gateway_incoming_message_pda, _) =
                axelar_solana_gateway::get_incoming_message_pda(&cmd);

            // Build execute instruction
            let ix = build_execute_instruction(
                fixture.payer.pubkey(),
                gateway_incoming_message_pda,
                message,
                abi_payload.encode(),
                rpc_client.clone(),
            )
            .await
            .unwrap();

            // Create ALT based on all accounts referenced by the execute instruction
            let recent_slot = rpc_client.get_slot().await.unwrap();
            let (ix_alt_create, alt_pubkey) =
                create_lookup_table(fixture.payer.pubkey(), fixture.payer.pubkey(), recent_slot);

            let alt_accounts: Vec<Pubkey> = ix.accounts.iter().map(|acc| acc.pubkey).collect();

            let ix_alt_extend = extend_lookup_table(
                alt_pubkey,
                fixture.payer.pubkey(),
                Some(fixture.payer.pubkey()),
                alt_accounts,
            );

            // Send ALT create+extend in one tx
            rpc_client
                .send_and_confirm_transaction(&Transaction::new_signed_with_payer(
                    &[ix_alt_create, ix_alt_extend],
                    Some(&fixture.payer.pubkey()),
                    &[&fixture.payer],
                    rpc_client.get_latest_blockhash().await.unwrap(),
                ))
                .await
                .unwrap();

            // Add execute ix to all_ixs
            all_ixs.push(ix);

            // Prepare priority fee ixa in a vec
            all_ixs.extend([
                ComputeBudgetInstruction::set_compute_unit_price(1),
                ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            ]);

            // Build the transaction
            let blockhash = rpc_client.get_latest_blockhash().await.unwrap();
            let alt_account_data = rpc_client.get_account_data(&alt_pubkey).await.unwrap();
            let alt_state = AddressLookupTable::deserialize(&alt_account_data).unwrap();
            let alt_ref = AddressLookupTableAccount {
                key: alt_pubkey,
                addresses: alt_state.addresses.to_vec(),
            };

            let v0_msg =
                v0::Message::try_compile(&fixture.payer.pubkey(), &all_ixs, &[alt_ref], blockhash)
                    .unwrap();

            let message = VersionedMessage::V0(v0_msg);
            let tx = VersionedTransaction::try_new(message, &[&fixture.payer]).unwrap();

            assert!(
                tx.message.serialize().len() <= 1232,
                "Transaction size {} exceeds max allowed size",
                tx.message.serialize().len()
            );
        }
    }
}

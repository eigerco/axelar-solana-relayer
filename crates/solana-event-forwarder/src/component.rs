use core::future::Future;
use core::pin::Pin;

use axelar_solana_gas_service::processor::{
    GasServiceEvent, NativeGasAddedEvent, NativeGasPaidForContractCallEvent, NativeGasRefundedEvent,
};
use axelar_solana_gateway::processor::{
    CallContractEvent, CallContractOffchainDataEvent, GatewayEvent, MessageEvent,
    VerifierSetRotated,
};
use futures::{SinkExt as _, StreamExt as _};
use gateway_event_stack::{
    build_program_event_stack, parse_gas_service_log, parse_gateway_logs, MatchContext,
    ProgramInvocationState,
};
use gateway_gas_computation::compute_total_gas;
use itertools::Itertools as _;
use relayer_amplifier_api_integration::amplifier_api::types::{
    BigInt, CallEvent, CallEventMetadata, CommandId, Event, EventBase, EventId, EventMetadata,
    GasCreditEvent, GasRefundedEvent, GatewayV2Message, MessageApprovedEvent,
    MessageApprovedEventMetadata, MessageExecutedEvent, MessageExecutedEventMetadata,
    MessageExecutionStatus, MessageId, PublishEventsRequest, SignersRotatedEvent,
    SignersRotatedMetadata, Token, TxEvent, TxId,
};
use relayer_amplifier_api_integration::AmplifierCommand;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

/// The core component that is responsible for ingesting raw Solana events.
///
/// As a result, the logs get parsed, filtererd and mapped to Amplifier API events.
pub struct SolanaEventForwarder {
    config: crate::Config,
    solana_listener_client: solana_listener::SolanaListenerClient,
    amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
}

impl relayer_engine::RelayerComponent for SolanaEventForwarder {
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GatewayOrGasEvent {
    GatewayEvent(GatewayEvent),
    GasEvent(GasServiceEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum GatewayAndGasEvent {
    CallContract(Option<NativeGasPaidForContractCallEvent>, CallContractEvent),
    CallContractOffchainData(
        Option<NativeGasPaidForContractCallEvent>,
        CallContractOffchainDataEvent,
    ),
    VerifierSetRotated(VerifierSetRotated),
    MessageApproved(MessageEvent),
    MessageExecuted(MessageEvent),
    NativeGasAdded(NativeGasAddedEvent),
    NativeGasRefunded(NativeGasRefundedEvent),
}

impl SolanaEventForwarder {
    /// Instantiate a new `SolanaEventForwarder` using the pre-configured configuration.
    #[must_use]
    pub const fn new(
        config: crate::Config,
        solana_listener_client: solana_listener::SolanaListenerClient,
        amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
    ) -> Self {
        Self {
            config,
            solana_listener_client,
            amplifier_client,
        }
    }

    #[tracing::instrument(skip_all, name = "Solana log forwarder")]
    pub(crate) async fn process_internal(mut self) -> eyre::Result<()> {
        let gateway_match_context =
            MatchContext::new(self.config.gateway_program_id.to_string().as_str());
        let gas_service_match_context =
            MatchContext::new(self.config.gas_service_program_id.to_string().as_str());

        while let Some(message) = self.solana_listener_client.log_receiver.next().await {
            let gateway_program_stack = build_program_event_stack(
                &gateway_match_context,
                &message.logs,
                parse_gateway_logs,
            );
            let gas_events_program_stack = build_program_event_stack(
                &gas_service_match_context,
                &message.logs,
                parse_gas_service_log,
            );
            let total_cost = compute_total_gas(
                self.config.gateway_program_id,
                &message,
                &self.config.rpc,
                self.config.commitment,
            )
            .await?;

            // Collect all successful events into a vector
            let gateway_events_vec = keep_successful_events(gateway_program_stack)
                .into_iter()
                .map(|(idx, event)| (idx, GatewayOrGasEvent::GatewayEvent(event)));
            let gas_events = keep_successful_events(gas_events_program_stack)
                .into_iter()
                .map(|(idx, event)| (idx, GatewayOrGasEvent::GasEvent(event)));
            let all_events = gateway_events_vec
                .chain(gas_events)
                .sorted_by(|event_a, event_b| event_a.0.cmp(&event_b.0));

            // combine gas events with the gateway events
            let combined_events = merge_all_events(all_events);

            // Calculate the number of events
            let num_events = combined_events.len();

            // Compute the price per event, handling the case where num_events is zero
            let price_for_event = total_cost.checked_div(num_events.try_into()?).unwrap_or(0);

            // Map the events to amplifier events with the calculated price
            let events_to_send = combined_events
                .into_iter()
                .flat_map(|(log_index, event)| {
                    let events = map_gateway_event_to_amplifier_event(
                        self.config.source_chain_name.as_str(),
                        event,
                        &message,
                        log_index,
                        price_for_event,
                    );
                    <[Option<Event>; 2]>::from(events).into_iter().flatten()
                })
                .collect::<Vec<_>>();

            // Only send events if there are any from successful invocations
            if events_to_send.is_empty() {
                continue;
            }

            tracing::info!(count = ?events_to_send.len(), "sending solana events to amplifier component");
            let command = AmplifierCommand::PublishEvents(PublishEventsRequest {
                events: events_to_send,
            });
            self.amplifier_client.sender.send(command).await?;
        }
        eyre::bail!("Listener has stopped unexpectedly");
    }
}

/// We have to associate the gas events with the gateway events because for
/// `NativeGasPaidForContractCallEvent` event we need to attach the `message_id` that
/// corresponds with the given `CallContract` event.
fn merge_all_events(
    all_events: std::vec::IntoIter<(usize, GatewayOrGasEvent)>,
) -> Vec<(usize, GatewayAndGasEvent)> {
    let combined_events = all_events.fold(
        // (accumulated vector, pending NativeGasPaidForContractCallEvent)
        (vec![], Vec::<NativeGasPaidForContractCallEvent>::new()),
        |(mut acc, mut pending_gas), (idx, evt)| {
            let mut find_corresponding_gas_call =
                |payload_hash: &[u8; 32], destination_chain: &str, destination_address: &str| {
                    let desired_gas = pending_gas.iter().position(|x| {
                        x.payload_hash == *payload_hash &&
                            x.destination_chain == destination_chain &&
                            x.destination_address == destination_address
                    });
                    desired_gas.map(|idx| pending_gas.remove(idx))
                };
            match evt {
                GatewayOrGasEvent::GatewayEvent(gateway_evt) => {
                    // Check if we have a pending gas event to combine
                    match gateway_evt {
                        GatewayEvent::CallContract(call_event) => {
                            let gas = find_corresponding_gas_call(
                                &call_event.payload_hash,
                                &call_event.destination_chain,
                                &call_event.destination_contract_address,
                            );
                            let event = GatewayAndGasEvent::CallContract(gas, call_event);
                            acc.push((idx, event));
                        }
                        GatewayEvent::CallContractOffchainData(call_event) => {
                            let gas = find_corresponding_gas_call(
                                &call_event.payload_hash,
                                &call_event.destination_chain,
                                &call_event.destination_contract_address,
                            );
                            let event =
                                GatewayAndGasEvent::CallContractOffchainData(gas, call_event);
                            acc.push((idx, event));
                        }
                        GatewayEvent::VerifierSetRotated(evt) => {
                            let event = GatewayAndGasEvent::VerifierSetRotated(evt);
                            acc.push((idx, event));
                        }
                        GatewayEvent::OperatorshipTransferred(event) => {
                            tracing::debug!(?event, "operator ship transferred");
                        }
                        GatewayEvent::MessageApproved(evt) => {
                            let event = GatewayAndGasEvent::MessageApproved(evt);
                            acc.push((idx, event));
                        }
                        GatewayEvent::MessageExecuted(evt) => {
                            let event = GatewayAndGasEvent::MessageExecuted(evt);
                            acc.push((idx, event));
                        }
                    };
                    (acc, pending_gas)
                }
                GatewayOrGasEvent::GasEvent(evt) => {
                    // Other gas events that aren't combined
                    match evt {
                        GasServiceEvent::NativeGasAdded(evt) => {
                            acc.push((idx, GatewayAndGasEvent::NativeGasAdded(evt)));
                        }
                        GasServiceEvent::NativeGasRefunded(evt) => {
                            acc.push((idx, GatewayAndGasEvent::NativeGasRefunded(evt)));
                        }
                        GasServiceEvent::NativeGasPaidForContractCall(evt) => {
                            // Store this gas event and wait for the next CallContract /
                            // CallContractOffchainData
                            pending_gas.push(evt);
                        }
                        GasServiceEvent::SplGasPaidForContractCall(_) |
                        GasServiceEvent::SplGasAdded(_) |
                        GasServiceEvent::SplGasRefunded(_) => {
                            tracing::warn!("unsupported gas event");
                        }
                    };
                    (acc, pending_gas)
                }
            }
        },
    );
    // Extract only the vector from the tuple, we ignore gas calls that don't have matching gateway
    // events
    combined_events.0
}

fn keep_successful_events<T>(
    program_call_stack: Vec<ProgramInvocationState<T>>,
) -> Vec<(usize, T)> {
    program_call_stack
        .into_iter()
        .filter_map(|x| {
            if let ProgramInvocationState::Succeeded(events) = x {
                Some(events)
            } else {
                None
            }
        })
        .flatten()
        .collect::<Vec<_>>()
}

/// At most, it can return 2 events (combo of gas event and gas payment event)
#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "easier to read when all the transformations in one place rather than scattered around"
)]
fn map_gateway_event_to_amplifier_event(
    source_chain: &str,
    event: GatewayAndGasEvent,
    message: &solana_listener::SolanaTransaction,
    log_index: usize,
    price_per_event_in_lamports: u64,
) -> (Option<Event>, Option<Event>) {
    let signature = message.signature.to_string();
    let event_id = EventId::new(&signature, log_index);
    let tx_id = TxId(signature.clone());

    let mut gateway_event = None;
    let mut gas_event = None;

    #[expect(
        clippy::little_endian_bytes,
        reason = "we are guaranteed correct conversion"
    )]
    match event {
        GatewayAndGasEvent::CallContractOffchainData(maybe_gas_paid, _event) => {
            let message_id = MessageId::new(&signature, log_index);

            tracing::info!(
                ?message_id,
                "CallContractOffchainData event is handled on user request"
            );

            if let Some(gas_paid_event) = maybe_gas_paid {
                gas_event = Some(construct_gas_event(
                    event_id,
                    tx_id,
                    message,
                    message_id,
                    gas_paid_event.gas_fee_amount,
                    gas_paid_event.refund_address,
                ));
            };
        }
        GatewayAndGasEvent::CallContract(maybe_gas_paid, call_contract) => {
            let message_id = MessageId::new(&signature, log_index);
            let source_address = call_contract.sender_key.to_string();

            if let Some(gas_paid_event) = maybe_gas_paid {
                gas_event = Some(construct_gas_event(
                    event_id.clone(),
                    tx_id.clone(),
                    message,
                    message_id.clone(),
                    gas_paid_event.gas_fee_amount,
                    gas_paid_event.refund_address,
                ));
            };

            gateway_event = Some(Event::Call(
                CallEvent::builder()
                    .base(
                        EventBase::builder()
                            .event_id(event_id)
                            .meta(Some(
                                EventMetadata::builder()
                                    .tx_id(Some(tx_id))
                                    .timestamp(message.timestamp)
                                    .from_address(Some(source_address.clone()))
                                    .finalized(Some(true))
                                    .extra(CallEventMetadata::builder().build())
                                    .build(),
                            ))
                            .build(),
                    )
                    .message(
                        GatewayV2Message::builder()
                            .message_id(message_id)
                            .source_chain(source_chain.to_owned())
                            .source_address(source_address)
                            .destination_address(call_contract.destination_contract_address)
                            .payload_hash(call_contract.payload_hash.to_vec())
                            .build(),
                    )
                    .destination_chain(call_contract.destination_chain)
                    .payload(call_contract.payload)
                    .build(),
            ));
        }
        GatewayAndGasEvent::VerifierSetRotated(signers) => {
            tracing::info!(?signers, "Signers rotated");

            let le_bytes = signers.epoch.to_le_bytes();
            let Some((le_u64, _)) = le_bytes.split_first_chunk::<8>() else {
                return (gateway_event, gas_event);
            };
            let epoch = u64::from_le_bytes(*le_u64);

            gateway_event = Some(Event::SignersRotated(
                SignersRotatedEvent::builder()
                    .base(
                        EventBase::builder()
                            .event_id(event_id)
                            .meta(Some(
                                EventMetadata::builder()
                                    .tx_id(Some(tx_id))
                                    .timestamp(message.timestamp)
                                    .finalized(Some(true))
                                    .extra(
                                        SignersRotatedMetadata::builder()
                                            .signer_hash(signers.verifier_set_hash.to_vec())
                                            .epoch(epoch)
                                            .build(),
                                    )
                                    .build(),
                            ))
                            .build(),
                    )
                    .cost(
                        Token::builder()
                            .token_id(None)
                            .amount(BigInt::from_u64(price_per_event_in_lamports))
                            .build(),
                    )
                    .build(),
            ));
        }
        GatewayAndGasEvent::MessageApproved(approved_message) => {
            let command_id = approved_message.command_id;
            let span = tracing::info_span!("message", message_id = ?approved_message.cc_id_id);
            let _g = span.enter();

            let message_id = TxEvent(approved_message.cc_id_id);
            gateway_event = Some(Event::MessageApproved(
                MessageApprovedEvent::builder()
                    .base(
                        EventBase::builder()
                            .event_id(event_id)
                            .meta(Some(
                                EventMetadata::builder()
                                    .tx_id(Some(tx_id))
                                    .timestamp(message.timestamp)
                                    .from_address(Some(approved_message.source_address.clone()))
                                    .finalized(Some(true))
                                    .extra(
                                        MessageApprovedEventMetadata::builder()
                                            .command_id(Some(CommandId(
                                                bs58::encode(command_id).into_string(),
                                            )))
                                            .build(),
                                    )
                                    .build(),
                            ))
                            .build(),
                    )
                    .message(
                        GatewayV2Message::builder()
                            .message_id(message_id)
                            .source_chain(approved_message.cc_id_chain)
                            .source_address(approved_message.source_address)
                            .destination_address(approved_message.destination_address.to_string())
                            .payload_hash(approved_message.payload_hash.to_vec())
                            .build(),
                    )
                    .cost(
                        Token::builder()
                            .amount(BigInt::from_u64(price_per_event_in_lamports))
                            .build(),
                    )
                    .build(),
            ));
            tracing::info!("message approved");
        }
        GatewayAndGasEvent::MessageExecuted(executed_message) => {
            let command_id = executed_message.command_id;
            let span = tracing::info_span!("message", message_id = ?executed_message.cc_id_id);
            let _g = span.enter();

            let message_id = TxEvent(executed_message.cc_id_id);
            gateway_event = Some(Event::MessageExecuted(
                MessageExecutedEvent::builder()
                    .base(
                        EventBase::builder()
                            .event_id(event_id)
                            .meta(Some(
                                EventMetadata::builder()
                                    .tx_id(Some(tx_id))
                                    .timestamp(message.timestamp)
                                    .from_address(Some(executed_message.source_address.clone()))
                                    .finalized(Some(true))
                                    .extra(
                                        MessageExecutedEventMetadata::builder()
                                            .command_id(Some(CommandId(
                                                bs58::encode(command_id).into_string(),
                                            )))
                                            .build(),
                                    )
                                    .build(),
                            ))
                            .build(),
                    )
                    .status(MessageExecutionStatus::Successful)
                    .source_chain(executed_message.cc_id_chain)
                    .message_id(message_id)
                    .cost(
                        Token::builder()
                            .amount(BigInt::from_u64(price_per_event_in_lamports))
                            .build(),
                    )
                    .build(),
            ));
            tracing::info!("message executed");
        }
        GatewayAndGasEvent::NativeGasRefunded(event) => {
            let sig = Signature::from(event.tx_hash);
            let message_id = MessageId::new(
                &sig.to_string(),
                usize::try_from(event.log_index).expect("log index must fit into usize"),
            );
            gas_event = Some(Event::GasRefunded(
                GasRefundedEvent::builder()
                    .base(
                        EventBase::builder()
                            .event_id(event_id)
                            .meta(Some(
                                EventMetadata::builder()
                                    .tx_id(Some(tx_id))
                                    .timestamp(message.timestamp)
                                    .from_address(None)
                                    .finalized(Some(true))
                                    .extra(())
                                    .build(),
                            ))
                            .build(),
                    )
                    .message_id(message_id)
                    .cost(
                        Token::builder()
                            .amount(BigInt::from_u64(price_per_event_in_lamports))
                            .token_id(None)
                            .build(),
                    )
                    .recipient_address(event.receiver.to_string())
                    .refunded_amount(
                        Token::builder()
                            .amount(BigInt::from_u64(event.fees))
                            .token_id(None)
                            .build(),
                    )
                    .build(),
            ));
        }
        GatewayAndGasEvent::NativeGasAdded(event) => {
            let sig = Signature::from(event.tx_hash);
            let message_id = MessageId::new(
                &sig.to_string(),
                usize::try_from(event.log_index).expect("log index must fit into usize"),
            );
            gas_event = Some(construct_gas_event(
                event_id,
                tx_id,
                message,
                message_id,
                event.gas_fee_amount,
                event.refund_address,
            ));
        }
    };

    (gateway_event, gas_event)
}

fn construct_gas_event(
    event_id: TxEvent,
    tx_id: TxId,
    message: &solana_listener::SolanaTransaction,
    message_id: TxEvent,
    gas_fee_amount: u64,
    refund_address: Pubkey,
) -> Event {
    Event::GasCredit(
        GasCreditEvent::builder()
            .base(
                EventBase::builder()
                    .event_id(event_id)
                    .meta(Some(
                        EventMetadata::builder()
                            .tx_id(Some(tx_id))
                            .timestamp(message.timestamp)
                            .from_address(None)
                            .finalized(Some(true))
                            .extra(())
                            .build(),
                    ))
                    .build(),
            )
            .message_id(message_id)
            .payment(
                Token::builder()
                    .amount(BigInt::from_u64(gas_fee_amount))
                    .token_id(None)
                    .build(),
            )
            .refund_address(refund_address.to_string())
            .build(),
    )
}

#[cfg(test)]
#[expect(clippy::unimplemented, reason = "needed for the test")]
#[expect(clippy::indexing_slicing, reason = "simpler code")]
#[expect(clippy::unreachable, reason = "simpler code")]
mod tests {
    use core::time::Duration;
    use std::path::PathBuf;
    use std::sync::Arc;

    use axelar_executable::EncodingScheme;
    use axelar_solana_encoding::types::execute_data::MerkleisedPayload;
    use axelar_solana_encoding::types::messages::{CrossChainId, Message, Messages};
    use axelar_solana_encoding::types::payload::Payload;
    use axelar_solana_gateway::state::incoming_message::command_id;
    use axelar_solana_gateway::{get_incoming_message_pda, get_verifier_set_tracker_pda};
    use axelar_solana_gateway_test_fixtures::base::TestFixture;
    use axelar_solana_gateway_test_fixtures::gateway::make_verifiers_with_quorum;
    use axelar_solana_gateway_test_fixtures::SolanaAxelarIntegrationMetadata;
    use axelar_solana_memo_program::instruction::from_axelar_to_solana::build_memo;
    use futures::{SinkExt as _, StreamExt as _};
    use pretty_assertions::assert_eq;
    use relayer_amplifier_api_integration::amplifier_api::types::{
        BigInt, CallEvent, CallEventMetadata, CommandId, Event, EventBase, EventMetadata,
        GasCreditEvent, GatewayV2Message, MessageApprovedEvent, MessageApprovedEventMetadata,
        MessageExecutedEvent, MessageExecutedEventMetadata, MessageExecutionStatus,
        PublishEventsRequest, Token, TxEvent, TxId,
    };
    use relayer_amplifier_api_integration::{AmplifierCommand, AmplifierCommandClient};
    use solana_listener::{fetch_logs, SolanaListenerClient};
    use solana_rpc::rpc::JsonRpcConfig;
    use solana_rpc::rpc_pubsub_service::PubSubConfig;
    use solana_rpc_client::nonblocking::rpc_client::RpcClient;
    use solana_sdk::account::AccountSharedData;
    use solana_sdk::commitment_config::CommitmentConfig;
    use solana_sdk::compute_budget::ComputeBudgetInstruction;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::{Keypair, Signature};
    use solana_sdk::signer::Signer as _;
    use solana_sdk::{bpf_loader_upgradeable, keccak, system_program};
    use solana_test_validator::UpgradeableProgramInfo;

    use crate::SolanaEventForwarder;

    #[test_log::test(tokio::test)]
    async fn event_forwrding_only_call_contract() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let payload = "msg memo only".to_owned();
        let payload_hash = keccak::hash(payload.as_bytes()).0;
        let destination_chain = "evm".to_owned();
        let destination_contract = "0xdeadbeef".to_owned();
        let ix = axelar_solana_memo_program::instruction::call_gateway_with_memo(
            &fixture.gateway_root_pda,
            &counter_pda.0,
            payload.clone(),
            destination_chain.clone(),
            destination_contract.clone(),
            &axelar_solana_gateway::id(),
        )
        .unwrap();
        let only_call_contract_sig = fixture.send_tx_with_signatures(&[ix]).await.unwrap().0[0];

        let tx = fetch_logs(
            CommitmentConfig::confirmed(),
            only_call_contract_sig,
            &rpc_client,
        )
        .await
        .unwrap()
        .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();
        let event_id = TxEvent::new(only_call_contract_sig.to_string().as_str(), 5);
        let expected_event = CallEvent {
            base: EventBase {
                event_id: event_id.clone(),
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(only_call_contract_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: Some(counter_pda.0.to_string()),
                    finalized: Some(true),
                    extra: CallEventMetadata {
                        parent_message_id: None,
                    },
                }),
            },
            message: GatewayV2Message {
                message_id: event_id,
                source_chain: "solana".to_owned(),
                source_address: counter_pda.0.to_string(),
                destination_address: destination_contract.clone(),
                payload_hash: payload_hash.to_vec(),
            },
            destination_chain,
            payload: payload.into_bytes(),
        };

        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::Call(expected_event)])
                    .build()
            )
        );
    }

    #[test_log::test(tokio::test)]
    async fn event_forwarding_message_approved() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, _counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let mut signatures_to_sum = vec![];
        let payload = "msg memo only".to_owned();
        let payload_hash = keccak::hash(payload.as_bytes()).0;
        let source_address = "0xdeadbeef".to_owned();
        let cc_id_id = "0xhash-123".to_owned();
        let message = Message {
            cc_id: CrossChainId {
                chain: "ethereum".to_owned(),
                id: cc_id_id.clone(),
            },
            source_address: source_address.clone(),
            destination_chain: "solana".to_owned(),
            destination_address: axelar_solana_memo_program::ID.to_string(),
            payload_hash,
        };
        let messages = [message];
        let (execute_data, verification_pda, messages) =
            verify_signatures(&messages, &mut fixture, &mut signatures_to_sum).await;
        let message = messages[0].clone();
        let (command_id, approve_signature) = approve_message(
            message,
            &execute_data,
            &mut fixture,
            verification_pda,
            &mut signatures_to_sum,
        )
        .await;

        let tx = fetch_logs(
            CommitmentConfig::confirmed(),
            approve_signature,
            &rpc_client,
        )
        .await
        .unwrap()
        .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();

        let mut expected_sum = 0_u64;
        for sig in signatures_to_sum {
            let tx = fetch_logs(CommitmentConfig::confirmed(), sig, &rpc_client)
                .await
                .unwrap()
                .unwrap();
            expected_sum = expected_sum.saturating_add(tx.cost_in_lamports);
        }

        let event_id = TxEvent::new(approve_signature.to_string().as_str(), 4);
        let event = MessageApprovedEvent {
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(approve_signature.to_string())),
                    timestamp: tx.timestamp,
                    from_address: Some(source_address.clone()),
                    finalized: Some(true),
                    extra: MessageApprovedEventMetadata {
                        command_id: Some(CommandId(bs58::encode(command_id).into_string())),
                    },
                }),
            },
            message: GatewayV2Message {
                message_id: TxEvent(cc_id_id.clone()),
                source_chain: "ethereum".to_owned(),
                source_address: "0xdeadbeef".to_owned(),
                destination_address: axelar_solana_memo_program::ID.to_string(),
                payload_hash: payload_hash.to_vec(),
            },
            cost: Token {
                token_id: None,
                amount: BigInt::from_u64(expected_sum),
            },
        };
        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::MessageApproved(event)])
                    .build()
            )
        );
    }

    #[test_log::test(tokio::test)]
    async fn event_forwarding_two_message_approved() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, _counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let payload = "msg memo only".to_owned();
        let payload_hash = keccak::hash(payload.as_bytes()).0;
        let source_address = "0xdeadbeef".to_owned();
        let cc_id_id = "0xhash-123".to_owned();
        let message_one = Message {
            cc_id: CrossChainId {
                chain: "ethereum".to_owned(),
                id: cc_id_id.clone(),
            },
            source_address: source_address.clone(),
            destination_chain: "solana".to_owned(),
            destination_address: axelar_solana_memo_program::ID.to_string(),
            payload_hash,
        };
        let message_two = Message {
            cc_id: CrossChainId {
                chain: "ethereum".to_owned(),
                id: "0xhash-333".to_owned(),
            },
            source_address: source_address.clone(),
            destination_chain: "solana".to_owned(),
            destination_address: axelar_solana_memo_program::ID.to_string(),
            payload_hash,
        };
        let messages = [message_one, message_two];
        let mut verify_sigs = vec![];
        let (execute_data, verification_pda, messages) =
            verify_signatures(&messages, &mut fixture, &mut verify_sigs).await;
        let message_one = messages[0].clone();
        let message_two = messages[1].clone();
        let mut approve_sigs = vec![];
        let (command_id, approve_signature) = approve_message(
            message_one,
            &execute_data,
            &mut fixture,
            verification_pda,
            &mut approve_sigs,
        )
        .await;
        approve_message(
            message_two,
            &execute_data,
            &mut fixture,
            verification_pda,
            &mut Vec::new(),
        )
        .await;

        let tx = fetch_logs(
            CommitmentConfig::confirmed(),
            approve_signature,
            &rpc_client,
        )
        .await
        .unwrap()
        .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();

        let mut expected_sum = 0_u64;
        for sig in &verify_sigs {
            let tx = fetch_logs(CommitmentConfig::confirmed(), *sig, &rpc_client)
                .await
                .unwrap()
                .unwrap();
            // because we have 2 msgs in the array, the signature verification cost is split between
            // them
            expected_sum =
                expected_sum.saturating_add(tx.cost_in_lamports.checked_div(2).unwrap_or(0));
        }
        for sig in &approve_sigs {
            let tx = fetch_logs(CommitmentConfig::confirmed(), *sig, &rpc_client)
                .await
                .unwrap()
                .unwrap();
            expected_sum = expected_sum.saturating_add(tx.cost_in_lamports);
        }

        let event_id = TxEvent::new(approve_signature.to_string().as_str(), 4);
        let event = MessageApprovedEvent {
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(approve_signature.to_string())),
                    timestamp: tx.timestamp,
                    from_address: Some(source_address.clone()),
                    finalized: Some(true),
                    extra: MessageApprovedEventMetadata {
                        command_id: Some(CommandId(bs58::encode(command_id).into_string())),
                    },
                }),
            },
            message: GatewayV2Message {
                message_id: TxEvent(cc_id_id.clone()),
                source_chain: "ethereum".to_owned(),
                source_address: "0xdeadbeef".to_owned(),
                destination_address: axelar_solana_memo_program::ID.to_string(),
                payload_hash: payload_hash.to_vec(),
            },
            cost: Token {
                token_id: None,
                amount: BigInt::from_u64(expected_sum),
            },
        };
        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::MessageApproved(event)])
                    .build()
            )
        );
    }

    #[test_log::test(tokio::test)]
    async fn event_forwrding_execute_message() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let bytes = b"msg memo only";
        let payload = build_memo(bytes, &counter_pda.0, &[], EncodingScheme::Borsh);
        let encoded_payload = payload.encode().unwrap();
        let payload_hash = keccak::hash(encoded_payload.as_slice()).0;
        let source_address = "0xdeadbeef".to_owned();
        let cc_id_id = "0xhash-123".to_owned();
        let message = Message {
            cc_id: CrossChainId {
                chain: "ethereum".to_owned(),
                id: cc_id_id.clone(),
            },
            source_address: source_address.clone(),
            destination_chain: "solana".to_owned(),
            destination_address: axelar_solana_memo_program::ID.to_string(),
            payload_hash,
        };

        let command_id = command_id(&message.cc_id.chain, &message.cc_id.id);
        let gateway_root_pda = fixture.gateway_root_pda;
        let payer = fixture.payer.pubkey();

        fixture
            .sign_session_and_approve_messages(&fixture.signers.clone(), &[message.clone()])
            .await
            .unwrap();
        let init_payload_sig = fixture
            .send_tx_with_signatures(&[
                axelar_solana_gateway::instructions::initialize_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                    encoded_payload
                        .len()
                        .try_into()
                        .expect("Unexpected u64 overflow in buffer size"),
                )
                .unwrap(),
            ])
            .await
            .unwrap()
            .0[0];

        let write_sig_1 = fixture
            .send_tx_with_signatures(
                &[axelar_solana_gateway::instructions::write_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                    &(encoded_payload[0..10]),
                    0,
                )
                .unwrap()],
            )
            .await
            .unwrap()
            .0[0];
        let write_sig_2 = fixture
            .send_tx_with_signatures(
                &[axelar_solana_gateway::instructions::write_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                    &(encoded_payload[10..]),
                    10,
                )
                .unwrap()],
            )
            .await
            .unwrap()
            .0[0];

        let commit_sig = fixture
            .send_tx_with_signatures(&[
                axelar_solana_gateway::instructions::commit_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                )
                .unwrap(),
            ])
            .await
            .unwrap()
            .0[0];

        let (message_payload_pda, _bump) =
            axelar_solana_gateway::find_message_payload_pda(gateway_root_pda, command_id, payer);

        let (incoming_message_pda, _bump) = get_incoming_message_pda(&command_id);
        let (execute_sigs, _execute_tx) = fixture
            .send_tx_with_signatures(&[axelar_executable::construct_axelar_executable_ix(
                &message,
                &encoded_payload,
                incoming_message_pda,
                message_payload_pda,
            )
            .unwrap()])
            .await
            .unwrap();
        let execute_sig = execute_sigs[0];

        // Close message payload and reclaim lamports
        let close_sig = fixture
            .send_tx_with_signatures(
                &[axelar_solana_gateway::instructions::close_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                )
                .unwrap()],
            )
            .await
            .unwrap()
            .0[0];

        let mut expected_sum = 0_u64;
        for sig in [
            close_sig,
            execute_sig,
            commit_sig,
            write_sig_1,
            write_sig_2,
            init_payload_sig,
        ] {
            let tx = fetch_logs(CommitmentConfig::confirmed(), sig, &rpc_client)
                .await
                .unwrap()
                .unwrap();
            expected_sum = expected_sum.saturating_add(tx.cost_in_lamports);
        }

        let tx = fetch_logs(CommitmentConfig::confirmed(), execute_sig, &rpc_client)
            .await
            .unwrap()
            .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();
        let event_id = TxEvent::new(execute_sig.to_string().as_str(), 4);
        let event = MessageExecutedEvent {
            status: MessageExecutionStatus::Successful,
            source_chain: "ethereum".to_owned(),
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(execute_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: Some(source_address.clone()),
                    finalized: Some(true),
                    extra: MessageExecutedEventMetadata {
                        command_id: Some(CommandId(bs58::encode(command_id).into_string())),
                        child_message_ids: None,
                    },
                }),
            },
            message_id: TxEvent(cc_id_id.clone()),
            cost: Token {
                token_id: None,
                amount: BigInt::from_u64(expected_sum),
            },
        };
        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::MessageExecuted(event)])
                    .build()
            )
        );
    }
    async fn approve_message(
        message: axelar_solana_encoding::types::execute_data::MerkleisedMessage,
        execute_data: &axelar_solana_encoding::types::execute_data::ExecuteData,
        fixture: &mut SolanaAxelarIntegrationMetadata,
        verification_pda: Pubkey,
        signatures_to_sum: &mut Vec<Signature>,
    ) -> ([u8; 32], Signature) {
        let command_id = command_id(
            &message.leaf.message.cc_id.chain,
            &message.leaf.message.cc_id.id,
        );

        let (incoming_message_pda, _incoming_message_pda_bump) =
            get_incoming_message_pda(&command_id);

        let ix = axelar_solana_gateway::instructions::approve_messages(
            message,
            execute_data.payload_merkle_root,
            fixture.gateway_root_pda,
            fixture.payer.pubkey(),
            verification_pda,
            incoming_message_pda,
        )
        .unwrap();
        let (sigs, ..) = fixture.send_tx_with_signatures(&[ix]).await.unwrap();
        let approve_signature = sigs[0];
        signatures_to_sum.push(approve_signature);
        (command_id, approve_signature)
    }

    async fn verify_signatures(
        messages: &[Message],
        fixture: &mut SolanaAxelarIntegrationMetadata,
        signatures_to_sum: &mut Vec<Signature>,
    ) -> (
        axelar_solana_encoding::types::execute_data::ExecuteData,
        Pubkey,
        Vec<axelar_solana_encoding::types::execute_data::MerkleisedMessage>,
    ) {
        let payload = Payload::Messages(Messages(messages.to_vec()));
        let execute_data = fixture.construct_execute_data(&fixture.signers.clone(), payload);
        let ix = axelar_solana_gateway::instructions::initialize_payload_verification_session(
            fixture.payer.pubkey(),
            fixture.gateway_root_pda,
            execute_data.payload_merkle_root,
        )
        .unwrap();
        let sigs = fixture.send_tx_with_signatures(&[ix]).await.unwrap().0;
        signatures_to_sum.push(sigs[0]);

        let (verifier_set_tracker_pda, _verifier_set_tracker_bump) =
            get_verifier_set_tracker_pda(execute_data.signing_verifier_set_merkle_root);

        for signature_leaves in &execute_data.signing_verifier_set_leaves {
            // Verify the signature
            let ix = axelar_solana_gateway::instructions::verify_signature(
                fixture.gateway_root_pda,
                verifier_set_tracker_pda,
                execute_data.payload_merkle_root,
                signature_leaves.clone(),
            )
            .unwrap();
            let (sigs, ..) = fixture
                .send_tx_with_signatures(&[
                    ComputeBudgetInstruction::set_compute_unit_limit(250_000),
                    ix,
                ])
                .await
                .unwrap();
            signatures_to_sum.push(sigs[0]);
        }

        // Check that the PDA contains the expected data
        let (verification_pda, _bump) = axelar_solana_gateway::get_signature_verification_pda(
            &fixture.gateway_root_pda,
            &execute_data.payload_merkle_root,
        );

        let MerkleisedPayload::NewMessages { messages } = execute_data.payload_items.clone() else {
            unreachable!("we constructed a message batch");
        };
        (execute_data, verification_pda, messages)
    }

    fn setup_forwarder(
        rpc_client: &Arc<RpcClient>,
    ) -> (
        futures::channel::mpsc::UnboundedReceiver<AmplifierCommand>,
        futures::channel::mpsc::UnboundedSender<solana_listener::SolanaTransaction>,
    ) {
        let commitment = CommitmentConfig::confirmed();
        let config = crate::Config {
            source_chain_name: "solana".to_owned(),
            gateway_program_id: axelar_solana_gateway::id(),
            gas_service_program_id: axelar_solana_gas_service::id(),
            rpc: Arc::clone(rpc_client),
            commitment,
        };
        let (tx_amplifier, rx_amplifier) = futures::channel::mpsc::unbounded();
        let (tx_listener, rx_listener) = futures::channel::mpsc::unbounded();
        let amplifier_client = AmplifierCommandClient {
            sender: tx_amplifier,
        };
        let solana_listener_client = SolanaListenerClient {
            log_receiver: rx_listener,
        };
        let event_forwarder =
            SolanaEventForwarder::new(config, solana_listener_client, amplifier_client);
        let _task = tokio::spawn(event_forwarder.process_internal());
        (rx_amplifier, tx_listener)
    }

    #[test_log::test(tokio::test)]
    async fn event_forwrding_only_gas_event() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (gas_config, _gas_init_sig, _counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let signature_to_fund = [111; 64];
        let idx_to_fund = 123;
        let refund_address = Pubkey::new_unique();
        let amount_to_refund = 5000;
        let gas_ix = axelar_solana_gas_service::instructions::add_native_gas_instruction(
            &axelar_solana_gas_service::id(),
            &fixture.payer.pubkey(),
            &gas_config.config_pda,
            signature_to_fund,
            idx_to_fund,
            amount_to_refund,
            refund_address,
        )
        .unwrap();
        let only_gas_add_sig = *fixture
            .send_tx_with_signatures(&[gas_ix])
            .await
            .unwrap()
            .0
            .first()
            .unwrap();

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
        let tx = fetch_logs(CommitmentConfig::confirmed(), only_gas_add_sig, &rpc_client)
            .await
            .unwrap()
            .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();
        let event_id = TxEvent::new(only_gas_add_sig.to_string().as_str(), 3);
        let message_id_to_fund = TxEvent::new(
            Signature::from(signature_to_fund).to_string().as_str(),
            idx_to_fund.try_into().unwrap(),
        );
        let expected_event = GasCreditEvent {
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(only_gas_add_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: Some(true),
                    extra: (),
                }),
            },
            message_id: message_id_to_fund,
            refund_address: refund_address.to_string(),
            payment: Token {
                token_id: None,
                amount: BigInt::from_u64(amount_to_refund),
            },
        };

        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::GasCredit(expected_event)])
                    .build()
            )
        );
    }

    #[test_log::test(tokio::test)]
    async fn event_forwrding_with_gas_and_contract_call() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        let payload = "msg memo and gas".to_owned();
        let destination_chain_name = "evm".to_owned();
        let payload_hash = solana_sdk::keccak::hashv(&[payload.as_bytes()]).0;
        let destination_address = "0xdeadbeef".to_owned();
        let ix = axelar_solana_memo_program::instruction::call_gateway_with_memo(
            &fixture.gateway_root_pda,
            &counter_pda.0,
            payload.clone(),
            destination_chain_name.clone(),
            destination_address.clone(),
            &axelar_solana_gateway::id(),
        )
        .unwrap();
        let refund_address = Pubkey::new_unique();
        let gas_fee_amount = 5000;
        let gas_ix =
            axelar_solana_gas_service::instructions::pay_native_for_contract_call_instruction(
                &axelar_solana_gas_service::id(),
                &fixture.payer.pubkey(),
                &gas_config.config_pda,
                destination_chain_name.clone(),
                destination_address.clone(),
                payload_hash,
                refund_address,
                vec![],
                gas_fee_amount,
            )
            .unwrap();
        let gas_and_call_contract_sig = fixture
            .send_tx_with_signatures(&[gas_ix, ix])
            .await
            .unwrap()
            .0[0];

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
        let tx = fetch_logs(
            CommitmentConfig::confirmed(),
            gas_and_call_contract_sig,
            &rpc_client,
        )
        .await
        .unwrap()
        .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();
        let event_id = TxEvent::new(gas_and_call_contract_sig.to_string().as_str(), 11);
        let expected_call_event = CallEvent {
            base: EventBase {
                event_id: event_id.clone(),
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(gas_and_call_contract_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: Some(counter_pda.0.to_string()),
                    finalized: Some(true),
                    extra: CallEventMetadata {
                        parent_message_id: None,
                    },
                }),
            },
            message: GatewayV2Message {
                message_id: event_id.clone(),
                source_chain: "solana".to_owned(),
                source_address: counter_pda.0.to_string(),
                destination_address: destination_address.clone(),
                payload_hash: payload_hash.to_vec(),
            },
            destination_chain: destination_chain_name.clone(),
            payload: payload.into_bytes(),
        };
        let expected_gas_event = GasCreditEvent {
            base: EventBase {
                event_id: event_id.clone(),
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(gas_and_call_contract_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: Some(true),
                    extra: (),
                }),
            },
            message_id: event_id,
            refund_address: refund_address.to_string(),
            payment: Token {
                token_id: None,
                amount: BigInt::from_u64(gas_fee_amount),
            },
        };

        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![
                        Event::Call(expected_call_event),
                        Event::GasCredit(expected_gas_event)
                    ])
                    .build()
            )
        );
    }

    pub(crate) async fn setup_aux_contracts(
        fixture: &mut SolanaAxelarIntegrationMetadata,
    ) -> (
        axelar_solana_gateway_test_fixtures::gas_service::GasServiceUtils,
        Signature,
        (Pubkey, u8),
        Signature,
    ) {
        // init gas config
        let gas_service_upgr_auth = fixture.payer.insecure_clone();
        let gas_config = fixture.setup_default_gas_config(gas_service_upgr_auth);
        let ix = axelar_solana_gas_service::instructions::init_config(
            &axelar_solana_gas_service::ID,
            &fixture.payer.pubkey(),
            &gas_config.config_authority.pubkey(),
            &gas_config.config_pda,
            gas_config.salt,
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
        let counter_pda = axelar_solana_memo_program::get_counter_pda(&fixture.gateway_root_pda);
        let ix = axelar_solana_memo_program::instruction::initialize(
            &fixture.payer.pubkey(),
            &fixture.gateway_root_pda,
            &counter_pda,
        )
        .unwrap();
        let init_memo_sig = fixture.send_tx_with_signatures(&[ix]).await.unwrap().0[0];
        (gas_config, gas_init_sig, counter_pda, init_memo_sig)
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

    pub(crate) async fn setup() -> (SolanaAxelarIntegrationMetadata, Arc<RpcClient>) {
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
            AccountSharedData::new(u64::MAX, 0, &system_program::ID),
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
                program_id: axelar_solana_memo_program::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_memo_program.so"),
            },
        ]);

        let forced_sleep = if std::env::var("CI").is_ok() {
            Duration::from_millis(1000)
        } else {
            Duration::from_millis(500)
        };
        let mut fixture = TestFixture::new_test_validator(validator, forced_sleep).await;
        let init_payer = fixture.payer.insecure_clone();
        fixture.payer = upgrade_authority.insecure_clone();

        let operator = Keypair::new();
        let domain_separator = [42; 32];
        let initial_signers = make_verifiers_with_quorum(&[42, 33, 26], 0, 100, domain_separator);
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

        (fixture, rpc_client)
    }
}

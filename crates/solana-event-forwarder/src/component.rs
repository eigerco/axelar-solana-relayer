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
#[derive(Debug)]
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
            // todo -- total cost is not representative
            let total_cost = message.cost_in_lamports;

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
                    .source_chain(executed_message.source_address)
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
mod tests {
    use core::time::Duration;
    use std::path::PathBuf;

    use axelar_solana_gateway_test_fixtures::base::TestFixture;
    use axelar_solana_gateway_test_fixtures::gateway::make_verifiers_with_quorum;
    use axelar_solana_gateway_test_fixtures::SolanaAxelarIntegrationMetadata;
    use futures::{SinkExt as _, StreamExt as _};
    use pretty_assertions::assert_eq;
    use relayer_amplifier_api_integration::amplifier_api::types::{
        BigInt, CallEvent, CallEventMetadata, Event, EventBase, EventMetadata, GasCreditEvent,
        GatewayV2Message, PublishEventsRequest, Token, TxEvent, TxId,
    };
    use relayer_amplifier_api_integration::{AmplifierCommand, AmplifierCommandClient};
    use solana_listener::{fetch_logs, SolanaListenerClient};
    use solana_rpc::rpc::JsonRpcConfig;
    use solana_rpc::rpc_pubsub_service::PubSubConfig;
    use solana_sdk::account::AccountSharedData;
    use solana_sdk::commitment_config::CommitmentConfig;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::{Keypair, Signature};
    use solana_sdk::signer::Signer as _;
    use solana_sdk::{bpf_loader_upgradeable, keccak, system_program};
    use solana_test_validator::UpgradeableProgramInfo;

    use crate::SolanaEventForwarder;

    #[test_log::test(tokio::test)]
    async fn event_forwrding_only_call_contract() {
        // setup
        let config = crate::Config {
            source_chain_name: "solana".to_owned(),
            gateway_program_id: axelar_solana_gateway::id(),
            gas_service_program_id: axelar_solana_gas_service::id(),
        };
        let (tx_amplifier, mut rx_amplifier) = futures::channel::mpsc::unbounded();
        let (mut tx_listener, rx_listener) = futures::channel::mpsc::unbounded();
        let amplifier_client = AmplifierCommandClient {
            sender: tx_amplifier,
        };
        let solana_listener_client = SolanaListenerClient {
            log_receiver: rx_listener,
        };
        let event_forwarder =
            SolanaEventForwarder::new(config, solana_listener_client, amplifier_client);
        let _task = tokio::spawn(event_forwarder.process_internal());

        let mut fixture = setup().await;
        let (_gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;

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
                max_concurrent_rpc_requests: 1,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });
        let tx = fetch_logs(
            CommitmentConfig::confirmed(),
            only_call_contract_sig,
            &rpc_client,
        )
        .await
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
    async fn event_forwrding_only_gas_event() {
        // setup
        let config = crate::Config {
            source_chain_name: "solana".to_owned(),
            gateway_program_id: axelar_solana_gateway::id(),
            gas_service_program_id: axelar_solana_gas_service::id(),
        };
        let (tx_amplifier, mut rx_amplifier) = futures::channel::mpsc::unbounded();
        let (mut tx_listener, rx_listener) = futures::channel::mpsc::unbounded();
        let amplifier_client = AmplifierCommandClient {
            sender: tx_amplifier,
        };
        let solana_listener_client = SolanaListenerClient {
            log_receiver: rx_listener,
        };
        let event_forwarder =
            SolanaEventForwarder::new(config, solana_listener_client, amplifier_client);
        let _task = tokio::spawn(event_forwarder.process_internal());

        let mut fixture = setup().await;
        let (gas_config, _gas_init_sig, _counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;

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
                max_concurrent_rpc_requests: 1,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });
        let tx = fetch_logs(CommitmentConfig::confirmed(), only_gas_add_sig, &rpc_client)
            .await
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
        let config = crate::Config {
            source_chain_name: "solana".to_owned(),
            gateway_program_id: axelar_solana_gateway::id(),
            gas_service_program_id: axelar_solana_gas_service::id(),
        };
        let (tx_amplifier, mut rx_amplifier) = futures::channel::mpsc::unbounded();
        let (mut tx_listener, rx_listener) = futures::channel::mpsc::unbounded();
        let amplifier_client = AmplifierCommandClient {
            sender: tx_amplifier,
        };
        let solana_listener_client = SolanaListenerClient {
            log_receiver: rx_listener,
        };
        let event_forwarder =
            SolanaEventForwarder::new(config, solana_listener_client, amplifier_client);
        let _task = tokio::spawn(event_forwarder.process_internal());

        let mut fixture = setup().await;
        let (gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;

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
                max_concurrent_rpc_requests: 1,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });
        let tx = fetch_logs(
            CommitmentConfig::confirmed(),
            gas_and_call_contract_sig,
            &rpc_client,
        )
        .await
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
}

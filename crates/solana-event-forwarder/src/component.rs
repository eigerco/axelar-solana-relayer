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
    MessageApprovedEventMetadata, MessageId, PublishEventsRequest, SignersRotatedEvent,
    SignersRotatedMetadata, Token, TxEvent, TxId,
};
use relayer_amplifier_api_integration::AmplifierCommand;
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
        let match_context = MatchContext::new(self.config.gateway_program_id.to_string().as_str());

        while let Some(message) = self.solana_listener_client.log_receiver.next().await {
            let gateway_program_stack =
                build_program_event_stack(&match_context, &message.logs, parse_gateway_logs);
            let gas_events_program_stack =
                build_program_event_stack(&match_context, &message.logs, parse_gas_service_log);
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

            // we have to associate the gas events with the gateway events because for
            // NativeGasPaidForContractCallEvent event we need to attach the message_id that
            // corresponds with the given CallContract event.
            let combined_events = all_events
                .fold(
                    // (accumulated vector, pending NativeGasPaidForContractCallEvent)
                    (vec![], Vec::<NativeGasPaidForContractCallEvent>::new()),
                    |(mut acc, mut pending_gas), (idx, evt)| {
                        let mut find_corresponding_gas_call =
                            |payload_hash: &[u8; 32],
                             destination_chain: &str,
                             destination_address: &str| {
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
                                        let event =
                                            GatewayAndGasEvent::CallContract(gas, call_event);
                                        acc.push((idx, event));
                                        (acc, pending_gas)
                                    }
                                    GatewayEvent::CallContractOffchainData(call_event) => {
                                        let gas = find_corresponding_gas_call(
                                            &call_event.payload_hash,
                                            &call_event.destination_chain,
                                            &call_event.destination_contract_address,
                                        );
                                        let event = GatewayAndGasEvent::CallContractOffchainData(
                                            gas, call_event,
                                        );
                                        acc.push((idx, event));
                                        (acc, pending_gas)
                                    }
                                    GatewayEvent::VerifierSetRotated(evt) => {
                                        let event = GatewayAndGasEvent::VerifierSetRotated(evt);
                                        acc.push((idx, event));
                                        (acc, pending_gas)
                                    }
                                    GatewayEvent::OperatorshipTransferred(event) => {
                                        tracing::debug!(?event, "operator ship transferred");
                                        (acc, pending_gas)
                                    }
                                    GatewayEvent::MessageApproved(evt) => {
                                        let event = GatewayAndGasEvent::MessageApproved(evt);
                                        acc.push((idx, event));
                                        (acc, pending_gas)
                                    }
                                    GatewayEvent::MessageExecuted(evt) => {
                                        let event = GatewayAndGasEvent::MessageExecuted(evt);
                                        acc.push((idx, event));
                                        (acc, pending_gas)
                                    }
                                }
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
                )
                .0; // Extract only the vector from the tuple, we ignore gas calls that don't have call
                    // contract events attached

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
                    &gas_paid_event,
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
                    &gas_paid_event,
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
        GatewayAndGasEvent::MessageExecuted(ref _executed_message) => {
            tracing::warn!(
                "current gateway event does not produce enough artifacts to relay this message"
            );
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
            gas_event = Some(Event::GasCredit(
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
                            .amount(BigInt::from_u64(event.gas_fee_amount))
                            .token_id(None)
                            .build(),
                    )
                    .refund_address(event.refund_address.to_string())
                    .build(),
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
    gas_paid_event: &NativeGasPaidForContractCallEvent,
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
                    .amount(BigInt::from_u64(gas_paid_event.gas_fee_amount))
                    .token_id(None)
                    .build(),
            )
            .refund_address(gas_paid_event.refund_address.to_string())
            .build(),
    )
}

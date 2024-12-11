use core::future::Future;
use core::pin::Pin;

use axelar_solana_gateway::processor::GatewayEvent;
use futures::{SinkExt as _, StreamExt as _};
use gateway_event_stack::{
    build_program_event_stack, parse_gateway_logs, MatchContext, ProgramInvocationState,
};
use relayer_amplifier_api_integration::amplifier_api::types::{
    BigInt, CallEvent, CallEventMetadata, CommandId, Event, EventBase, EventId, EventMetadata,
    GatewayV2Message, MessageApprovedEvent, MessageApprovedEventMetadata, MessageId,
    PublishEventsRequest, SignersRotatedEvent, SignersRotatedMetadata, Token, TxEvent, TxId,
};
use relayer_amplifier_api_integration::AmplifierCommand;

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
            let total_cost = message.cost_in_lamports;

            // Collect all successful events into a vector
            let events_vec = gateway_program_stack
                .into_iter()
                .filter_map(|x| {
                    if let ProgramInvocationState::Succeeded(events) = x {
                        Some(events)
                    } else {
                        None
                    }
                })
                .flatten()
                .collect::<Vec<_>>();

            // Calculate the number of events
            let num_events = events_vec.len();

            // Compute the price per event, handling the case where num_events is zero
            let price_for_event = total_cost.checked_div(num_events.try_into()?).unwrap_or(0);

            // Map the events to amplifier events with the calculated price
            let events_to_send = events_vec
                .into_iter()
                .filter_map(|(log_index, event)| {
                    map_gateway_event_to_amplifier_event(
                        self.config.source_chain_name.as_str(),
                        event,
                        &message,
                        log_index,
                        price_for_event,
                    )
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

#[expect(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    reason = "easier to read when all the transformations in one place rather than scattered around"
)]
fn map_gateway_event_to_amplifier_event(
    source_chain: &str,
    event: GatewayEvent,
    message: &solana_listener::SolanaTransaction,
    log_index: usize,
    price_per_event_in_lamports: u64,
) -> Option<Event> {
    let signature = message.signature.to_string();
    let event_id = EventId::new(&signature, log_index);
    let tx_id = TxId(signature.clone());

    #[expect(
        clippy::little_endian_bytes,
        reason = "we are guaranteed correct conversion"
    )]
    match event {
        GatewayEvent::CallContract(call_contract) => {
            let message_id = MessageId::new(&signature, log_index);
            let source_address = call_contract.sender_key.to_string();
            let amplifier_event = Event::Call(
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
            );
            Some(amplifier_event)
        }
        GatewayEvent::VerifierSetRotated(signers) => {
            tracing::info!(?signers, "Signers rotated");

            let le_bytes = signers.epoch.to_le_bytes();
            let (le_u64, _) = le_bytes.split_first_chunk::<8>()?;
            let epoch = u64::from_le_bytes(*le_u64);

            let amplifier_event = Event::SignersRotated(
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
            );
            Some(amplifier_event)
        }
        GatewayEvent::MessageApproved(approved_message) => {
            let command_id = approved_message.command_id;
            let span = tracing::info_span!("message", message_id = ?approved_message.cc_id_id);
            let _g = span.enter();

            let message_id = TxEvent(approved_message.cc_id_id);
            let amplifier_event = Event::MessageApproved(
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
            );
            tracing::info!("message approved");
            Some(amplifier_event)
        }
        GatewayEvent::MessageExecuted(ref _executed_message) => {
            tracing::warn!(
                "current gateway event does not produce enough artifacts to relay this message"
            );
            None
        }
        GatewayEvent::OperatorshipTransferred(ref new_operatorship) => {
            tracing::info!(?new_operatorship, "Operatorship transferred");
            None
        }
    }
}

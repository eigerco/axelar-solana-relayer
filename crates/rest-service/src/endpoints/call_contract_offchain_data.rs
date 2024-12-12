//! Endpoint for calling a contract with offchain data.
use core::str::FromStr as _;
use std::sync::Arc;

use amplifier_api::types::{
    CallEvent, CallEventMetadata, Event, EventBase, EventId, EventMetadata, GatewayV2Message,
    MessageId, PublishEventsRequest, TxId,
};
use axelar_solana_gateway::processor::{CallContractOffchainDataEvent, GatewayEvent};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{post, MethodRouter};
use futures::channel::mpsc::UnboundedSender;
use futures::SinkExt as _;
use gateway_event_stack::{MatchContext, ProgramInvocationState};
use relayer_amplifier_api_integration::AmplifierCommand;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_listener::{fetch_logs, SolanaTransaction};
use solana_sdk::signature::Signature;
use thiserror::Error;

use crate::component::ServiceState;

pub(crate) const PATH: &str = "/call-contract-offchain-data/:signature";
pub(crate) fn handlers() -> MethodRouter<Arc<ServiceState>> {
    post(post_handler)
}

#[derive(Debug, Error)]
pub(crate) enum CallContractOffchainDataError {
    #[error("Failed to fetch transaction logs")]
    FailedToFetchTransactionLogs(#[from] eyre::Report),

    #[error("Payload hashes don't match")]
    PayloadHashMismatch,

    #[error("Successful transaction with CallContractOffchainDataEvent not found")]
    EventNotFound,

    #[error("Failed to relay message to Axelar Amplifier")]
    FailedToRelayToAmplifier,

    #[error("Invalid transaction signature")]
    InvalidTransactionSignature,
}

impl IntoResponse for CallContractOffchainDataError {
    fn into_response(self) -> axum::response::Response {
        let error_tuple = match self {
            Self::FailedToRelayToAmplifier => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            Self::FailedToFetchTransactionLogs(_) |
            Self::PayloadHashMismatch |
            Self::InvalidTransactionSignature => (StatusCode::BAD_REQUEST, self.to_string()),
            Self::EventNotFound => (StatusCode::NOT_FOUND, self.to_string()),
        };

        error_tuple.into_response()
    }
}

async fn post_handler(
    State(state): State<Arc<ServiceState>>,
    Path(signature): Path<String>,
    payload: Bytes,
) -> Result<(), CallContractOffchainDataError> {
    let Ok(parsed_signature) = Signature::from_str(&signature) else {
        tracing::warn!("Invalid transaction signature");
        return Err(CallContractOffchainDataError::InvalidTransactionSignature);
    };
    let data = payload.to_vec();

    let result = try_build_and_push_event_with_data(
        data,
        parsed_signature,
        state.chain_name().to_owned(),
        state.rpc_client(),
        state.amplifier_client().sender.clone(),
    )
    .await;

    match result {
        Ok(()) => tracing::info!("CallContract data successfully forwarded to Axelar Amplifier"),
        Err(CallContractOffchainDataError::FailedToRelayToAmplifier) => {
            tracing::error!("Irrecoverable error: Communication with Axelar Amplifier was lost");
            state
                .shutdown(CallContractOffchainDataError::FailedToRelayToAmplifier.into())
                .await;
        }
        Err(ref report) => {
            tracing::warn!("Failed to forward message to Axelar: {report}");
        }
    }

    result
}

async fn try_build_and_push_event_with_data(
    data: Vec<u8>,
    signature: Signature,
    chain_name: String,
    solana_rpc_client: Arc<RpcClient>,
    mut amplifier_channel: UnboundedSender<AmplifierCommand>,
) -> Result<(), CallContractOffchainDataError> {
    let solana_transaction = fetch_logs(signature, &solana_rpc_client).await?;
    let hash = solana_sdk::keccak::hash(&data).to_bytes();
    let match_context = MatchContext::new(&axelar_solana_gateway::id().to_string());
    let invocations = gateway_event_stack::build_program_event_stack(
        &match_context,
        solana_transaction.logs.as_slice(),
        gateway_event_stack::parse_gateway_logs,
    );

    let mut events_iter = invocations
        .into_iter()
        .filter_map(|inv| match inv {
            ProgramInvocationState::Succeeded(events) => Some(events),
            ProgramInvocationState::InProgress(_) | ProgramInvocationState::Failed(_) => None,
        })
        .flatten();
    let maybe_event = events_iter.find_map(|(idx, event)| {
        if let GatewayEvent::CallContractOffchainData(event_data) = event {
            Some((idx, event_data))
        } else {
            None
        }
    });

    match maybe_event {
        Some((idx, event_data)) if event_data.payload_hash == hash => {
            let amplifier_event =
                build_amplifier_event(chain_name, &solana_transaction, event_data, data, idx);
            let command = AmplifierCommand::PublishEvents(PublishEventsRequest {
                events: vec![amplifier_event],
            });

            amplifier_channel
                .send(command)
                .await
                .map_err(|_err| CallContractOffchainDataError::FailedToRelayToAmplifier)?;

            Ok(())
        }
        Some(_) => Err(CallContractOffchainDataError::PayloadHashMismatch),
        None => Err(CallContractOffchainDataError::EventNotFound),
    }
}

fn build_amplifier_event(
    source_chain: String,
    transaction: &SolanaTransaction,
    solana_event: CallContractOffchainDataEvent,
    payload: Vec<u8>,
    log_index: usize,
) -> Event {
    let tx_id = TxId(transaction.signature.to_string());
    let message_id = MessageId::new(&transaction.signature.to_string(), log_index);
    let event_id = EventId::new(&transaction.signature.to_string(), log_index);
    let source_address = solana_event.sender_key.to_string();

    Event::Call(
        CallEvent::builder()
            .base(
                EventBase::builder()
                    .event_id(event_id)
                    .meta(Some(
                        EventMetadata::builder()
                            .tx_id(Some(tx_id))
                            .timestamp(transaction.timestamp)
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
                    .source_chain(source_chain)
                    .source_address(source_address)
                    .destination_address(solana_event.destination_contract_address)
                    .payload_hash(solana_event.payload_hash.to_vec())
                    .build(),
            )
            .destination_chain(solana_event.destination_chain)
            .payload(payload)
            .build(),
    )
}

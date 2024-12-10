//! Endpoint for calling a contract with offchain data.
use std::sync::Arc;

use amplifier_api::types::{
    CallEvent, CallEventMetadata, Event, EventBase, EventId, EventMetadata, GatewayV2Message,
    MessageId, PublishEventsRequest, TxId,
};
use axelar_solana_gateway::processor::{CallContractOffchainDataEvent, GatewayEvent};
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{post, MethodRouter};
use futures::channel::mpsc::UnboundedSender;
use futures::SinkExt as _;
use gateway_event_stack::{MatchContext, ProgramInvocationState};
use relayer_amplifier_api_integration::AmplifierCommand;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_listener::{fetch_logs, SolanaTransaction};
use solana_sdk::signature::Signature;
use thiserror::Error;

use crate::component::ServiceState;

pub(crate) const PATH: &str = "/call-contract-offchain-data";
pub(crate) fn handlers() -> MethodRouter<Arc<ServiceState>> {
    post(post_handler)
}

/// Payload for the call-contract-offchain-data endpoint.
#[derive(Serialize, Deserialize)]
pub struct Payload {
    /// Signature of the transaction to fetch the event from.
    pub signature: Signature,
    /// The data to send to the destination contract.
    pub data: String,
}

#[derive(Debug, Error)]
pub(crate) enum CallContractOffchainDataError {
    /// Failed to fetch transaction logs.
    #[error("Failed to fetch transaction logs: {0}")]
    FailedToFetchTransactionLogs(#[from] eyre::Report),

    /// The hash of the given payload doesn't match the one emitted in the transaction logs.
    #[error("Payload hashes don't match")]
    PayloadHashMismatch,

    #[error("Successful transaction with CallContractOffchainDataEvent not found")]
    EventNotFound,

    #[error("Failed to relay message to Axelar Amplifier")]
    FailedToRelayToAmplifier,

    #[error("Failed to decode base58 data: {0}")]
    DecodeError(#[from] bs58::decode::Error),
}

impl IntoResponse for CallContractOffchainDataError {
    fn into_response(self) -> axum::response::Response {
        let error_tuple = match self {
            Self::FailedToFetchTransactionLogs(report) => {
                (StatusCode::INTERNAL_SERVER_ERROR, report.to_string())
            }
            Self::FailedToRelayToAmplifier | Self::DecodeError(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }

            Self::PayloadHashMismatch => (StatusCode::BAD_REQUEST, self.to_string()),
            Self::EventNotFound => (StatusCode::NOT_FOUND, self.to_string()),
        };

        error_tuple.into_response()
    }
}

async fn post_handler(
    State(state): State<Arc<ServiceState>>,
    Json(payload): Json<Payload>,
) -> Result<(), CallContractOffchainDataError> {
    let data = bs58::decode(&payload.data).into_vec()?;
    try_build_and_push_event_with_data(
        data,
        payload.signature,
        state.chain_name().to_owned(),
        state.rpc_client(),
        state.amplifier_client().sender.clone(),
    )
    .await
}

async fn try_build_and_push_event_with_data(
    data: Vec<u8>,
    signature: Signature,
    chain_name: String,
    solana_rpc_client: Arc<RpcClient>,
    mut amplifier_channel: UnboundedSender<AmplifierCommand>,
) -> Result<(), CallContractOffchainDataError> {
    let solana_transaction = fetch_logs(signature, &solana_rpc_client).await?;
    let match_context = MatchContext::new(&axelar_solana_gateway::id().to_string());
    let invocations = gateway_event_stack::build_program_event_stack(
        &match_context,
        solana_transaction.logs.as_slice(),
        gateway_event_stack::parse_gateway_logs,
    );

    for invocation in invocations {
        if let ProgramInvocationState::Succeeded(events) = invocation {
            for (idx, event) in events {
                if let GatewayEvent::CallContractOffchainData(event_data) = event {
                    if event_data.payload_hash == solana_sdk::keccak::hash(&data).to_bytes() {
                        let amplifier_event = build_amplifier_event(
                            chain_name,
                            &solana_transaction,
                            event_data,
                            data,
                            idx,
                        );

                        let command = AmplifierCommand::PublishEvents(PublishEventsRequest {
                            events: vec![amplifier_event],
                        });

                        amplifier_channel.send(command).await.map_err(|_err| {
                            CallContractOffchainDataError::FailedToRelayToAmplifier
                        })?;

                        return Ok(());
                    }

                    return Err(CallContractOffchainDataError::PayloadHashMismatch);
                }
            }
        }
    }

    Err(CallContractOffchainDataError::EventNotFound)
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

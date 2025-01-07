use core::pin::Pin;
use core::str::FromStr as _;
use core::task::Context;
use std::ops::ControlFlow;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::{poll_fn, BoxStream, FuturesUnordered, StreamExt as _};
use futures::task::Poll;
use futures::{SinkExt as _, Stream, StreamExt};
use solana_client::nonblocking::pubsub_client::PubsubClientResult;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use solana_client::rpc_response::RpcLogsResponse;
use solana_rpc_client_api::response::Response;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Signature;
use tracing::{info_span, Instrument as _};

use super::MessageSender;
use crate::component::log_processor::fetch_logs;
use crate::component::signature_batch_scanner;
use crate::SolanaTransaction;

// #[tracing::instrument(skip_all, err, name = "realtime log ingestion")]
pub(crate) async fn process_realtime_logs(
    config: crate::Config,
    latest_processed_signature: Option<Signature>,
    rpc_client: Arc<RpcClient>,
    mut signature_sender: MessageSender,
) -> Result<(), eyre::Error> {
    let gateway_program_address = config.gateway_program_address;
    let gas_service_config_pda = config.gas_service_config_pda;

    'outer: loop {
        tracing::info!(
            endpoint = ?config.solana_ws.as_str(),
            ?gateway_program_address,
            "init new WS connection"
        );
        let client =
            solana_client::nonblocking::pubsub_client::PubsubClient::new(config.solana_ws.as_str())
                .await?;

        // Reason for subscribing to gateway and gas service separately
        // (https://solana.com/docs/rpc/websocket/logssubscribe):
        //
        // "The `mentions` field currently only supports one Pubkey string per method call. Listing
        // additional addresses will result in an error."
        let (gateway_ws_stream, _unsub) =
            init_logs_subscribe(&client, gateway_program_address).await?;

        // we subscribe to all txs that contain the gas service config PDA (not just the
        // program id, because there can be many configs).
        let (gas_service_ws_stream, _unsub) =
            init_logs_subscribe(&client, gas_service_config_pda).await?;

        let mut ws_stream =
            round_robin_two_streams(gateway_ws_stream.fuse(), gas_service_ws_stream.fuse())
                // only successful txs are checked
                .filter(|x| std::future::ready(x.value.err.is_none()));

        // We have special handling for the very first message we receive:
        // We fetch messages in batches based on the config defitned strategy to recover old
        // messages
        // Get the first item from the ws_stream
        let first_item = ws_stream.next().await;
        let Some(first_item) = first_item else {
            // Reconnect if connection dropped
            continue 'outer;
        };
        // Process the first item
        let sig = Signature::from_str(&first_item.value.signature)
            .expect("signature from RPC must be valid");
        let t2_signature = fetch_logs(sig, &rpc_client).await?;

        // Fetch missed batches
        signature_batch_scanner::fetch_batches_in_range(
            &config,
            Arc::clone(&rpc_client),
            &signature_sender,
            Some(t2_signature.signature),
            latest_processed_signature,
        )
        .instrument(info_span!("fetching missed signatures"))
        .await?;
        // Send the first item
        signature_sender.send(t2_signature).await?;

        // Create the FuturesUnordered
        let mut fetch_futures = FuturesUnordered::new();

        // Manual polling using poll_fn
        tracing::info!("waiting realtime logs");
        let rpc_client = Arc::clone(&rpc_client);
        let mut merged_stream = poll_fn(move |cx| {
            // Poll fetch_futures
            let poll_next_unpin = fetch_futures.poll_next_unpin(cx);
            match poll_next_unpin {
                Poll::Ready(Some(fetch_result)) => {
                    cx.waker().wake_by_ref();
                    return Poll::Ready(Some(fetch_result))
                }
                Poll::Ready(None) | Poll::Pending => {} // No more futures to poll
            }

            // Poll ws_stream
            match Pin::new(&mut ws_stream).poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if let Ok(sig) = Signature::from_str(&item.value.signature) {
                        // Push fetch_logs future into fetch_futures
                        let rpc_client = Arc::clone(&rpc_client);
                        let fetch_future = async move {
                            let log_item = fetch_logs(sig, &rpc_client).await?;
                            tracing::info!(item = ?log_item.signature, "found tx");
                            eyre::Result::Ok(log_item)
                        };
                        fetch_futures.push(fetch_future);
                    }
                    // We return Pending here because the actual result will come from
                    // fetch_futures
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Poll::Ready(None) => {
                    // WS stream ended
                    tracing::warn!("websocket stream exited");
                    Poll::Ready(None)
                }
                Poll::Pending => Poll::Pending,
            }
        });

        // Process the merged stream
        while let Option::<eyre::Result<SolanaTransaction>>::Some(result) =
            merged_stream.next().await
        {
            match result {
                Ok(log_item) => {
                    // Send the fetched log item
                    signature_sender.send(log_item).await?;
                }
                Err(err) => {
                    // Handle error in fetch_logs
                    tracing::error!(?err, "Error in merged stream");
                    continue 'outer;
                }
            }
        }
    }
}

type UnsubscribeFn = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;
async fn init_logs_subscribe(
    client: &solana_client::nonblocking::pubsub_client::PubsubClient,
    pubkey: solana_sdk::pubkey::Pubkey,
) -> PubsubClientResult<(BoxStream<'_, Response<RpcLogsResponse>>, UnsubscribeFn)> {
    client
        .logs_subscribe(
            // we subscribe to all txs that contain the gateway program id
            RpcTransactionLogsFilter::Mentions(vec![pubkey.to_string()]),
            RpcTransactionLogsConfig {
                commitment: Some(CommitmentConfig::finalized()),
            },
        )
        .await
}

// We could use `futures::stream::select` here, but it only completes when both streams
// complete. As we want to complete when either stream completes, we need a custom
// combinator.
fn round_robin_two_streams<S1, S2>(mut s1: S1, mut s2: S2) -> impl Stream<Item = S1::Item>
where
    S1: Stream + Unpin,
    S2: Stream<Item = S1::Item> + Unpin,
{
    let mut poll_first = false;

    poll_fn(move |cx| {
        // Flip which stream is polled first.
        poll_first = !poll_first;

        // Helper to poll one stream first, then the other.
        fn poll_round<AS, BS>(
            cx: &mut Context<'_>,
            a: &mut AS,
            b: &mut BS,
        ) -> Poll<Option<AS::Item>>
        where
            AS: Stream + Unpin,
            BS: Stream<Item = AS::Item> + Unpin,
        {
            match Pin::new(a).poll_next(cx) {
                Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => match Pin::new(b).poll_next(cx) {
                    Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Pending => Poll::Pending,
                },
            }
        }

        if poll_first {
            poll_round(cx, &mut s1, &mut s2)
        } else {
            poll_round(cx, &mut s2, &mut s1)
        }
    })
}

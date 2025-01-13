use std::sync::Arc;
use std::time::Duration;

use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};
use chrono::DateTime;
use eyre::OptionExt as _;
use futures::SinkExt as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Signature;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_transaction_status::{EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding};
use tokio::task::JoinSet;

use super::{MessageSender, SolanaTransaction};

#[tracing::instrument(skip_all)]
pub(crate) async fn fetch_and_send(
    commitment: CommitmentConfig,
    fetched_signatures: impl Iterator<Item = Signature>,
    rpc_client: Arc<RpcClient>,
    signature_sender: MessageSender,
) -> Result<(), eyre::Error> {
    let mut log_fetch_js = JoinSet::new();
    for signature in fetched_signatures {
        log_fetch_js.spawn({
            let rpc_client = Arc::clone(&rpc_client);
            let mut signature_sender = signature_sender.clone();
            async move {
                let tx = fetch_logs(commitment, signature, &rpc_client).await?;
                signature_sender.send(tx).await?;
                Result::<_, eyre::Report>::Ok(())
            }
        });
    }
    while let Some(item) = log_fetch_js.join_next().await {
        if let Err(err) = item? {
            tracing::warn!(?err, "error when parsing tx");
        }
    }
    Ok(())
}

/// Fetch the logs of a Solana transaction.
///
/// # Errors
///
/// - If request to the Solana RPC fails
/// - If the metadata is not included with the logs
/// - If the logs are not included
/// - If the transaction was not successful
#[tracing::instrument(skip_all, fields(signtaure))]
pub async fn fetch_logs(
    commitment: CommitmentConfig,
    signature: Signature,
    rpc_client: &RpcClient,
) -> eyre::Result<SolanaTransaction> {
    use solana_client::rpc_config::RpcTransactionConfig;
    let config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Binary),
        commitment: Some(commitment),
        max_supported_transaction_version: None,
    };

    // let operation = || async {
    //     rpc_client
    //         .get_transaction_with_config(&signature, config)
    //         .await
    //         .inspect_err(|error| error!(%error))
    //         .map_err(|error| match *error.kind() {
    //         //     solana_client::client_error::ClientErrorKind::Io(error) => todo!(),
    //         //     solana_client::client_error::ClientErrorKind::Reqwest(error) => todo!(),
    //         //     solana_client::client_error::ClientErrorKind::Middleware(error) => todo!(),
    //         //     solana_client::client_error::ClientErrorKind::RpcError(rpc_error) => todo!(),
    //         //     solana_client::client_error::ClientErrorKind::SerdeJson(error) => todo!(),
    //         //     solana_client::client_error::ClientErrorKind::SigningError(signer_error) =>
    // todo!(),         //
    // solana_client::client_error::ClientErrorKind::TransactionError(transaction_error) => todo!(),
    //         //     solana_client::client_error::ClientErrorKind::Custom(_) => todo!(),
    //         // }
    //             // Retry on networking-io related errors
    //             // Io(_) | Reqwest(_) => backoff::Error::transient(error),
    //             // // Fail instantly on other errors
    //             // SerdeJson(_) | RpcError(_) | SigningError(_) | TransactionError(_) | Custom(_)
    // => {             //     backoff::Error::permanent(error)
    //             // }
    //             // Middleware(_) => backoff::Error::permanent(error),
    //         })
    // };
    // let res = backoff::future::retry(
    //     ExponentialBackoffBuilder::new()
    //         .with_max_interval(Duration::from_secs(60))
    //         .build(),
    //     operation,
    // );
    let EncodedConfirmedTransactionWithStatusMeta {
        slot,
        transaction: transaction_with_meta,
        block_time,
    } = rpc_client
        .get_transaction_with_config(&signature, config)
        .await?;

    let meta = transaction_with_meta
        .meta
        .ok_or_eyre("metadata not included with logs")?;

    let OptionSerializer::Some(logs) = meta.log_messages else {
        eyre::bail!("logs not included");
    };
    if meta.err.is_some() {
        eyre::bail!("tx was not successful");
    }

    let transaction = SolanaTransaction {
        signature,
        logs,
        slot,
        timestamp: block_time.and_then(|secs| DateTime::from_timestamp(secs, 0)),
        cost_in_lamports: meta.fee,
    };

    Ok(transaction)
}

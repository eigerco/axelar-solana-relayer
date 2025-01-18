use core::time::Duration;
use std::sync::Arc;

use backoff::ExponentialBackoffBuilder;
use chrono::DateTime;
use eyre::OptionExt as _;
use futures::SinkExt as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Signature;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_transaction_status::{EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding};
use tokio::task::JoinSet;

use super::{MessageSender, SolanaTransaction, TxStatus};

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
                let TxStatus::Successful(tx) = tx else {
                    return Ok(());
                };
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
#[tracing::instrument(skip_all, fields(signtaure))]
pub async fn fetch_logs(
    commitment: CommitmentConfig,
    signature: Signature,
    rpc_client: &RpcClient,
) -> eyre::Result<TxStatus> {
    use solana_client::rpc_config::RpcTransactionConfig;
    let config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Binary),
        commitment: Some(commitment),
        max_supported_transaction_version: None,
    };

    let operation = || async {
        rpc_client
            .get_transaction_with_config(&signature, config)
            .await
            .inspect_err(|error| tracing::error!(%error))
            .map_err(backoff::Error::transient)
    };
    let EncodedConfirmedTransactionWithStatusMeta {
        slot,
        transaction: transaction_with_meta,
        block_time,
    } = backoff::future::retry(
        ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(Duration::from_secs(10)))
            .build(),
        operation,
    )
    .await?;

    // parse the provided accounts
    let message = transaction_with_meta.transaction.decode().unwrap().message;
    let ixs = message.instructions();
    let accounts = message.static_account_keys();
    let mut instruction_accounts = Vec::with_capacity(ixs.len());
    for ix in ixs {
        let mut tmp_accounts = Vec::with_capacity(ix.accounts.len());
        let program_id = accounts[ix.program_id_index as usize];
        for account_idx in ix.accounts.iter().copied() {
            tmp_accounts.push(accounts[account_idx as usize]);
        }
        instruction_accounts.push((program_id, tmp_accounts));
    }

    let meta = transaction_with_meta
        .meta
        .ok_or_eyre("metadata not included with logs")?;

    let OptionSerializer::Some(logs) = meta.log_messages else {
        eyre::bail!("logs not included");
    };

    let tx = SolanaTransaction {
        signature,
        logs,
        slot,
        timestamp: block_time.and_then(|secs| DateTime::from_timestamp(secs, 0)),
        cost_in_lamports: meta.fee,
        instruction_accounts,
    };

    let tx = match meta.err {
        Some(error) => TxStatus::Failed { tx, error },
        None => TxStatus::Successful(tx),
    };
    Ok(tx)
}

use core::str::FromStr as _;
use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

use super::{MessageSender, SolanaTransaction};
use crate::component::log_processor;
use crate::config::MissedSignatureCatchupStrategy;

#[tracing::instrument(skip_all, name = "scan old signatures")]
pub(crate) async fn scan_old_signatures(
    config: &crate::Config,
    signature_sender: &futures::channel::mpsc::UnboundedSender<SolanaTransaction>,
    rpc_client: &Arc<RpcClient>,
) -> Result<Option<Signature>, eyre::Error> {
    let latest_processed_signature = match (
        &config.missed_signature_catchup_strategy,
        config.latest_processed_signature,
    ) {
        (&MissedSignatureCatchupStrategy::None, None) => {
            tracing::info!(
                "Starting from the latest available signature as no catch-up is configured and no latest signature is known."
            );
            None
        }
        (&MissedSignatureCatchupStrategy::None, Some(latest_signature)) => {
            tracing::info!(
                ?latest_signature,
                "Starting from the latest processed signature",
            );
            Some(latest_signature)
        }
        (
            &MissedSignatureCatchupStrategy::UntilSignatureReached(target_signature),
            latest_signature,
        ) => {
            tracing::info!(
                ?target_signature,
                ?latest_signature,
                "Catching up missed signatures until target signature",
            );
            fetch_batches_in_range(
                config,
                Arc::clone(rpc_client),
                signature_sender,
                Some(target_signature),
                latest_signature,
            )
            .await?
        }
        (&MissedSignatureCatchupStrategy::UntilBeginning, latest_signature) => {
            tracing::info!(
                ?latest_signature,
                "Catching up all missed signatures starting from",
            );
            fetch_batches_in_range(
                config,
                Arc::clone(rpc_client),
                signature_sender,
                None,
                latest_signature,
            )
            .await?
        }
    };

    Ok(latest_processed_signature)
}

#[tracing::instrument(skip_all, err)]
pub(crate) async fn fetch_batches_in_range(
    config: &crate::Config,
    rpc_client: Arc<RpcClient>,
    signature_sender: &MessageSender,
    t1_signature: Option<Signature>,
    mut t2_signature: Option<Signature>,
) -> Result<Option<Signature>, eyre::Error> {
    let mut interval = tokio::time::interval(config.tx_scan_poll_period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Track the chronologically youngest t2 that we've seen
    let mut chronologically_newest_signature = t2_signature;

    loop {
        let mut fetcher = SignatureRangeFetcher {
            t1: t1_signature,
            t2: t2_signature,
            rpc_client: Arc::clone(&rpc_client),
            address: config.gateway_program_address,
            signature_sender: signature_sender.clone(),
        };

        let res = fetcher.fetch().await?;
        match res {
            FetchingState::Completed => break,
            FetchingState::FetchAgain { new_t2 } => {
                // Set the newest signature only once
                if chronologically_newest_signature.is_none() {
                    chronologically_newest_signature = Some(new_t2);
                }

                // Update t2 to fetch older signatures
                t2_signature = Some(new_t2);
            }
        }
        // Sleep to avoid rate limiting
        interval.tick().await;
    }
    Ok(chronologically_newest_signature)
}

enum FetchingState {
    Completed,
    FetchAgain { new_t2: Signature },
}

struct SignatureRangeFetcher {
    t1: Option<Signature>,
    t2: Option<Signature>,
    rpc_client: Arc<RpcClient>,
    address: Pubkey,
    signature_sender: MessageSender,
}

impl SignatureRangeFetcher {
    #[tracing::instrument(skip(self), fields(t1 = ?self.t1, t2 = ?self.t2))]
    async fn fetch(&mut self) -> eyre::Result<FetchingState> {
        /// The maximum allowed by the Solana RPC is 1000. We use a smaller limit to reduce load.
        const LIMIT: usize = 10;

        tracing::debug!(?self.address, "Fetching signatures");

        let fetched_signatures = self
            .rpc_client
            .get_signatures_for_address_with_config(
                &self.address,
                GetConfirmedSignaturesForAddress2Config {
                    // start searching backwards from this transaction signature. If not provided
                    // the search starts from the top of the highest max confirmed block.
                    before: self.t2,
                    // search until this transaction signature, if found before limit reached
                    until: self.t1,
                    limit: Some(LIMIT),
                    commitment: Some(CommitmentConfig::finalized()),
                },
            )
            .await?;

        let total_signatures = fetched_signatures.len();
        tracing::info!(total_signatures, "Fetched new set of signatures");

        if fetched_signatures.is_empty() {
            tracing::info!("No more signatures to fetch");
            return Ok(FetchingState::Completed);
        }

        let (chronologically_oldest_signature, _) =
            match (fetched_signatures.last(), fetched_signatures.first()) {
                (Some(oldest), Some(newest)) => (
                    Signature::from_str(&oldest.signature)?,
                    Signature::from_str(&newest.signature)?,
                ),
                _ => return Ok(FetchingState::Completed),
            };

        let fetched_signatures_iter = fetched_signatures
            .into_iter()
            .flat_map(|status| Signature::from_str(&status.signature))
            .rev();

        // Fetch logs and send them via the sender
        log_processor::fetch_and_send(
            fetched_signatures_iter,
            Arc::clone(&self.rpc_client),
            self.signature_sender.clone(),
        )
        .await?;

        if total_signatures < LIMIT {
            tracing::info!("Fetched all available signatures in the range");
            Ok(FetchingState::Completed)
        } else {
            tracing::info!("More signatures available, continuing fetch");
            Ok(FetchingState::FetchAgain {
                new_t2: chronologically_oldest_signature,
            })
        }
    }
}

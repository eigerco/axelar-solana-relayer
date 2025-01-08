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

/// Fetches events in range. Processes them "backwards" in time.
/// Fetching the events in range: batch(t1..t2), batch(t2..t3), ..
///
/// The fetching will be done for: gateway and gas service programs until both programs don't return
/// anu more events.
///
/// The fetching of events stops after *both* programs have no more events to report.
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
    'a: loop {
        let mut all_completed = true;
        let mut newest_sig = chronologically_newest_signature;

        for program_to_monitor in [
            config.gateway_program_address,
            config.gas_service_config_pda,
        ] {
            let mut fetcher = SignatureRangeFetcher {
                t1: t1_signature,
                t2: t2_signature,
                rpc_client: Arc::clone(&rpc_client),
                address: program_to_monitor,
                signature_sender: signature_sender.clone(),
            };

            let res = fetcher.fetch().await?;
            match res {
                FetchingState::Completed => {
                    // This program finished, but we still need to process the other one
                }
                FetchingState::FetchAgain { new_t2 } => {
                    all_completed = false;
                    if newest_sig.is_none() {
                        newest_sig = Some(new_t2);
                    }
                }
            }

            // Avoid rate limiting
            interval.tick().await;
        }

        // After processing both programs:
        if all_completed {
            // Both completed, break out
            break 'a;
        }

        t2_signature = newest_sig;
        chronologically_newest_signature = newest_sig;
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

#[cfg(test)]
mod test {
    use std::path::PathBuf;
    use std::time::Duration;

    use axelar_solana_gateway_test_fixtures::base::TestFixture;
    use axelar_solana_gateway_test_fixtures::SolanaAxelarIntegrationMetadata;
    use solana_sdk::account::AccountSharedData;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer;
    use solana_sdk::{bpf_loader_upgradeable, system_program};
    use solana_test_validator::{TestValidator, UpgradeableProgramInfo};
    use tokio::task::JoinSet;

    use super::*;

    /// Return the [`PathBuf`] that points to the `[repo]` folder
    #[must_use]
    pub fn workspace_root_dir() -> PathBuf {
        let dir = std::env::var("CARGO_MANIFEST_DIR")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
        PathBuf::from(dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn can_initialize_gateway() {
        let mut fixture = setup().await;
    }

    #[tokio::test]
    async fn signature_range_fetcher() {
        let mut fixture = setup().await;
        // init gas config
        let gas_service_upgr_auth = fixture.payer.insecure_clone();
        let gas_config = fixture.setup_default_gas_config(gas_service_upgr_auth);
        fixture.init_gas_config(&gas_config).await.unwrap();

        // init memo program
        let counter_pda = axelar_solana_memo_program::get_counter_pda(&fixture.gateway_root_pda);
        let ix = axelar_solana_memo_program::instruction::initialize(
            &fixture.payer.pubkey(),
            &fixture.gateway_root_pda,
            &counter_pda,
        )
        .unwrap();
        fixture.send_tx(&[ix]).await.unwrap();

        // solana memo program to evm raw message (3)
        for i in 0..3 {
            let ix = axelar_solana_memo_program::instruction::call_gateway_with_memo(
                &fixture.gateway_root_pda,
                &counter_pda.0,
                format!("msg {i}"),
                "evm".to_string(),
                "0xdeadbeef".to_string(),
                &axelar_solana_gateway::id(),
            )
            .unwrap();
            fixture.send_tx(&[ix]).await.unwrap();
        }
        // solana memo program + gas service  (3)
        for i in 0..3 {
            let payload = format!("msg {i}");
            let payload_hash = solana_sdk::keccak::hashv(&[payload.as_str().as_bytes()]).0;
            let destination_address = format!("0xdeadbeef-{i}");
            let ix = axelar_solana_memo_program::instruction::call_gateway_with_memo(
                &fixture.gateway_root_pda,
                &counter_pda.0,
                format!("msg {i}"),
                "evm".to_string(),
                destination_address.clone(),
                &axelar_solana_gateway::id(),
            )
            .unwrap();
            let gas_ix =
                axelar_solana_gas_service::instructions::pay_native_for_contract_call_instruction(
                    &axelar_solana_gas_service::id(),
                    &fixture.payer.pubkey(),
                    &gas_config.config_pda,
                    "evm".to_string(),
                    destination_address.clone(),
                    payload_hash,
                    Pubkey::new_unique(),
                    vec![],
                    5000,
                )
                .unwrap();
            fixture.send_tx(&[ix, gas_ix]).await.unwrap();
        }
        // gas service to fund extra 3 msgs  (3)
        for _ in 0..3 {
            let gas_ix = axelar_solana_gas_service::instructions::add_native_gas_instruction(
                &axelar_solana_gas_service::id(),
                &fixture.payer.pubkey(),
                &gas_config.config_pda,
                [42; 64],
                123,
                5000,
                Pubkey::new_unique(),
            )
            .unwrap();
            fixture.send_tx(&[gas_ix]).await.unwrap();
        }
    }

    pub async fn setup() -> SolanaAxelarIntegrationMetadata {
        use axelar_solana_gateway_test_fixtures::SolanaAxelarIntegration;
        use solana_test_validator::TestValidatorGenesis;
        let mut validator = TestValidatorGenesis::default();
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
        let mut fixture =
            TestFixture::new_test_validator(validator, Duration::from_millis(500)).await;
        fixture.payer = upgrade_authority.insecure_clone();

        let mut fixture = SolanaAxelarIntegration::builder()
            .initial_signer_weights(vec![42])
            .fixture(fixture)
            .build()
            .stetup_without_deployment(upgrade_authority);

        fixture.initialize_gateway_config_account().await.unwrap();
        fixture
    }
}

use core::str::FromStr as _;
use std::sync::Arc;

use eyre::Context as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use tracing::Instrument as _;

use super::{MessageSender, SolanaTransaction};
use crate::component::log_processor;
use crate::config::MissedSignatureCatchupStrategy;

/// Scan old signatures based on the configured catch-up strategy
///
/// # Returns
/// The latest processed signature, if any was found.
/// Latest -- chronologically oldest
#[tracing::instrument(skip_all)]
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
///
/// # Returns
/// The chronologically newest/latest signature
#[tracing::instrument(skip_all, err)]
#[expect(
    clippy::unreachable,
    reason = "unreachable code in here, but the type system doesn't know that"
)]
pub(crate) async fn fetch_batches_in_range(
    config: &crate::Config,
    rpc_client: Arc<RpcClient>,
    signature_sender: &MessageSender,
    t1_signature: Option<Signature>,
    t2_signature: Option<Signature>,
) -> Result<Option<Signature>, eyre::Error> {
    let mut interval = tokio::time::interval(config.tx_scan_poll_period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;

    let mut chronologically_newest_sig = t2_signature;
    let mut slot = None;

    for program_to_monitor in [
        config.gateway_program_address,
        config.gas_service_config_pda,
    ] {
        // keep using the arg provided t2 for the initial loop
        let mut t2_in_loop = t2_signature;
        loop {
            let mut fetcher = SignatureRangeFetcher {
                t1: t1_signature,
                t2: t2_in_loop,
                rpc_client: Arc::clone(&rpc_client),
                address: program_to_monitor,
                signature_sender: signature_sender.clone(),
                commitment: config.commitment,
            };

            let fetch_result = fetcher.fetch().in_current_span().await?;
            let newest_signature = match fetch_result {
                FetchingState::Completed { newest_signature } => newest_signature,
                FetchingState::FetchAgain {
                    new_t2,
                    newest_signature,
                } => {
                    t2_in_loop = Some(new_t2.0); // Always update the t2 to use in the loop
                    newest_signature
                }
            };

            match (newest_signature, chronologically_newest_sig, slot) {
                (Some((new_t2, new_slot)), None, None) => {
                    chronologically_newest_sig = Some(new_t2);
                    slot = Some(new_slot);
                }
                (Some((_new_t2, _new_slot)), Some(_), None) => {
                    // a fetched new_t2 will never be newer than a t2 provided in args
                    // this is noop
                }
                (Some((new_t2, new_slot)), Some(_), Some(old_slot)) => {
                    // `old_slot` is only available if it's been set in a past step
                    if old_slot < new_slot {
                        chronologically_newest_sig = Some(new_t2);
                    }
                }
                (Some(_), None, Some(_)) => unreachable!(),
                (None, _, _) => {
                    // no op
                }
            };

            // go to the next itereation if we've fetched all messages for this Pubkey
            if matches!(fetch_result, FetchingState::Completed { .. }) {
                break;
            }

            // Avoid rate-limiting
            interval.tick().await;
        }
    }

    // Final updated signature is our newest
    Ok(chronologically_newest_sig)
}

#[derive(Debug, PartialEq)]
enum FetchingState {
    Completed {
        newest_signature: Option<(Signature, u64)>,
    },
    FetchAgain {
        new_t2: (Signature, u64),
        newest_signature: Option<(Signature, u64)>,
    },
}

#[derive(Clone)]
struct SignatureRangeFetcher {
    t1: Option<Signature>,
    t2: Option<Signature>,
    rpc_client: Arc<RpcClient>,
    address: Pubkey,
    signature_sender: MessageSender,
    commitment: CommitmentConfig,
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
                    commitment: Some(self.commitment),
                },
            )
            .await
            .context("fetching signatures with address")?;

        let total_signatures = fetched_signatures.len();

        if fetched_signatures.is_empty() {
            tracing::info!("No more signatures to fetch");
            return Ok(FetchingState::Completed {
                newest_signature: None,
            });
        }

        let chronologically_oldest_signature = fetched_signatures
            .last()
            .map(|x| {
                (
                    Signature::from_str(&x.signature).expect("rpc will return valid signatures"),
                    // other variables besides `slot` are not available
                    x.slot,
                )
            })
            .expect("we checked that the vec is not empty");
        let chronologically_newest_signature = fetched_signatures
            .first()
            .map(|x| {
                (
                    Signature::from_str(&x.signature).expect("rpc will return valid signatures"),
                    // other variables besides `slot` are not available
                    x.slot,
                )
            })
            .expect("we checked that the vec is not empty");

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
            tracing::info!(
                ?chronologically_newest_signature,
                "Fetched all available signatures in the range"
            );
            Ok(FetchingState::Completed {
                newest_signature: Some(chronologically_newest_signature),
            })
        } else {
            tracing::info!(
                ?chronologically_oldest_signature,
                ?chronologically_newest_signature,
                "More signatures available, continuing fetch"
            );
            Ok(FetchingState::FetchAgain {
                new_t2: chronologically_oldest_signature,
                newest_signature: Some(chronologically_newest_signature),
            })
        }
    }
}

#[cfg(test)]
#[expect(clippy::unimplemented, reason = "needed for the test")]
pub(crate) mod test {
    use core::time::Duration;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use axelar_solana_gateway_test_fixtures::base::TestFixture;
    use axelar_solana_gateway_test_fixtures::gateway::make_verifiers_with_quorum;
    use axelar_solana_gateway_test_fixtures::SolanaAxelarIntegrationMetadata;
    use futures::StreamExt as _;
    use pretty_assertions::assert_eq;
    use solana_rpc::rpc::JsonRpcConfig;
    use solana_rpc::rpc_pubsub_service::PubSubConfig;
    use solana_sdk::account::AccountSharedData;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer as _;
    use solana_sdk::{bpf_loader_upgradeable, system_program};
    use solana_test_validator::UpgradeableProgramInfo;

    use super::*;
    use crate::Config;

    impl FetchingState {
        fn signature(&self) -> Option<Signature> {
            match self {
                Self::Completed { newest_signature } => newest_signature.map(|x| x.0),
                Self::FetchAgain { .. } => unimplemented!(),
            }
        }
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

    #[test_log::test(tokio::test)]
    async fn can_initialize_gateway() {
        let _fixture = setup().await;
    }

    #[test_log::test(tokio::test)]
    async fn signature_range_fetcher() {
        let mut fixture = setup().await;
        let (gas_config, gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let generated_signs =
            generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;

        let rpc_client_url = match &fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                validator,
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

        let (tx, _rx) = futures::channel::mpsc::unbounded();
        let fetcher = SignatureRangeFetcher {
            t1: None,
            t2: None,
            rpc_client: Arc::clone(&rpc_client),
            address: Pubkey::new_unique(),
            signature_sender: tx,
            // TestValidator never has a tx in a `finalized state`. When I try to adjust the
            // validator.ticks_per_slot(1) then the test error output is full of panic stack traces
            commitment: CommitmentConfig::confirmed(),
        };

        // test that t1=None and t2=Some works
        {
            let (tx, rx) = futures::channel::mpsc::unbounded();
            let last = *generated_signs.gas_signatures.last().unwrap();
            let mut fetcher = fetcher.clone();
            fetcher.t2 = Some(last);
            fetcher.t1 = None;
            fetcher.signature_sender = tx;
            fetcher.address = gas_config.config_pda;
            let fetch_state = fetcher.fetch().await.unwrap();
            drop(fetcher);

            let mut all_gas_entries = generated_signs
                .gas_signatures
                .iter()
                .chain(generated_signs.memo_and_gas_signatures.iter())
                .copied()
                .collect::<BTreeSet<_>>();
            all_gas_entries.remove(&last);
            all_gas_entries.insert(gas_init_sig);
            let fetched_gas_events = rx.collect::<Vec<_>>().await;
            let fetched_gas_events = fetched_gas_events
                .into_iter()
                .map(|x| x.signature)
                .collect::<BTreeSet<_>>();
            assert!(matches!(fetch_state, FetchingState::Completed { .. }));
            assert_eq!(
                fetch_state.signature(),
                // the t2 entry is not included in the RPC response, the assumption is that we
                // already have processed it hence we know its signature beforehand
                Some(*generated_signs.gas_signatures.iter().nth_back(1).unwrap())
            );
            assert_eq!(
                fetched_gas_events.len(),
                5,
                "the intersection does not include the `last` entry."
            );
            assert_eq!(fetched_gas_events, all_gas_entries,)
        };
        // test that t1=Some and t2=Some works
        {
            let (tx, rx) = futures::channel::mpsc::unbounded();
            let items_in_range = 5;
            let all_memo_signatures_to_fetch = generated_signs
                .memo_signatures
                .iter()
                .chain(generated_signs.memo_and_gas_signatures.iter())
                .copied()
                .take(items_in_range)
                .collect::<Vec<_>>();
            let newest = *all_memo_signatures_to_fetch.last().unwrap();
            let oldest = *all_memo_signatures_to_fetch.first().unwrap();

            let mut fetcher = fetcher.clone();
            fetcher.t2 = Some(newest);
            fetcher.t1 = Some(oldest);
            fetcher.signature_sender = tx;
            fetcher.address = axelar_solana_memo_program::id();
            let fetch_state = fetcher.fetch().await.unwrap();
            drop(fetcher);
            assert!(matches!(fetch_state, FetchingState::Completed { .. }));
            assert_eq!(
                fetch_state.signature(),
                // the t2 entry is not included in the RPC response, the assumption is that we
                // already have processed it hence we know its signature beforehand
                Some(*all_memo_signatures_to_fetch.iter().nth_back(1).unwrap())
            );

            // the t2 entry is not included in the RPC response, the assumption is that we already
            // have processed it hence we know its signature beforehand
            let fetched = rx
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|x| x.signature)
                .collect::<BTreeSet<_>>();
            assert_eq!(
                fetched.len(),
                items_in_range - 2,
                "does not include the `oldest` and `newest` entry."
            );
            let mut all_memo_signatures_to_fetch = all_memo_signatures_to_fetch
                .into_iter()
                .collect::<BTreeSet<_>>();
            all_memo_signatures_to_fetch.remove(&newest);
            all_memo_signatures_to_fetch.remove(&oldest);
            assert_eq!(fetched, all_memo_signatures_to_fetch,);
        }
    }

    #[test_log::test(tokio::test)]
    async fn fetch_large_range_of_signatures() {
        let mut fixture = setup().await;
        let (gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let generated_signs_set_1 =
            generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;
        let generated_signs_set_2 =
            generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;
        let generated_signs_set_3 =
            generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;

        let rpc_client_url = match &fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                validator,
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

        let config = Config {
            gateway_program_address: axelar_solana_gateway::id(),
            gas_service_config_pda: gas_config.config_pda,
            solana_ws: rpc_client_url.parse().unwrap(),
            missed_signature_catchup_strategy: MissedSignatureCatchupStrategy::UntilBeginning,
            latest_processed_signature: None,
            tx_scan_poll_period: Duration::from_millis(1),
            commitment: CommitmentConfig::confirmed(),
        };
        let (tx, rx) = futures::channel::mpsc::unbounded();

        let latest_sig = scan_old_signatures(&config, &tx, &rpc_client)
            .await
            .unwrap();
        let all_items_seq = [
            generated_signs_set_1.flatten_sequentially(),
            generated_signs_set_2.flatten_sequentially(),
            generated_signs_set_3.flatten_sequentially(),
        ]
        .concat();
        dbg!(&all_items_seq);
        assert_eq!(latest_sig, all_items_seq.last().copied());
        drop(tx);
        let fetched = rx.map(|x| x.signature).collect::<BTreeSet<_>>().await;

        let all_items_btree = all_items_seq.clone().into_iter().collect::<BTreeSet<_>>();
        assert_eq!(
            fetched
                .intersection(&all_items_btree)
                .copied()
                .collect::<BTreeSet<_>>(),
            all_items_btree,
            "expect to have fetched every single item"
        );
        assert_eq!(all_items_btree.len(), all_items_seq.len());
        assert_eq!(
            fetched.len(),
            all_items_seq.len() + 2,
            "adding init / deployment tx counts in there"
        );
    }

    #[derive(Debug)]
    pub(crate) struct GenerateTestSolanaDataResult {
        pub memo_signatures: Vec<Signature>,
        pub memo_and_gas_signatures: Vec<Signature>,
        pub gas_signatures: Vec<Signature>,
    }

    impl GenerateTestSolanaDataResult {
        pub(crate) fn flatten_sequentially(&self) -> Vec<Signature> {
            [
                self.memo_signatures.clone(),
                self.memo_and_gas_signatures.clone(),
                self.gas_signatures.clone(),
            ]
            .concat()
        }
    }

    pub(crate) async fn generate_test_solana_data(
        fixture: &mut SolanaAxelarIntegrationMetadata,
        counter_pda: (Pubkey, u8),
        gas_config: &axelar_solana_gateway_test_fixtures::gas_service::GasServiceUtils,
    ) -> GenerateTestSolanaDataResult {
        // solana memo program to evm raw message (3 logs)
        let mut memo_signatures = vec![];
        for i in 0..3 {
            let ix = axelar_solana_memo_program::instruction::call_gateway_with_memo(
                &fixture.gateway_root_pda,
                &counter_pda.0,
                format!("msg {i}"),
                "evm".to_owned(),
                "0xdeadbeef".to_owned(),
                &axelar_solana_gateway::id(),
            )
            .unwrap();
            let sig = fixture.send_tx_with_signatures(&[ix]).await.unwrap().0[0];
            memo_signatures.push(sig);
        }
        // solana memo program + gas service  (3 logs)
        let mut memo_and_gas_signatures = vec![];
        for i in 0..3 {
            let payload = format!("msg {i}");
            let payload_hash = solana_sdk::keccak::hashv(&[payload.as_bytes()]).0;
            let destination_address = format!("0xdeadbeef-{i}");
            let ix = axelar_solana_memo_program::instruction::call_gateway_with_memo(
                &fixture.gateway_root_pda,
                &counter_pda.0,
                format!("msg {i}"),
                "evm".to_owned(),
                destination_address.clone(),
                &axelar_solana_gateway::id(),
            )
            .unwrap();
            let gas_ix =
                axelar_solana_gas_service::instructions::pay_native_for_contract_call_instruction(
                    &axelar_solana_gas_service::id(),
                    &fixture.payer.pubkey(),
                    &gas_config.config_pda,
                    "evm".to_owned(),
                    destination_address.clone(),
                    payload_hash,
                    Pubkey::new_unique(),
                    vec![],
                    5000,
                )
                .unwrap();
            let sig = fixture
                .send_tx_with_signatures(&[ix, gas_ix])
                .await
                .unwrap()
                .0[0];
            memo_and_gas_signatures.push(sig);
        }
        // gas service to fund some arbitrary events from the past (2 logs)
        let mut gas_signatures = vec![];
        for i in 0..2 {
            let gas_ix = axelar_solana_gas_service::instructions::add_native_gas_instruction(
                &axelar_solana_gas_service::id(),
                &fixture.payer.pubkey(),
                &gas_config.config_pda,
                [42 + i; 64],
                123,
                5000,
                Pubkey::new_unique(),
            )
            .unwrap();
            let sig = fixture
                .send_tx_with_signatures(&[gas_ix])
                .await
                .unwrap()
                .0
                .get(0)
                .unwrap()
                .clone();
            gas_signatures.push(sig);
        }
        GenerateTestSolanaDataResult {
            memo_signatures,
            memo_and_gas_signatures,
            gas_signatures,
        }
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
        let gas_init_sig = fixture
            .send_tx_with_signatures(&[ix])
            .await
            .unwrap()
            .0
            .get(0)
            .unwrap()
            .clone();

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
            Duration::from_millis(1000)
        } else {
            Duration::from_millis(500)
        };
        let mut fixture = TestFixture::new_test_validator(validator, forced_sleep).await;
        let init_payer = fixture.payer.insecure_clone();
        fixture.payer = upgrade_authority.insecure_clone();

        let operator = Keypair::new();
        let domain_separator = [42; 32];
        let initial_signers = make_verifiers_with_quorum(&vec![42], 0, 42, domain_separator);
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

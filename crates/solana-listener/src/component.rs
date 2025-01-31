use core::future::Future;
use core::pin::Pin;
use core::str::FromStr as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use file_based_storage::SolanaListenerState;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::TransactionError;
use tracing::{info_span, Instrument as _};

use crate::config;

mod log_processor;
mod signature_batch_scanner;
mod signature_realtime_scanner;

pub use log_processor::fetch_logs;

/// Environment variable that expects a base58 encoded signature
/// of the last processed signature we want to force.
const FORCE_LAST_PROCESSED_SIGNATURE: &str = "FORCE_LAST_PROCESSED_SIGNATURE";

/// Typical message with the produced work.
#[derive(Debug, Clone)]
pub struct SolanaTransaction {
    /// signature of the transaction (id)
    pub signature: Signature,
    /// optional timespamp
    pub timestamp: Option<DateTime<Utc>>,
    /// The raw transaction logs
    pub logs: Vec<String>,
    /// The accounts that were passed to an instructoin.
    /// - first item: the program id
    /// - second item: the Pubkeys provided to the ix
    /// - third item: payload data
    pub ixs: Vec<(Pubkey, Vec<Pubkey>, Vec<u8>)>,
    /// the slot number of the tx
    pub slot: u64,
    /// How expensive was the transaction expressed in lamports
    pub cost_in_lamports: u64,
}

#[derive(Debug, Clone)]
/// Transaction status
pub enum TxStatus {
    /// State when the transaction was successful
    Successful(SolanaTransaction),
    /// State when the transaction was unsuccessful
    Failed {
        /// the raw tx object
        tx: SolanaTransaction,
        /// the actual tx error
        error: TransactionError,
    },
}

impl TxStatus {
    /// Assert that the TX was successful and return the inner object
    ///
    /// # Panics
    /// if the tx had failed
    #[must_use]
    pub const fn tx(&self) -> &SolanaTransaction {
        match self {
            Self::Successful(solana_transaction) => solana_transaction,
            Self::Failed { tx, .. } => tx,
        }
    }

    /// Assert that the TX was successful and return the inner object
    ///
    /// # Panics
    /// if the tx had failed
    #[must_use]
    #[expect(clippy::panic, reason = "necessary for this implementation")]
    pub fn unwrap(self) -> SolanaTransaction {
        match self {
            Self::Successful(solana_transaction) => solana_transaction,
            Self::Failed { .. } => panic!(),
        }
    }

    /// Assert that the tx was unsuccessful and return the inner object
    ///
    /// # Panics
    /// if the case was successful
    #[must_use]
    #[expect(clippy::panic, reason = "necessary for this implementation")]
    pub fn unwrap_err(self) -> (SolanaTransaction, TransactionError) {
        match self {
            Self::Successful(..) => panic!(),
            Self::Failed { tx, error } => (tx, error),
        }
    }
}

pub(crate) type MessageSender = futures::channel::mpsc::UnboundedSender<SolanaTransaction>;

/// The listener component that has the core functionality:
/// - monitor (poll) the solana blockchain for new signatures coming from the gateway program
/// - fetch the actual event data from the provided signature
/// - forward the tx event data to the `SolanaListenerClient`
pub struct SolanaListener<ST>
where
    ST: SolanaListenerState,
{
    config: config::Config,
    rpc_client: Arc<RpcClient>,
    sender: MessageSender,
    state: ST,
}

/// Utility client used for communicating with the `SolanaListener` instance
#[derive(Debug)]
pub struct SolanaListenerClient {
    /// Receive transaction messagese from `SolanaListener` instance
    pub log_receiver: futures::channel::mpsc::UnboundedReceiver<SolanaTransaction>,
}

impl<ST: SolanaListenerState> relayer_engine::RelayerComponent for SolanaListener<ST> {
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

impl<ST: SolanaListenerState> SolanaListener<ST> {
    /// Instantiate a new `SolanaListener` using the pre-configured configuration.
    ///
    /// The returned variable also returns a helper client that encompasses ways to communicate with
    /// the underlying `SolanaListener` instance.
    #[must_use]
    pub fn new(
        config: config::Config,
        rpc_client: Arc<RpcClient>,
        state: ST,
    ) -> (Self, SolanaListenerClient) {
        let (tx_outgoing, rx_outgoing) = futures::channel::mpsc::unbounded();
        let this = Self {
            config,
            rpc_client,
            sender: tx_outgoing,
            state,
        };
        let client = SolanaListenerClient {
            log_receiver: rx_outgoing,
        };
        (this, client)
    }

    #[tracing::instrument(skip_all, name = "Solana Listener")]
    pub(crate) async fn process_internal(self) -> eyre::Result<()> {
        if let Ok(force_last_processed_signature) = std::env::var(FORCE_LAST_PROCESSED_SIGNATURE) {
            if !force_last_processed_signature.is_empty() {
                tracing::warn!(
                    "forcing last processed signature to {}",
                    force_last_processed_signature
                );
                self.state
                    .set_latest_processed_signature(Signature::from_str(
                        &force_last_processed_signature,
                    )?)?;
            }
        }

        let latest_processed_signature = self.state.latest_processed_signature();

        // Fetch missed batches
        let latest_signature = signature_batch_scanner::fetch_batches_in_range(
            &self.config,
            Arc::clone(&self.rpc_client),
            &self.sender,
            latest_processed_signature,
            None,
        )
        .instrument(info_span!("fetching missed signatures"))
        .in_current_span()
        .await?;

        if let Some(latest_signature) = latest_signature {
            // Send the latest signature
            self.state
                .set_latest_processed_signature(latest_signature)?;
        }

        // we start processing realtime logs
        signature_realtime_scanner::process_realtime_logs(
            self.config,
            self.rpc_client,
            self.sender,
            self.state,
        )
        .await?;

        eyre::bail!("listener crashed");
    }
}

#[cfg(test)]
mod tests {
    use core::future;
    use core::time::Duration;
    use std::collections::BTreeSet;
    use std::env;
    use std::path::Path;

    use file_based_storage::MemmapState;
    use futures::StreamExt as _;
    use pretty_assertions::{assert_eq, assert_ne};
    use solana_sdk::commitment_config::CommitmentConfig;

    use crate::component::signature_batch_scanner::test::{
        generate_test_solana_data, setup, setup_aux_contracts,
    };
    use crate::{Config, SolanaListener};

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    #[expect(clippy::unimplemented, reason = "needed for the test")]
    async fn can_receive_realtime_tx_events() {
        // 1. setup
        let mut fixture = setup().await;
        let (gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        // 2. generate test data
        let generated_signs_set_1 =
            generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;

        // 3. setup client
        let (rpc_client_url, pubsub_url) = match fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                ref validator,
                ..
            } => (validator.rpc_url(), validator.rpc_pubsub_url()),
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::ProgramTest { .. } => {
                unimplemented!()
            }
        };
        let rpc_client =
            retrying_solana_http_sender::new_client(&retrying_solana_http_sender::Config {
                max_concurrent_rpc_requests: 10,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });
        let config = Config {
            gateway_program_address: axelar_solana_gateway::id(),
            gas_service_config_pda: gas_config.config_pda,
            solana_ws: pubsub_url.parse().unwrap(),
            tx_scan_poll_period: if std::env::var("CI").is_ok() {
                Duration::from_millis(1500)
            } else {
                Duration::from_millis(500)
            },
            commitment: CommitmentConfig::confirmed(),
        };
        let (tx, mut rx) = futures::channel::mpsc::unbounded();

        let state = setup_new_test_state();

        let listener = SolanaListener {
            config,
            rpc_client,
            sender: tx,
            state,
        };
        // 4. start realtime processing
        let processor = tokio::spawn(listener.process_internal());
        {
            // assert that we scan old signatures up to the very beginning of time
            let init_items = generated_signs_set_1.flatten_sequentially();
            let fetched = rx
                .by_ref()
                .map(|x| {
                    assert!(!x.logs.is_empty(), "we expect txs to contain logs");
                    assert_ne!(!x.cost_in_lamports, 0, "tx cost should not be 0");

                    x.signature
                })
                // all init items + the 2 deployment txs + memo_and_gas signatures another time
                // because it's picked up by both WS streams
                .take(
                    init_items
                        .len()
                        .saturating_add(2)
                        .saturating_add(generated_signs_set_1.memo_and_gas.len()),
                )
                .collect::<BTreeSet<_>>()
                .await;
            let init_items_btree = init_items.clone().into_iter().collect::<BTreeSet<_>>();
            let is_finished = processor.is_finished();
            if is_finished {
                processor.await.unwrap().unwrap();
                panic!();
            }
            assert_eq!(
                fetched
                    .intersection(&init_items_btree)
                    .copied()
                    .collect::<BTreeSet<_>>(),
                init_items_btree,
                "expect to have fetched every single item"
            );
        };

        for _ in 0..2_u8 {
            // 4. generate more test data
            let generated_signs_set_2 =
                generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;
            // 5. assert that we receive all the items we generated, and there's no overlap with the
            //    old data
            let new_items = generated_signs_set_2.flatten_sequentially();
            let last_signature = new_items.last().unwrap();
            let mut prev = *last_signature;
            let fetched = rx
                .by_ref()
                .map(|x| {
                    assert!(!x.logs.is_empty(), "we expect txs to contain logs");
                    assert_ne!(!x.cost_in_lamports, 0, "tx cost should not be 0");

                    x.signature
                })
                .take_while(|x| {
                    prev = *x;
                    future::ready(*last_signature != prev)
                })
                .chain(futures::stream::iter([*last_signature]))
                .collect::<BTreeSet<_>>()
                .await;
            let new_items_btree = new_items.clone().into_iter().collect::<BTreeSet<_>>();
            let is_finished = processor.is_finished();
            if is_finished {
                processor.await.unwrap().unwrap();
                panic!();
            }
            assert_eq!(
                fetched
                    .intersection(&new_items_btree)
                    .copied()
                    .collect::<BTreeSet<_>>(),
                new_items_btree,
                "expect to have fetched every single item"
            );
        }
    }

    #[test_log::test(tokio::test(flavor = "current_thread"))]
    async fn can_force_last_processed_signature_via_env_var() {
        // 1. setup
        let mut fixture = setup().await;
        let (gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;

        // 3. setup client
        let (rpc_client_url, pubsub_url) = match fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                ref validator,
                ..
            } => (validator.rpc_url(), validator.rpc_pubsub_url()),
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::ProgramTest { .. } => {
                unimplemented!()
            }
        };
        let rpc_client =
            retrying_solana_http_sender::new_client(&retrying_solana_http_sender::Config {
                max_concurrent_rpc_requests: 10,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });
        let config = Config {
            gateway_program_address: axelar_solana_gateway::id(),
            gas_service_config_pda: gas_config.config_pda,
            solana_ws: pubsub_url.parse().unwrap(),
            tx_scan_poll_period: if std::env::var("CI").is_ok() {
                Duration::from_millis(1500)
            } else {
                Duration::from_millis(500)
            },
            commitment: CommitmentConfig::confirmed(),
        };
        let (tx, mut rx) = futures::channel::mpsc::unbounded();

        let listener = SolanaListener {
            config,
            rpc_client,
            sender: tx,
            state: setup_new_test_state(),
        };

        let generated_signs_set_1 =
            generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;

        env::set_var(
            "FORCE_LAST_PROCESSED_SIGNATURE",
            generated_signs_set_1
                .flatten_sequentially()
                .get(1)
                .expect("to get the second signature as last known one")
                .to_string(),
        ); // Artificially force the last processed signature to exclude the latest one.

        // 4. start realtime processing
        let processor = tokio::spawn(listener.process_internal());

        {
            // assert that we scan old signatures up to the very beginning of time
            let init_items = generated_signs_set_1
                .flatten_sequentially()
                .into_iter()
                .skip(1) // we skip the first one because it's the last processed signature
                .collect::<Vec<_>>();
            let fetched = rx
                .by_ref()
                .map(|x| {
                    assert!(!x.logs.is_empty(), "we expect txs to contain logs");
                    assert_ne!(!x.cost_in_lamports, 0, "tx cost should not be 0");

                    x.signature
                })
                // all init items + the 2 deployment txs + memo_and_gas signatures another time
                // because it's picked up by both WS streams
                .take(
                    init_items
                        .len()
                        .saturating_add(2)
                        .saturating_add(generated_signs_set_1.memo_and_gas.len()),
                )
                .collect::<BTreeSet<_>>()
                .await;
            let init_items_btree = init_items.clone().into_iter().collect::<BTreeSet<_>>();
            let is_finished = processor.is_finished();
            if is_finished {
                processor.await.unwrap().unwrap();
                panic!();
            }
            assert_eq!(
                fetched
                    .intersection(&init_items_btree)
                    .copied()
                    .collect::<BTreeSet<_>>(),
                init_items_btree,
                "expect to have fetched every single item"
            );
        };
    }

    fn setup_new_test_state() -> MemmapState {
        let state_path = Path::new("/tmp").join(format!("state-{}", uuid::Uuid::new_v4()));
        MemmapState::new(state_path).unwrap()
    }
}

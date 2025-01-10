use core::future::Future;
use core::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::Signature;

use crate::config;

mod log_processor;
mod signature_batch_scanner;
mod signature_realtime_scanner;

pub use log_processor::fetch_logs;
/// Typical message with the produced work.
/// Contains the handle to a task that resolves into a
/// [`SolanaTransaction`].
#[derive(Debug, Clone)]
pub struct SolanaTransaction {
    /// signature of the transaction (id)
    pub signature: Signature,
    /// optional timespamp
    pub timestamp: Option<DateTime<Utc>>,
    /// The raw transaction logs
    pub logs: Vec<String>,
    /// the slot number of the tx
    pub slot: u64,
    /// How expensive was the transaction expressed in lamports
    pub cost_in_lamports: u64,
}

pub(crate) type MessageSender = futures::channel::mpsc::UnboundedSender<SolanaTransaction>;

/// The listener component that has the core functionality:
/// - monitor (poll) the solana blockchain for new signatures coming from the gateway program
/// - fetch the actual event data from the provided signature
/// - forward the tx event data to the `SolanaListenerClient`
pub struct SolanaListener {
    config: config::Config,
    rpc_client: Arc<RpcClient>,
    sender: MessageSender,
}

/// Utility client used for communicating with the `SolanaListener` instance
#[derive(Debug)]
pub struct SolanaListenerClient {
    /// Receive transaction messagese from `SolanaListener` instance
    pub log_receiver: futures::channel::mpsc::UnboundedReceiver<SolanaTransaction>,
}

impl relayer_engine::RelayerComponent for SolanaListener {
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

impl SolanaListener {
    /// Instantiate a new `SolanaListener` using the pre-configured configuration.
    ///
    /// The returned variable also returns a helper client that encompasses ways to communicate with
    /// the underlying `SolanaListener` instance.
    #[must_use]
    pub fn new(config: config::Config, rpc_client: Arc<RpcClient>) -> (Self, SolanaListenerClient) {
        let (tx_outgoing, rx_outgoing) = futures::channel::mpsc::unbounded();
        let this = Self {
            config,
            rpc_client,
            sender: tx_outgoing,
        };
        let client = SolanaListenerClient {
            log_receiver: rx_outgoing,
        };
        (this, client)
    }

    #[tracing::instrument(skip_all, name = "Solana Listener")]
    pub(crate) async fn process_internal(self) -> eyre::Result<()> {
        // we fetch potentially missed signatures based on the provided the config
        let latest = signature_batch_scanner::scan_old_signatures(
            &self.config,
            &self.sender,
            &self.rpc_client,
        )
        .await?;

        // we start processing realtime logs
        signature_realtime_scanner::process_realtime_logs(
            self.config,
            latest,
            self.rpc_client,
            self.sender,
        )
        .await?;

        eyre::bail!("listener crashed");
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::collections::BTreeSet;

    use futures::StreamExt as _;
    use pretty_assertions::{assert_eq, assert_ne};
    use solana_sdk::commitment_config::CommitmentConfig;

    use crate::component::signature_batch_scanner::test::{
        generate_test_solana_data, setup, setup_aux_contracts,
    };
    use crate::{Config, MissedSignatureCatchupStrategy, SolanaListener};

    #[test_log::test(tokio::test)]
    async fn can_receive_realtime_tx_events() {
        // 1. setup
        let mut fixture = setup().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let (gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        // 2. generate test data
        let generated_signs_set_1 =
            generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;

        // 3. setup client
        let (rpc_client_url, pubsub_url) = match &fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                validator,
                ..
            } => (validator.rpc_url(), validator.rpc_pubsub_url()),
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
            solana_ws: pubsub_url.parse().unwrap(),
            missed_signature_catchup_strategy: MissedSignatureCatchupStrategy::UntilBeginning,
            latest_processed_signature: None,
            tx_scan_poll_period: Duration::from_millis(500),
            commitment: CommitmentConfig::confirmed(),
        };
        let (tx, mut rx) = futures::channel::mpsc::unbounded();

        let listener = SolanaListener {
            config,
            rpc_client,
            sender: tx,
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
                .take(init_items.len() + 2 + generated_signs_set_1.memo_and_gas_signatures.len())
                .collect::<BTreeSet<_>>()
                .await;
            let init_items_btree = init_items.clone().into_iter().collect::<BTreeSet<_>>();
            assert!(!processor.is_finished());
            dbg!(&init_items_btree);
            assert_eq!(
                fetched
                    .intersection(&init_items_btree)
                    .copied()
                    .collect::<BTreeSet<_>>(),
                init_items_btree,
                "expect to have fetched every single item"
            )
        };

        for _ in 0..2 {
            // 4. generate more test data
            let generated_signs_set_2 =
                generate_test_solana_data(&mut fixture, counter_pda, &gas_config).await;
            // 5. assert that we receive all the items we generated, and there's no overlap with the
            //    old data
            let new_items = generated_signs_set_2.flatten_sequentially();
            let fetched = rx
                .by_ref()
                .map(|x| {
                    assert!(!x.logs.is_empty(), "we expect txs to contain logs");
                    assert_ne!(!x.cost_in_lamports, 0, "tx cost should not be 0");

                    x.signature
                })
                // all the new items
                .take(new_items.len() + generated_signs_set_2.memo_and_gas_signatures.len())
                .collect::<BTreeSet<_>>()
                .await;
            let new_items_btree = new_items.clone().into_iter().collect::<BTreeSet<_>>();
            assert!(!processor.is_finished());
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
}

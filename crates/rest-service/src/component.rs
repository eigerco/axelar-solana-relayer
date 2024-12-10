//! REST service component for the relayer.
use core::future::Future;
use core::net::SocketAddr;
use core::pin::Pin;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::Router;
use relayer_amplifier_api_integration::AmplifierCommandClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use tower_http::trace::TraceLayer;

use crate::{endpoints, Config};

/// The REST service component for the relayer.
#[derive(Debug)]
pub struct RestService {
    router: Router,
    socket_addr: SocketAddr,
}

impl relayer_engine::RelayerComponent for RestService {
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

pub(crate) struct ServiceState {
    chain_name: String,
    rpc_client: Arc<RpcClient>,
    amplifier_client: AmplifierCommandClient,
}

impl ServiceState {
    pub(crate) fn rpc_client(&self) -> Arc<RpcClient> {
        Arc::clone(&self.rpc_client)
    }

    pub(crate) fn chain_name(&self) -> &str {
        &self.chain_name
    }

    pub(crate) const fn amplifier_client(&self) -> &AmplifierCommandClient {
        &self.amplifier_client
    }
}

impl RestService {
    /// Create a new REST service component with the given configuration and RPC client.
    #[must_use]
    pub fn new(
        config: &Config,
        chain_name: String,
        rpc_client: Arc<RpcClient>,
        amplifier_client: AmplifierCommandClient,
    ) -> Self {
        let state = ServiceState {
            chain_name,
            rpc_client,
            amplifier_client,
        };
        let router = Router::new()
            .route(
                endpoints::call_contract_offchain_data::PATH,
                endpoints::call_contract_offchain_data::handlers(),
            )
            .with_state(Arc::new(state))
            .layer(DefaultBodyLimit::max(
                config
                    .call_contract_offchain_data_size_limit
                    .saturating_add(size_of::<endpoints::call_contract_offchain_data::Payload>()),
            ))
            .layer(TraceLayer::new_for_http());

        Self {
            router,
            socket_addr: config.bind_addr,
        }
    }

    async fn process_internal(self) -> eyre::Result<()> {
        let listener = tokio::net::TcpListener::bind(self.socket_addr).await?;
        tracing::info!("REST Service Listening on {}", listener.local_addr()?);
        axum::serve(listener, self.router).await?;

        Ok(())
    }
}

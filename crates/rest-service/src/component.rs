//! REST service component for the relayer.
use core::future::{Future, IntoFuture as _};
use core::net::SocketAddr;
use core::pin::{pin, Pin};
use core::time::Duration;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::{Request, Response};
use axum::Router;
use futures::future::{select, Either};
use relayer_amplifier_api_integration::AmplifierCommandClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use tower_http::trace::TraceLayer;
use tracing::Span;

use crate::{endpoints, Config};

/// The REST service component for the relayer.
#[derive(Debug)]
pub struct RestService {
    router: Router,
    socket_addr: SocketAddr,
    shutdown_rx: tokio::sync::mpsc::Receiver<eyre::Error>,
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
    shutdown_tx: tokio::sync::mpsc::Sender<eyre::Error>,
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

    pub(crate) async fn shutdown(&self, error: eyre::Error) {
        self.shutdown_tx
            .send(error)
            .await
            .expect("Failed to send shutdown signal");
    }
}

impl RestService {
    /// Create a new REST service component.
    #[must_use]
    pub fn new(
        config: &Config,
        chain_name: String,
        rpc_client: Arc<RpcClient>,
        amplifier_client: AmplifierCommandClient,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        let state = ServiceState {
            chain_name,
            rpc_client,
            amplifier_client,
            shutdown_tx,
        };
        let router = Router::new()
            .route(
                endpoints::call_contract_offchain_data::PATH,
                endpoints::call_contract_offchain_data::handlers(),
            )
            .with_state(Arc::new(state))
            .layer(DefaultBodyLimit::max(
                config.call_contract_offchain_data_size_limit,
            ))
            .layer(TraceLayer::new_for_http().make_span_with(|req: &Request<Body>| {
                tracing::info_span!("", method = %req.method(), uri = %req.uri())
            }).on_response(|res: &Response<Body>, latency: Duration, _span: &Span| {
                if res.status().is_server_error() {
                    tracing::error!(status = %res.status().as_u16(), latency = ?latency);
                } else if res.status().is_client_error() {
                    tracing::warn!(status = %res.status().as_u16(), latency = ?latency);
                } else {
                    tracing::info!(status = %res.status().as_u16(), latency = ?latency);
                }
            }).on_failure(()));

        Self {
            router,
            socket_addr: config.bind_addr,
            shutdown_rx,
        }
    }

    async fn process_internal(mut self) -> eyre::Result<()> {
        fn bail() -> Result<(), eyre::Error> {
            Err(eyre::eyre!("REST service unexpectedly shut down"))
        }

        let listener = tokio::net::TcpListener::bind(self.socket_addr).await?;
        tracing::info!("REST Service Listening on {}", listener.local_addr()?);

        match select(
            pin!(self.shutdown_rx.recv()),
            axum::serve(listener, self.router).into_future(),
        )
        .await
        {
            Either::Left((result, _)) => match result {
                Some(err) => {
                    tracing::info!("Shutting down REST service due to internal error");
                    Err(err)
                }
                _ => bail(),
            },
            Either::Right((_, _)) => bail(),
        }
    }
}
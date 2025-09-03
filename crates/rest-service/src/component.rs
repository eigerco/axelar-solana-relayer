//! REST service component for the relayer.
use core::future::Future;
use core::net::SocketAddr;
use core::pin::Pin;
use core::time::Duration;

use axum::body::Body;
use axum::http::{Request, Response};
use axum::Router;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::trace::TraceLayer;
use tracing::Span;

use crate::{endpoints, Config};

/// The REST service component for the relayer.
#[derive(Debug)]
pub struct RestService {
    router: Router,
    socket_addr: SocketAddr,
    shutdown_tx: tokio::sync::mpsc::Sender<Result<(), eyre::Error>>,
    shutdown_rx: tokio::sync::mpsc::Receiver<Result<(), eyre::Error>>,
}

impl relayer_engine::RelayerComponent for RestService {
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

impl RestService {
    /// Create a new REST service component.
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        let router = Router::new()
            .route(
                endpoints::health::PATH,
                endpoints::health::handlers(),
            )
            .layer(ConcurrencyLimitLayer::new(config.max_concurrent_http_requests))
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
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Returns the tx side of the channel used to shutdown the service. Main use case is to
    /// gracefully shutdown the http server during tests to avoid errors binding to the same port.
    #[must_use]
    pub fn shutdown_sender(&self) -> tokio::sync::mpsc::Sender<Result<(), eyre::Error>> {
        self.shutdown_tx.clone()
    }

    async fn process_internal(mut self) -> eyre::Result<()> {
        let listener = tokio::net::TcpListener::bind(self.socket_addr).await?;
        tracing::info!("REST Service Listening on {}", listener.local_addr()?);

        axum::serve(listener, self.router)
            .with_graceful_shutdown(async move {
                match self.shutdown_rx.recv().await {
                    Some(Ok(())) => {
                        tracing::info!("Shutting down REST service gracefully");
                    }
                    Some(Err(error)) => {
                        tracing::error!("Shutting down REST service due to error: {:?}", error);
                    }
                    None => {
                        tracing::warn!("Shutting down REST service due to channel close");
                    }
                }
            })
            .await?;

        Ok(())
    }
}

use core::future::Future;
use core::net::SocketAddrV4;
use core::pin::Pin;

use axum::Router;

/// TODO: Doc
pub struct RestService {
    router: Router,
    socket_addr: SocketAddrV4,
}

impl relayer_engine::RelayerComponent for RestService {
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

impl RestService {
    #[must_use]
    /// TODO: Doc
    pub fn new(config: &crate::Config) -> Self {
        let router = Router::new();

        Self {
            router,
            socket_addr: config.socket_addr,
        }
    }

    async fn process_internal(self) -> eyre::Result<()> {
        let listener = tokio::net::TcpListener::bind(self.socket_addr).await?;
        axum::serve(listener, self.router).await?;

        Ok(())
    }
}

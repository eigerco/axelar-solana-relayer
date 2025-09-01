use core::net::SocketAddr;

use clap::Parser;
use serde::{Deserialize, Serialize};

/// Configuration for the REST service.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize, Parser)]
pub struct Config {
    /// The socket address to bind to.
    #[arg(
        value_name = "REST_SERVICE_BIND_ADDRESS",
        env = "REST_SERVICE_BIND_ADDRESS"
    )]
    pub bind_addr: SocketAddr,
    /// The maximum number of concurrent HTTP requests.
    #[arg(
        value_name = "REST_SERVICE_MAX_CONCURRENT_HTTP_REQUESTS",
        env = "REST_SERVICE_MAX_CONCURRENT_HTTP_REQUESTS"
    )]
    pub max_concurrent_http_requests: usize,
}

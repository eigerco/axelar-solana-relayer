use core::net::SocketAddr;

use clap::Parser;
use serde::{Deserialize, Serialize};

/// Configuration for the REST service.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize, Parser)]
pub struct Config {
    /// The socket address to bind to.
    #[arg(env = "REST_SERVICE_BIND_ADDRESS")]
    pub bind_addr: SocketAddr,
    /// The maximum size of the data in a contract call with offchain data handling.
    #[arg(env = "REST_SERVICE_CALL_CONTRACT_OFFCHAIN_DATA_SIZE_LIMIT")]
    pub call_contract_offchain_data_size_limit: usize,
    /// The maximum number of concurrent HTTP requests.
    #[arg(env = "REST_SERVICE_MAX_CONCURRENT_HTTP_REQUESTS")]
    pub max_concurrent_http_requests: usize,
}

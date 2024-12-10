use core::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Configuration for the REST service.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The socket address to bind to.
    pub bind_addr: SocketAddr,
    /// The maximum size of the data in a contract call with offchain data handling.
    pub call_contract_offchain_data_size_limit: usize,
}

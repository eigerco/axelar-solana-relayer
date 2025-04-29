//! Simple HTTP client that will limit the amount of concurrent requests.
//! It will also retry the HTTP calls if they failed with a an exponential backoff strategy.
//! Intended to rate-limit Solana RPC calls.

use core::time::Duration;
use std::sync::Arc;

use async_trait::async_trait;
use backoff::future::retry;
use backoff::ExponentialBackoffBuilder;
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;
use solana_client::client_error::{ClientError, ClientErrorKind};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::RpcClientConfig;
use solana_client::rpc_sender::RpcTransportStats;
use solana_rpc_client::http_sender::HttpSender;
use solana_rpc_client::rpc_sender::RpcSender;
use solana_rpc_client_api::client_error::Result as ClientResult;
use solana_rpc_client_api::request::RpcRequest;
use solana_sdk::commitment_config::CommitmentConfig;
use tokio::sync::Semaphore;
use tracing::error;
use typed_builder::TypedBuilder;

/// The maximum elapsed time for retrying failed requests.
const MAX_BACKOFF_ELAPSED_TIME: Duration = Duration::from_millis(60 * 1_000);

/// Create a new Solana RPC client based on the provided config
#[must_use]
pub fn new_client(config: &Config) -> Arc<RpcClient> {
    let sender = RetryingHttpSender::new(
        config.solana_http_rpc.to_string(),
        config.max_concurrent_rpc_requests,
    );
    let config = RpcClientConfig::with_commitment(config.commitment);
    let client = RpcClient::new_sender(sender, config);
    Arc::new(client)
}

/// A wrapper around `HttpSender` that adds retry logic for sending RPC
/// requests.
pub struct RetryingHttpSender {
    http_client: HttpSender,
    request_permit: Arc<Semaphore>,
}

impl RetryingHttpSender {
    /// Initialize a new
    #[must_use]
    pub fn new(url: String, max_concurrent_requests: usize) -> Self {
        let http = HttpSender::new(url);
        let request_permit = Arc::new(Semaphore::new(max_concurrent_requests));
        Self {
            http_client: http,
            request_permit,
        }
    }

    async fn send_internal(
        &self,
        request: RpcRequest,
        params: &Value,
    ) -> Result<Value, backoff::Error<ClientError>> {
        use ClientErrorKind::{
            Custom, Io, Middleware, Reqwest, RpcError, SerdeJson, SigningError, TransactionError,
        };
        // get the permit to make the request
        let _permit = Arc::clone(&self.request_permit)
            .acquire_owned()
            .await
            .expect("the semaphore will never be closed");

        // make the actual request
        self.http_client
            .send(request, params.clone())
            .await
            .inspect_err(|error| error!(%error))
            .map_err(|error| match *error.kind() {
                // Retry on networking-io related errors and cases where the node may need some
                // catch-up time
                Io(_) | Reqwest(_) | SerdeJson(_) | RpcError(_) => backoff::Error::transient(error),
                // Fail instantly on other errors
                SigningError(_) | TransactionError(_) | Custom(_) => {
                    backoff::Error::permanent(error)
                }
                Middleware(_) => backoff::Error::permanent(error),
            })
    }
}

#[async_trait]
impl RpcSender for RetryingHttpSender {
    #[tracing::instrument(skip(self), name = "retrying_http_sender")]
    async fn send(&self, request: RpcRequest, params: Value) -> ClientResult<Value> {
        let strategy = ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(MAX_BACKOFF_ELAPSED_TIME))
            .build();
        let operation = || self.send_internal(request, &params);
        retry(strategy, operation).await
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        self.http_client.get_transport_stats()
    }

    fn url(&self) -> String {
        self.http_client.url()
    }
}

/// Configuration for initialising the [`RetryingHttpSender`]
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, TypedBuilder, Parser)]
pub struct Config {
    /// The rpc of the solana node
    #[arg(value_name = "SOLANA_RPC_SOLANA_HTTP_RPC", env = "SOLANA_RPC_SOLANA_HTTP_RPC")]
    pub solana_http_rpc: url::Url,
    
    /// How many rpc requests we process at the same time
    #[builder(default = config_defaults::max_concurrent_rpc_requests())]
    #[serde(
        rename = "max_concurrent_rpc_requests",
        default = "config_defaults::max_concurrent_rpc_requests"
    )]
    #[arg(
        value_name = "SOLANA_RPC_MAX_CONCURRENT_RPC_REQUESTS",
        env = "SOLANA_RPC_MAX_CONCURRENT_RPC_REQUESTS",
        default_value = config_defaults::max_concurrent_rpc_requests().to_string()
    )]
    pub max_concurrent_rpc_requests: usize,

    /// The Solna RPC tx commitment config
    #[serde(default = "CommitmentConfig::finalized")]
    #[arg(
        value_name = "SOLANA_RPC_COMMITMENT",
        env = "SOLANA_RPC_COMMITMENT",
        default_value = "finalized"
    )]
    pub commitment: CommitmentConfig,
}

mod config_defaults {
    pub(crate) const fn max_concurrent_rpc_requests() -> usize {
        5
    }
}

use std::sync::Arc;

use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;

/// Config for the [`crate::SolanaEventForwarder`] component.
///
/// Parses events coming in from [`solana_listener::SolanaListener`] and forwards them to the
/// [`relayer_amplifier_api_integration::Amplifier`] component.
#[derive(Clone)]
pub struct Config {
    /// The chain name that we're listening for.
    /// This value must be the same one that Amplifier API is expected to interact with
    pub source_chain_name: String,
    /// The Solana gateway program id.
    pub gateway_program_id: Pubkey,
    /// The gas service program id
    pub gas_service_program_id: Pubkey,
    /// RPC client for the Solana chain
    pub rpc: Arc<RpcClient>,
    /// commitment config to use when communicating with Solana
    pub commitment: CommitmentConfig,
}

impl Config {
    /// Create a new configuration based on the [`solana_listener::Config`] and
    /// [`relayer_amplifier_api_integration::Config`] configurations.
    #[must_use]
    pub fn new(
        sol_listener_cfg: &solana_listener::Config,
        solana_task_processor: &solana_gateway_task_processor::Config,
        amplifier_cfg: &relayer_amplifier_api_integration::Config,
        rpc: Arc<RpcClient>,
    ) -> Self {
        Self {
            source_chain_name: amplifier_cfg.chain.clone(),
            gateway_program_id: sol_listener_cfg.gateway_program_address,
            gas_service_program_id: solana_task_processor.gas_service_program_address,
            rpc,
            commitment: sol_listener_cfg.commitment,
        }
    }
}

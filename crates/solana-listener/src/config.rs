//! Configuration structures and primitives for the [`crate::RelayerEngine`]

use core::time::Duration;

use serde::Deserialize;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use typed_builder::TypedBuilder;

/// Top-level configuration for the solana component.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, TypedBuilder)]
pub struct Config {
    /// Gateway program id
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    #[builder(default = config_defaults::gateway_program_address())]
    #[serde(default = "config_defaults::gateway_program_address")]
    pub gateway_program_address: Pubkey,

    /// Gas service config PDA
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    pub gas_service_config_pda: Pubkey,

    /// The websocket endpoint of the solana node
    pub solana_ws: url::Url,

    /// How often we want to poll the network for new signatures
    #[builder(default = config_defaults::tx_scan_poll_period())]
    #[serde(
        rename = "tx_scan_poll_period_in_milliseconds",
        default = "config_defaults::tx_scan_poll_period",
        deserialize_with = "core_common_serde_utils::duration_ms_decode"
    )]
    pub tx_scan_poll_period: Duration,

    /// The commitment level for the solana node tx state
    #[serde(default = "CommitmentConfig::finalized")]
    pub commitment: CommitmentConfig,
}

pub(crate) mod config_defaults {
    use core::time::Duration;

    use solana_sdk::pubkey::Pubkey;

    pub(crate) const fn tx_scan_poll_period() -> Duration {
        Duration::from_millis(1000)
    }

    pub(crate) const fn gateway_program_address() -> Pubkey {
        axelar_solana_gateway::id()
    }
}

//! Configuration structures and primitives for the [`crate::RelayerEngine`]

use core::time::Duration;

use clap::Parser;
use eyre::Result;
use serde::Deserialize;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use typed_builder::TypedBuilder;

/// Top-level configuration for the solana component.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, TypedBuilder, Parser)]
pub struct Config {
    /// Gateway program id
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    #[builder(default = config_defaults::gateway_program_address())]
    #[serde(default = "config_defaults::gateway_program_address")]
    #[arg(env = "GATEWAY_PROGRAM_ADDRESS")]
    pub gateway_program_address: Pubkey,

    /// Gas service config PDA
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    #[arg(env = "GAS_SERVICE_CONFIG_PDA")]
    pub gas_service_config_pda: Pubkey,

    /// The websocket endpoint of the solana node
    #[arg(
        value_name = "SOLANA_LISTENER_SOLANA_WS",
        env = "SOLANA_LISTENER_SOLANA_WS"
    )]
    pub solana_ws: url::Url,

    /// How often we want to poll the network for new signatures
    #[builder(default = config_defaults::tx_scan_poll_period())]
    #[serde(
        rename = "tx_scan_poll_period_in_milliseconds",
        default = "config_defaults::tx_scan_poll_period",
        deserialize_with = "core_common_serde_utils::duration_ms_decode"
    )]
    #[arg(
        value_name= "SOLANA_LISTENER_TX_SCAN_POLL_PERIOD",
        env = "SOLANA_LISTENER_TX_SCAN_POLL_PERIOD",
        value_parser = parse_tx_scan_poll_period,
        default_value = config_defaults::tx_scan_poll_period_value().to_string()
    )]
    pub tx_scan_poll_period: Duration,

    /// The commitment level for the solana node tx state
    #[serde(default = "CommitmentConfig::finalized")]
    #[arg(
        value_name = "SOLANA_LISTENER_COMMITMENT",
        env = "SOLANA_LISTENER_COMMITMENT",
        default_value = "finalized"
    )]
    pub commitment: CommitmentConfig,
}

fn parse_tx_scan_poll_period(input: &str) -> Result<Duration> {
    Ok(Duration::from_secs(input.parse::<u64>()?))
}

pub(crate) mod config_defaults {
    use core::time::Duration;

    use solana_sdk::pubkey::Pubkey;

    pub(crate) const fn tx_scan_poll_period() -> Duration {
        Duration::from_millis(tx_scan_poll_period_value())
    }

    pub(crate) const fn tx_scan_poll_period_value() -> u64 {
        1000
    }

    pub(crate) const fn gateway_program_address() -> Pubkey {
        axelar_solana_gateway::id()
    }
}

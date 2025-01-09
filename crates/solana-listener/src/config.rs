//! Configuration structures and primitives for the [`crate::RelayerEngine`]

use core::time::Duration;

use serde::Deserialize;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
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

    /// This defines how to handle missed signatures upon startup
    pub missed_signature_catchup_strategy: MissedSignatureCatchupStrategy,

    /// This defines the latest signature that we have parsed
    #[serde(default)]
    #[serde(deserialize_with = "serde_utils::signature_decode")]
    pub latest_processed_signature: Option<Signature>,

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

/// The strategy which defines on how we want to handle parsing historical signatures.
///
/// It is useful for when you want to double-check or suspect that the relayer has missed some txs
/// in the past.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissedSignatureCatchupStrategy {
    /// Don't parse historical signatures at all
    None,
    /// Parse all signatures until the initial gateway deployment
    UntilBeginning,
    /// Parse all signtatures until we reach the desired end signature.
    UntilSignatureReached(Signature),
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

mod serde_utils {
    use core::str::FromStr as _;

    use serde::{Deserialize as _, Deserializer};
    use solana_sdk::signature::Signature;

    pub(crate) fn signature_decode<'de, D>(deserializer: D) -> Result<Option<Signature>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)?
            .map(|raw_string| {
                Signature::from_str(&raw_string).map_err(|err| {
                    serde::de::Error::custom(format!("Cannot parse signature: {err}"))
                })
            })
            .transpose()
    }
}

use clap::Parser;
use serde::Deserialize;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use typed_builder::TypedBuilder;

/// Configuration for the [`SolanaTxPusher`] component
#[derive(Debug, Deserialize, PartialEq, TypedBuilder, Parser)]
pub struct Config {
    /// Gateway program id
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    #[builder(default = config_defaults::gateway_program_address())]
    #[serde(default = "config_defaults::gateway_program_address")]
    #[arg(env = "GATEWAY_PROGRAM_ADDRESS")]
    pub gateway_program_address: Pubkey,

    /// Gas service program id
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    #[builder(default = config_defaults::gas_service_program_address())]
    #[serde(default = "config_defaults::gas_service_program_address")]
    #[arg(env = "GAS_SERVICE_PROGRAM_ADDRESS")]
    pub gas_service_program_address: Pubkey,

    /// The PDA used to store the gas service configuration associated with this relayer instance.
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    #[arg(env = "GAS_SERVICE_CONFIG_PDA")]
    pub gas_service_config_pda: Pubkey,

    /// The signing keypair for transactions.
    /// Can be represented as a base58 string or 64 element array `[42, 42, ..]`
    #[arg(env = "SOLANA_GATEWAY_SIGNING_KEYPAIR")]
    pub signing_keypair: String,

    /// Commitment config to use for solana RPC interactions
    #[builder(default = CommitmentConfig::finalized())]
    #[serde(default = "CommitmentConfig::finalized")]
    #[arg(env = "SOLANA_GATEWAY_COMMITMENT")]
    pub commitment: CommitmentConfig,
}

impl Config {
    /// Signing keypair as Keypair struct
    pub fn signing_keypair(&self) -> Keypair {
        Keypair::from_base58_string(&self.signing_keypair)
    }
}

pub(crate) mod config_defaults {
    use solana_sdk::pubkey::Pubkey;

    pub(crate) const fn gateway_program_address() -> Pubkey {
        axelar_solana_gateway::id()
    }

    pub(crate) const fn gas_service_program_address() -> Pubkey {
        axelar_solana_gas_service::id()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use solana_sdk::signature::Keypair;

    use super::*;

    #[test]
    fn test_deserialize_keypair_base58() {
        // Generate a new Keypair and encode it in Base58
        let keypair = Keypair::new();
        let keypair_bytes = keypair.to_bytes();
        let base58_encoded = bs58::encode(&keypair_bytes).into_string();

        // Prepare JSON data
        let data = json!({
            "gateway_program_address": Pubkey::new_unique().to_string(),
            "gas_service_config_pda": Pubkey::new_unique().to_string(),
            "signing_keypair": base58_encoded
        });

        // Deserialize Config
        let config: Config = serde_json::from_value(data).expect("Failed to deserialize Config");

        // Check if the deserialized keypair matches the original
        assert_eq!(config.signing_keypair.to_bytes(), keypair_bytes);
    }

    #[test]
    fn test_deserialize_keypair_array() {
        // Generate a new Keypair
        let keypair = Keypair::new();
        let keypair_bytes = keypair.to_bytes();

        // Prepare JSON data
        let data = json!({
            "gateway_program_address": Pubkey::new_unique().to_string(),
            "gas_service_config_pda": Pubkey::new_unique().to_string(),
            "signing_keypair": keypair_bytes.to_vec()
        });

        // Deserialize Config
        let config: Config = serde_json::from_value(data).expect("Failed to deserialize Config");

        // Check if the deserialized keypair matches the original
        assert_eq!(config.signing_keypair.to_bytes(), keypair_bytes);
    }

    #[test]
    fn test_deserialize_keypair_invalid_length() {
        // Create an invalid keypair byte array of incorrect length
        let invalid_bytes = vec![0_u8; 63]; // Should be 64 bytes

        // Prepare JSON data
        let data = json!({
            "gateway_program_address": Pubkey::new_unique().to_string(),
            "signing_keypair": invalid_bytes
        });

        // Attempt to deserialize Config
        let result: Result<Config, _> = serde_json::from_value(data);

        // Check that deserialization fails
        result.unwrap_err();
    }

    #[test]
    fn test_deserialize_keypair_invalid_encoding() {
        // Provide an invalid encoded string
        let invalid_encoded = "invalid_keypair_string";

        // Prepare JSON data
        let data = json!({
            "gateway_program_address": Pubkey::new_unique().to_string(),
            "signing_keypair": invalid_encoded
        });

        // Attempt to deserialize Config
        let result: Result<Config, _> = serde_json::from_value(data);

        // Check that deserialization fails
        result.unwrap_err();
    }
}

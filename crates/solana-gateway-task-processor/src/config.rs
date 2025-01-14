use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use typed_builder::TypedBuilder;

/// Configuration for the [`SolanaTxPusher`] component
#[derive(Debug, Deserialize, PartialEq, TypedBuilder)]
pub struct Config {
    /// Gateway program id
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    #[builder(default = config_defaults::gateway_program_address())]
    #[serde(default = "config_defaults::gateway_program_address")]
    pub gateway_program_address: Pubkey,

    /// Gas service program id
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    #[builder(default = config_defaults::gas_service_program_address())]
    #[serde(default = "config_defaults::gas_service_program_address")]
    pub gas_service_program_address: Pubkey,

    /// The PDA used to store the gas service configuration associated with this relayer instance.
    #[serde(deserialize_with = "common_serde_utils::pubkey_decode")]
    pub gas_service_config_pda: Pubkey,

    /// The signing keypair for transactions.
    /// Can be represented as a base58 string or 64 element array `[42, 42, ..]`
    #[serde(deserialize_with = "serde_utils::deserialize_keypair")]
    pub signing_keypair: Keypair,
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

#[expect(clippy::min_ident_chars, reason = "part of trait definitions")]
mod serde_utils {

    use serde::de::{self, Deserializer, Visitor};
    use solana_sdk::signature::Keypair;

    pub(crate) fn deserialize_keypair<'de, D>(deserializer: D) -> Result<Keypair, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(KeypairVisitor)
    }

    struct KeypairVisitor;

    impl<'de> Visitor<'de> for KeypairVisitor {
        type Value = Keypair;

        fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            formatter
                .write_str("a base58 encoded string or an array of 64 bytes representing a keypair")
        }

        fn visit_str<E>(self, v: &str) -> Result<Keypair, E>
        where
            E: de::Error,
        {
            // Try Base58 decoding
            if let Ok(keypair_bytes) = bs58::decode(v).into_vec() {
                if keypair_bytes.len() == 64 {
                    return Keypair::from_bytes(&keypair_bytes).map_err(de::Error::custom);
                }
            }

            Err(de::Error::custom("Invalid keypair encoding or length"))
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Keypair, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut bytes = Vec::with_capacity(64);
            while let Some(value) = seq.next_element::<u8>()? {
                bytes.push(value);
            }
            if bytes.len() != 64 {
                return Err(de::Error::custom("Invalid keypair length"));
            }
            Keypair::from_bytes(&bytes).map_err(de::Error::custom)
        }
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

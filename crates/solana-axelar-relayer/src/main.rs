//! Transaction relayer for Solana-Axelar integration

use std::path::PathBuf;
use std::sync::Arc;

use relayer_amplifier_api_integration::Amplifier;
use relayer_engine::{RelayerComponent, RelayerEngine};
use serde::Deserialize;

mod telemetry;

#[tokio::main]
async fn main() {
    // Load configuration
    let tracing_endpoint = std::env::var("TRACING_ENDPOINT").ok();
    let config_file = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("invalid file path");

    // Initialize tracing
    telemetry::init_telemetry(tracing_endpoint).expect("could not init telemetry");
    color_eyre::install().expect("color eyre could not be installed");

    let config_file = std::fs::read_to_string(config_file).expect("cannot read config file");
    let config = toml::from_str::<Config>(&config_file).expect("invalid config file");

    let file_based_storage = file_based_storage::MemmapState::new(config.storage_path)
        .expect("could not init file based storage");

    let rpc_client = retrying_solana_http_sender::new_client(&config.solana_rpc);
    let event_forwarder_config = solana_event_forwarder::Config::new(
        &config.solana_listener_component,
        &config.amplifier_component,
    );
    let name_on_amplifier = config.amplifier_component.chain.clone();
    let (amplifier_component, amplifier_client, amplifier_task_receiver) =
        Amplifier::new(config.amplifier_component, file_based_storage.clone());
    let gateway_task_processor = solana_gateway_task_processor::SolanaTxPusher::new(
        config.solana_gateway_task_processor,
        name_on_amplifier.clone(),
        Arc::clone(&rpc_client),
        amplifier_task_receiver,
        amplifier_client.clone(),
        file_based_storage,
    );
    let (solana_listener_component, solana_listener_client) = solana_listener::SolanaListener::new(
        config.solana_listener_component,
        Arc::clone(&rpc_client),
    );
    let solana_event_forwarder_component = solana_event_forwarder::SolanaEventForwarder::new(
        event_forwarder_config,
        solana_listener_client,
        amplifier_client.clone(),
    );

    let rest_service_component = rest_service::RestService::new(
        &config.rest_service,
        name_on_amplifier,
        Arc::clone(&rpc_client),
        amplifier_client,
    );

    let components: Vec<Box<dyn RelayerComponent>> = vec![
        Box::new(amplifier_component),
        Box::new(solana_listener_component),
        Box::new(solana_event_forwarder_component),
        Box::new(gateway_task_processor),
        Box::new(rest_service_component),
    ];
    RelayerEngine::new(config.relayer_engine, components)
        .start_and_wait_for_shutdown()
        .await;
}

pub(crate) const fn get_service_version() -> &'static str {
    env!("GIT_HASH")
}

pub(crate) const fn get_service_name() -> &'static str {
    concat!(env!("CARGO_PKG_NAME"), env!("GIT_HASH"))
}

/// Top-level configuration for the relayer.
#[derive(Debug, Deserialize, PartialEq)]
pub struct Config {
    /// Configuration for the Amplifier API processor
    pub amplifier_component: relayer_amplifier_api_integration::Config,
    /// Configuration for the Solana transaction listener processor
    pub solana_listener_component: solana_listener::Config,
    /// Configuration for the Solana transaction listener processor
    pub solana_gateway_task_processor: solana_gateway_task_processor::Config,
    /// Meta-configuration on the engine
    pub relayer_engine: relayer_engine::Config,
    /// Shared configuration for the Solana RPC client
    pub solana_rpc: retrying_solana_http_sender::Config,
    /// Path to the storage configuration file
    pub storage_path: std::path::PathBuf,
    /// Configuration for the REST service
    pub rest_service: rest_service::Config,
}

#[expect(
    clippy::panic_in_result_fn,
    reason = "assertions in tests that return Result is fine"
)]
#[cfg(test)]
mod tests {
    use core::net::SocketAddr;
    use core::str::FromStr as _;
    use core::time::Duration;

    use amplifier_api::identity::Identity;
    use pretty_assertions::assert_eq;
    use solana_listener::solana_sdk::commitment_config::CommitmentConfig;
    use solana_listener::solana_sdk::pubkey::Pubkey;
    use solana_listener::solana_sdk::signature::{Keypair, Signature};
    use solana_listener::MissedSignatureCatchupStrategy;

    use crate::Config;

    #[test]
    fn parse_toml() -> eyre::Result<()> {
        let amplifier_url = "https://examlple.com".parse()?;
        let healthcheck_bind_addr = "127.0.0.1:8000";
        let chain = "solana-devnet";
        let gateway_program_address = Pubkey::new_unique();
        let gateway_program_address_as_str = gateway_program_address.to_string();
        let gas_service_config_pda = Pubkey::new_unique();
        let gas_service_config_pda_as_str = Pubkey::new_unique();
        let solana_rpc = "https://api.solana-devnet.com".parse()?;
        let solana_ws = "wss://api.solana-devnet.com".parse()?;
        let solana_tx_scan_poll_period = Duration::from_millis(42);
        let solana_tx_scan_poll_period_ms = solana_tx_scan_poll_period.as_millis();
        let max_concurrent_rpc_requests = 100;
        let signing_keypair = Keypair::new();
        let signing_keypair_as_str = signing_keypair.to_base58_string();
        let latest_processed_signature = Signature::new_unique().to_string();
        let identity = identity_fixture();
        let missed_signature_catchup_strategy = "until_beginning";
        let rest_service_bind_addr = "127.0.0.1:80";
        let call_contract_offchain_data_size_limit = 10 * 1024 * 1024;

        let input = indoc::formatdoc! {r#"
            storage_path = "./store"

            [amplifier_component]
            identity = '''
            {identity}
            '''
            url = "{amplifier_url}"
            chain = "{chain}"

            [relayer_engine]
            [relayer_engine.health_check]
            bind_addr = "{healthcheck_bind_addr}"

            [solana_listener_component]
            gateway_program_address = "{gateway_program_address_as_str}"
            gas_service_config_pda = "{gas_service_config_pda_as_str}"
            solana_ws = "{solana_ws}"
            tx_scan_poll_period_in_milliseconds = {solana_tx_scan_poll_period_ms}
            missed_signature_catchup_strategy = "{missed_signature_catchup_strategy}"
            latest_processed_signature = "{latest_processed_signature}"

            [solana_gateway_task_processor]
            signing_keypair = "{signing_keypair_as_str}"
            gateway_program_address = "{gateway_program_address_as_str}"
            gas_service_config_pda = "{gas_service_config_pda_as_str}"

            [solana_rpc]            
            max_concurrent_rpc_requests = {max_concurrent_rpc_requests}
            solana_http_rpc = "{solana_rpc}"

            [rest_service]
            bind_addr = "{rest_service_bind_addr}"
            call_contract_offchain_data_size_limit = {call_contract_offchain_data_size_limit}
        "#};

        let parsed: Config = toml::from_str(&input)?;
        let expected = Config {
            amplifier_component: relayer_amplifier_api_integration::Config::builder()
                .identity(Identity::new_from_pem_bytes(identity_fixture().as_bytes())?)
                .url(amplifier_url)
                .chain(chain.to_owned())
                .build(),
            relayer_engine: relayer_engine::Config {
                health_check: relayer_engine::HealthCheckConfig {
                    bind_addr: SocketAddr::from_str(healthcheck_bind_addr)?,
                },
            },
            solana_listener_component: solana_listener::Config {
                gateway_program_address,
                gas_service_config_pda,
                tx_scan_poll_period: solana_tx_scan_poll_period,
                solana_ws,
                missed_signature_catchup_strategy: MissedSignatureCatchupStrategy::UntilBeginning,
                latest_processed_signature: Some(Signature::from_str(&latest_processed_signature)?),
                commitment: CommitmentConfig::finalized(),
            },
            solana_gateway_task_processor: solana_gateway_task_processor::Config {
                gateway_program_address,
                gas_service_config_pda,
                signing_keypair,
            },
            solana_rpc: retrying_solana_http_sender::Config {
                max_concurrent_rpc_requests,
                solana_http_rpc: solana_rpc,
            },
            storage_path: "./store".parse().unwrap(),
            rest_service: rest_service::Config {
                bind_addr: SocketAddr::from_str(rest_service_bind_addr)?,
                call_contract_offchain_data_size_limit,
            },
        };
        assert_eq!(parsed, expected);
        Ok(())
    }

    fn identity_fixture() -> String {
        indoc::indoc! {"
            -----BEGIN CERTIFICATE-----
            MIIC3zCCAcegAwIBAgIJALAul9kzR0W/MA0GCSqGSIb3DQEBBQUAMA0xCzAJBgNV
            BAYTAmx2MB4XDTIyMDgwMjE5MTE1NloXDTIzMDgwMjE5MTE1NlowDTELMAkGA1UE
            BhMCbHYwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC8WWPaghYJcXQp
            W/GAoFqKrQIwxy+h8vdZiURVzzqDKt/Mz45x0Zqj8RVSe4S0lLfkRxcgrLz7ZYSc
            TKsVcur8P66F8A2AJaC4KDiYj4azkTtYQDs+RDLRJUCz5xf/Nw7m+6Y0K7p/p2m8
            bPSm6osefz0orQqpwGogqOwI0FKMkU+BpYjMb+k29xbOec6aHxlaPlHLBPa+n3WC
            V96KwmzSMPEN6Fn/G6PZ5PtwmNg769PiXKk02p+hbnx5OCKvi94mn8vVBGgXF6JR
            Vq9IQQvfFm6G6tf7q+yxMdR2FBR2s03t1daJ3RLGdHzXWTAaNRS7E93OWx+ZyTkd
            kIVM16HTAgMBAAGjQjBAMAkGA1UdEwQCMAAwEQYJYIZIAYb4QgEBBAQDAgeAMAsG
            A1UdDwQEAwIFoDATBgNVHSUEDDAKBggrBgEFBQcDAjANBgkqhkiG9w0BAQUFAAOC
            AQEAU/uQHjntyIVR4uQCRoSO5VKyQcXXFY5pbx4ny1yrn0Uxb9P6bOxY5ojcs0r6
            z8ApT3sUfww7kzhle/G5DRtP0cELq7N2YP+qsFx8UO1GYZ5SLj6xm81vk3c0+hrO
            Q3yoS60xKd/7nVsPZ3ch6+9ND0vVUOkefy0aeNix9YgbYjS11rTj7FNiHD25zOJd
            VpZtHkvYDpHcnwUCd0UAuu9ntKKMFGwc9GMqzfY5De6nITvlqzH8YM4AjKO26JsU
            7uMSyHtGF0vvyzhkwCqcuy7r9lQr9m1jTsJ5pSaVasIOJe+/JBUEJm5E4ppdslnW
            1PkfLWOJw34VKkwibWLlwAwTDQ==
            -----END CERTIFICATE-----
            -----BEGIN PRIVATE KEY-----
            MIIEpAIBAAKCAQEAvFlj2oIWCXF0KVvxgKBaiq0CMMcvofL3WYlEVc86gyrfzM+O
            cdGao/EVUnuEtJS35EcXIKy8+2WEnEyrFXLq/D+uhfANgCWguCg4mI+Gs5E7WEA7
            PkQy0SVAs+cX/zcO5vumNCu6f6dpvGz0puqLHn89KK0KqcBqIKjsCNBSjJFPgaWI
            zG/pNvcWznnOmh8ZWj5RywT2vp91glfeisJs0jDxDehZ/xuj2eT7cJjYO+vT4lyp
            NNqfoW58eTgir4veJp/L1QRoFxeiUVavSEEL3xZuhurX+6vssTHUdhQUdrNN7dXW
            id0SxnR811kwGjUUuxPdzlsfmck5HZCFTNeh0wIDAQABAoIBAQCNJFNukCMhanKI
            98xu/js7RlCo6urn6mGvJ+0cfJE1b/CL01HEOzUt+2BmEgetJvDy0M8k/i0UGswY
            MF/YT+iFpNcMqYoEaK4aspFOyedAMuoMxP1gOMz363mkFt3ls4WoVBYFbGtyc6sJ
            t4BSgNpFvUXAcIPYF0ewN8XBCRODH6v7Z6CrbvtjlUXMuU02r5vzMh8a4znIJmZY
            40x6oNIss3YDCGe8J6qMWHByMDZbO63gBoBYayTozzCzl1TG0RZ1oTTL4z36wRto
            uAhjoRek2kiO5axIgKPR/tYlyKzwLkS5v1W09K+pvsabAU6gQlC8kUPk7/+GOaeI
            wGMI9FAZAoGBAOJN8mqJ3zHKvkyFW0uFMU14dl8SVrCZF1VztIooVgnM6bSqNZ3Y
            nKE7wk1DuFjqKAi/mgXTr1v8mQtr40t5dBEMdgDpfRf/RrMfQyhEgQ/m1WqBQtPx
            Suz+EYMpcH05ynrfSbxCDNYM4OHNJ1QfIvHJ/Q9wt5hT7w+MOH5h5TctAoGBANUQ
            cXF4QKU6P+dLUYNjrYP5Wjg4194i0fh/I9NVoUE9Xl22J8l0lybV2phkuODMp1I+
            rBi9AON9skjdCnwtH2ZbRCP6a8Zjv7NMLy4b4dQqfoHwTdCJ0FBfgZXhH4i+AXMb
            XsKotxKGqCWgFKY8LB3UJ0qakK6h9Ze+/zbnZ9z/AoGBAJwrQkD3SAkqakyQMsJY
            9f8KRFWzaBOSciHMKSi2UTmOKTE9zKZTFzPE838yXoMtg9cVsgqXXIpUNKFHIKGy
            /L/PI5fZiTQIPBfcWRHuxEne+CP5c86i0xvc8OTcsf4Y5XwJnu7FfeoxFPd+Bcft
            fMXyqCoBlREPywelsk606+M5AoGAfXLICJJQJbitRYbQQLcgw/K+DxpQ54bC8DgT
            pOvnHR2AAVcuB+xwzrndkhrDzABTiBZEh/BIpKkunr4e3UxID6Eu9qwMZuv2RCBY
            KyLZjW1TvTf66Q0rrRb+mnvJcF7HRbnYym5CFFNaj4S4g8QsCYgPdlqZU2kizCz1
            4aLQQYsCgYAGKytrtHi2BM4Cnnq8Lwd8wT8/1AASIwg2Va1Gcfp00lamuy14O7uz
            yvdFIFrv4ZPdRkf174B1G+FDkH8o3NZ1cf+OuVIKC+jONciIJsYLPTHR0pgWqE4q
            FAbbOyAg51Xklqm2Q954WWFmu3lluHCWUGB9eSHshIurTmDd+8o15A==
            -----END PRIVATE KEY-----
        "}
        .to_owned()
    }
}

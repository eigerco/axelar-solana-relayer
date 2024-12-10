//! Tests for call-contract-offchain-data endpoint
#![cfg(test)]
#![expect(clippy::non_ascii_literal, reason = "Test code")]
use core::time::Duration;
use std::collections::HashMap;
use std::sync::Arc;

use amplifier_api::types::{Event, PublishEventsRequest};
use futures::StreamExt as _;
use indoc::indoc;
use relayer_amplifier_api_integration::{AmplifierCommand, AmplifierCommandClient};
use relayer_engine::RelayerComponent as _;
use serde_json::{json, Value};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use test_log::test;

// The only real things here are the logs and the status, the rest is made up garbage.
const RESPONSE_JSON: &str = indoc! {r#"
{
    "meta": {
      "err": null,
      "fee": 5000,
      "innerInstructions": [],
      "postBalances": [499998932500, 26858640, 1, 1, 1],
      "postTokenBalances": [],
      "preBalances": [499998937500, 26858640, 1, 1, 1],
      "preTokenBalances": [],
      "rewards": [],
      "status": {
        "Ok": null
      },
      "logMessages": [
        "Program memQuKMGBounhwP5yw9qomYNU97Eqcx9c4XwDUo6uGV invoke [1]",
        "Program log: Invalid instruction data: [2, 136, 4, 0, 0, 240, 159, 144, 170, 240, 159, 144, 170, 240, 159, 144]",
        "Program log: Instruction: Native",
        "Program log: Instruction: SendToGateway",
        "Program gtwgM94UYHwBh3g7rWi1tcpkgELxHQRLPpPHsaECW57 invoke [2]",
        "Program log: Instruction: Call Contract Offchain Data",
        "Program data: b2ZmY2hhaW4gZGF0YV9fXw== 6NGe5cm7PkXHz/g8V2VdRg0nU0l7R48x8lll4s0Clz0= ik8DocGbvnSCYaX9IUJZUapVH+LFQmwjbrT91ZGn450= ZXRoZXJldW0= MHgwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBhZmRlMGY1MWU2OTJmMGU0YmZkNDVkMGYyYmI2ODRmYjM3YmZjNjQz",
        "Program gtwgM94UYHwBh3g7rWi1tcpkgELxHQRLPpPHsaECW57 consumed 4452 of 172724 compute units",
        "Program gtwgM94UYHwBh3g7rWi1tcpkgELxHQRLPpPHsaECW57 success",
        "Program memQuKMGBounhwP5yw9qomYNU97Eqcx9c4XwDUo6uGV consumed 31849 of 200000 compute units",
        "Program memQuKMGBounhwP5yw9qomYNU97Eqcx9c4XwDUo6uGV success"
      ],
      "computeUnitsConsumed": 31849,
      "returnData": null
    },
    "slot": 430,
    "transaction":["5EcULiPU5rnAanHzXHY1xw9tLbgWJzCvKKaja6jaf8gy6wVgKBr6xB2xU4SWTwTk58TBfSNQ37nRphcsd1gonD3WHeC9zzeByWMZanQindk1jat3G819JeDrU7y6PmQSn5xFVE3S5iaUNGGN2r3aSy2vX86T21ENjCpbvLstUyxKM56B77MeSDZkyCxFQWn8Q69eoaJPvKb2p7y922nG75PH1YFjxrKn2EQet7k4jNh6RbnD2NAU2BNRrC5BiP2N4SJC3tTunfiNbskUbjmRSxTMdaG2JWpqgYyrZfS4AbjCiULhxEvtbjJwTvRzXzaFuiEMwvr3EcoZEReDEN4XRxqSSxDMpQch9qhn2p5sUuTDzzMntpQS1Ngr2TYwro2EqjhRqQLMXt7arpbWkhMG9DDXfKEmNtRsAGk9ANM2Dw1JUeK73xQ6N7s43sRi6rE7oTKTYewfHFzsuzcSqNa4LNUzNirzAGE6PGFVw7NUoB97jbKJscYFLXfR3K1N1K4reH6S46uW4SJMs9vABDVeUFk5FpKDY9ep9rgrscxc7xcviDBkEtMqmzhLwyWyLwPcjGWfSAFxTSeVXgAj3p7ekXe5UrE49tb5vKMX91MrSbhMd45VPa7xQYwFBopfXV6PCbPjZAbwas2azZfqXUEzBai9u92yMemtTbU3BYh3g15pabKa42sWbZabrfvkPi39ue7LK6v8F4Fu275U8xRGDeibDz5MzHX4AJPfAvKAcsMhUR8G9vAcvAz4WqqPVbToatQiYf6NRceC9oGrAYSRJsSL34ATPwSdCnxJN8SAKo9k7q4aNJpp5Q1fEV5", "base58"]
}
"#
};

#[test(tokio::test)]
async fn test_successful_call_contract_offchain_data() {
    let config = rest_service::Config {
        bind_addr: "127.0.0.1:8080".parse().unwrap(),
        call_contract_offchain_data_size_limit: 1024 * 1024 * 1024,
    };

    let parsed_response: Value = serde_json::from_str(RESPONSE_JSON).unwrap();
    let encoded_signature =
        "4dy5N9UQ2pX3DrKV8ueKY6K8tdNYYRfwzhUZ3Qxj6X3t7ykEDW2t8KJcbmvvr53F67MDzhxBsu9SN3pMKYqrwAss"
            .to_owned();

    let mut mocks = HashMap::new();
    mocks.insert(RpcRequest::GetTransaction, parsed_response);
    let rpc_client = RpcClient::new_mock_with_mocks("succeeds".to_owned(), mocks);

    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let amplifier_client = AmplifierCommandClient { sender: tx };

    let service = Box::new(rest_service::RestService::new(
        &config,
        "solana-devnet".to_owned(),
        Arc::new(rpc_client),
        amplifier_client,
    ));

    tokio::spawn(async move {
        service.process().await.unwrap();
    });

    let memo = "🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪"
        .to_owned()
        .replace(['\n', ' '], "");
    let memo_bytes = memo.as_bytes().to_vec();

    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:8080/call-contract-offchain-data/{encoded_signature}");
        client
            .post(url)
            .body(memo_bytes)
            .send()
            .await
            .expect("Failed to send request");
    });

    let amplifier_command = tokio::time::timeout(Duration::from_secs(1), rx.next())
        .await
        .expect("Timed out waiting for AmplifierCommand");

    let Some(AmplifierCommand::PublishEvents(PublishEventsRequest { mut events })) =
        amplifier_command
    else {
        panic!("Expected PublishEvents");
    };
    let Some(Event::Call(metadata)) = events.pop() else {
        panic!("Expected CallEvent");
    };
    assert_eq!(metadata.payload, memo.as_bytes());
}

#[test(tokio::test)]
async fn test_fail_call_contract_offchain_data_too_big() {
    let config = rest_service::Config {
        bind_addr: "127.0.0.1:8080".parse().unwrap(),
        call_contract_offchain_data_size_limit: 10,
    };

    let parsed_response: Value = serde_json::from_str(RESPONSE_JSON).unwrap();
    let encoded_signature =
        "4dy5N9UQ2pX3DrKV8ueKY6K8tdNYYRfwzhUZ3Qxj6X3t7ykEDW2t8KJcbmvvr53F67MDzhxBsu9SN3pMKYqrwAss"
            .to_owned();

    let mut mocks = HashMap::new();
    mocks.insert(RpcRequest::GetTransaction, parsed_response);
    let rpc_client = RpcClient::new_mock_with_mocks("succeeds".to_owned(), mocks);

    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let amplifier_client = AmplifierCommandClient { sender: tx };

    let service = Box::new(rest_service::RestService::new(
        &config,
        "solana-devnet".to_owned(),
        Arc::new(rpc_client),
        amplifier_client,
    ));

    tokio::spawn(async move {
        service.process().await.unwrap();
    });

    #[expect(clippy::non_ascii_literal, reason = "Test code")]
    let memo = "🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪"
        .to_owned()
        .replace(['\n', ' '], "");
    let memo_bytes = memo.as_bytes().to_vec();

    tokio::time::sleep(Duration::from_secs(1)).await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:8080/call-contract-offchain-data/{encoded_signature}");
    let Ok(response) = client.post(url).body(memo_bytes).send().await else {
        panic!("Expected response");
    };

    assert_eq!(response.status(), 413);
}

#[test(tokio::test)]
async fn test_fail_call_contract_offchain_data_invalid_signature() {
    let config = rest_service::Config {
        bind_addr: "127.0.0.1:8080".parse().unwrap(),
        call_contract_offchain_data_size_limit: 10 * 1024 * 1024,
    };

    let parsed_response: Value = serde_json::from_str(RESPONSE_JSON).unwrap();
    let encoded_signature = "3FiVfamkLFV8V4PXVVDdp7ciGn2yHxx".to_owned();

    let mut mocks = HashMap::new();
    mocks.insert(RpcRequest::GetTransaction, parsed_response);
    let rpc_client = RpcClient::new_mock_with_mocks("succeeds".to_owned(), mocks);

    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let amplifier_client = AmplifierCommandClient { sender: tx };

    let service = Box::new(rest_service::RestService::new(
        &config,
        "solana-devnet".to_owned(),
        Arc::new(rpc_client),
        amplifier_client,
    ));

    tokio::spawn(async move {
        service.process().await.unwrap();
    });

    #[expect(clippy::non_ascii_literal, reason = "Test code")]
    let memo = "🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪"
        .to_owned()
        .replace(['\n', ' '], "");
    let memo_bytes = memo.as_bytes().to_vec();

    tokio::time::sleep(Duration::from_secs(1)).await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:8080/call-contract-offchain-data/{encoded_signature}");
    let Ok(response) = client.post(url).body(memo_bytes).send().await else {
        panic!("Expected response");
    };

    assert_eq!(response.status(), 400);

    let error_message = response.text().await.expect("Expected error message");
    assert!(error_message.contains("Invalid transaction signature"));
}

#[test(tokio::test)]
async fn test_fail_call_contract_offchain_data_invalid_data() {
    let config = rest_service::Config {
        bind_addr: "127.0.0.1:8080".parse().unwrap(),
        call_contract_offchain_data_size_limit: 10 * 1024 * 1024,
    };

    let parsed_response: Value = serde_json::from_str(RESPONSE_JSON).unwrap();
    let encoded_signature =
        "4dy5N9UQ2pX3DrKV8ueKY6K8tdNYYRfwzhUZ3Qxj6X3t7ykEDW2t8KJcbmvvr53F67MDzhxBsu9SN3pMKYqrwAss"
            .to_owned();

    let mut mocks = HashMap::new();
    mocks.insert(RpcRequest::GetTransaction, parsed_response);
    let rpc_client = RpcClient::new_mock_with_mocks("succeeds".to_owned(), mocks);

    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let amplifier_client = AmplifierCommandClient { sender: tx };

    let service = Box::new(rest_service::RestService::new(
        &config,
        "solana-devnet".to_owned(),
        Arc::new(rpc_client),
        amplifier_client,
    ));

    tokio::spawn(async move {
        service.process().await.unwrap();
    });

    #[expect(clippy::non_ascii_literal, reason = "Test code")]
    let memo = "🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪"
        .to_owned()
        .replace(['\n', ' '], "");
    let memo_bytes = memo.as_bytes().to_vec();

    tokio::time::sleep(Duration::from_secs(1)).await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:8080/call-contract-offchain-data/{encoded_signature}");
    let Ok(response) = client.post(url).body(memo_bytes).send().await else {
        panic!("Expected response");
    };

    assert_eq!(response.status(), 400);

    let error_message = response.text().await.expect("Expected error message");
    assert!(error_message.contains("Payload hashes don't match"));
}

#[test(tokio::test)]
async fn test_fail_call_contract_offchain_data_on_tx_fetch_error() {
    let config = rest_service::Config {
        bind_addr: "127.0.0.1:8080".parse().unwrap(),
        call_contract_offchain_data_size_limit: 10 * 1024 * 1024,
    };

    let encoded_signature =
        "4dy5N9UQ2pX3DrKV8ueKY6K8tdNYYRfwzhUZ3Qxj6X3t7ykEDW2t8KJcbmvvr53F67MDzhxBsu9SN3pMKYqrwAss"
            .to_owned();

    let rpc_client = RpcClient::new_mock("fails".to_owned());

    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let amplifier_client = AmplifierCommandClient { sender: tx };

    let service = Box::new(rest_service::RestService::new(
        &config,
        "solana-devnet".to_owned(),
        Arc::new(rpc_client),
        amplifier_client,
    ));

    tokio::spawn(async move {
        service.process().await.unwrap();
    });

    #[expect(clippy::non_ascii_literal, reason = "Test code")]
    let memo = "🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪"
        .to_owned()
        .replace(['\n', ' '], "");
    let memo_bytes = memo.as_bytes().to_vec();

    tokio::time::sleep(Duration::from_secs(1)).await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:8080/call-contract-offchain-data/{encoded_signature}");
    let Ok(response) = client.post(url).body(memo_bytes).send().await else {
        panic!("Expected response");
    };

    assert_eq!(response.status(), 400);

    let error_message = response.text().await.expect("Expected error message");
    dbg!(&error_message);
    assert!(error_message.contains("Failed to fetch transaction logs"));
}

#[test(tokio::test)]
#[expect(clippy::indexing_slicing, reason = "It's fine to panic in tests")]
async fn test_fail_call_contract_offchain_data_on_event_not_found() {
    let config = rest_service::Config {
        bind_addr: "127.0.0.1:8080".parse().unwrap(),
        call_contract_offchain_data_size_limit: 10 * 1024 * 1024,
    };

    let mut parsed_response: Value = serde_json::from_str(RESPONSE_JSON).unwrap();

    // Remove logs so the event is not found
    parsed_response["meta"]["logMessages"] = json!([]);

    let encoded_signature =
        "4dy5N9UQ2pX3DrKV8ueKY6K8tdNYYRfwzhUZ3Qxj6X3t7ykEDW2t8KJcbmvvr53F67MDzhxBsu9SN3pMKYqrwAss"
            .to_owned();

    let mut mocks = HashMap::new();
    mocks.insert(RpcRequest::GetTransaction, parsed_response);
    let rpc_client = RpcClient::new_mock_with_mocks("succeeds".to_owned(), mocks);

    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let amplifier_client = AmplifierCommandClient { sender: tx };

    let service = Box::new(rest_service::RestService::new(
        &config,
        "solana-devnet".to_owned(),
        Arc::new(rpc_client),
        amplifier_client,
    ));

    tokio::spawn(async move {
        service.process().await.unwrap();
    });

    #[expect(clippy::non_ascii_literal, reason = "Test code")]
    let memo = "🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪
    🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪🐪"
        .to_owned()
        .replace(['\n', ' '], "");
    let memo_bytes = memo.as_bytes().to_vec();

    tokio::time::sleep(Duration::from_secs(1)).await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:8080/call-contract-offchain-data/{encoded_signature}");
    let Ok(response) = client.post(url).body(memo_bytes).send().await else {
        panic!("Expected response");
    };

    assert_eq!(response.status(), 404);

    let error_message = response.text().await.expect("Expected error message");
    assert!(error_message
        .contains("Successful transaction with CallContractOffchainDataEvent not found"));
}

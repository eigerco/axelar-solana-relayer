use core::future::Future;
use core::pin::Pin;

use futures::{SinkExt as _, StreamExt as _};
use gateway_gas_computation::compute_total_gas;
use relayer_amplifier_api_integration::amplifier_api::types::PublishEventsRequest;
use relayer_amplifier_api_integration::AmplifierCommand;
use solana_sdk::pubkey::Pubkey;
use solana_transaction_parser::parser::TransactionParserTrait as _;

use crate::map_core_events_to_amplifier;
use crate::utils::convert_to_parser_transaction;

/// The core component that is responsible for ingesting raw Solana events.
///
/// As a result, the logs get parsed, filtererd and mapped to Amplifier API events.
pub struct SolanaEventForwarder {
    config: crate::Config,
    solana_listener_client: solana_listener::SolanaListenerClient,
    amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
}

impl relayer_engine::RelayerComponent for SolanaEventForwarder {
    fn process(self: Box<Self>) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send>> {
        use futures::FutureExt as _;

        self.process_internal().boxed()
    }
}

impl SolanaEventForwarder {
    /// Instantiate a new `SolanaEventForwarder` using the pre-configured configuration.
    #[must_use]
    pub const fn new(
        config: crate::Config,
        solana_listener_client: solana_listener::SolanaListenerClient,
        amplifier_client: relayer_amplifier_api_integration::AmplifierCommandClient,
    ) -> Self {
        Self {
            config,
            solana_listener_client,
            amplifier_client,
        }
    }

    #[tracing::instrument(skip_all, name = "Solana log forwarder")]
    pub(crate) async fn process_internal(mut self) -> eyre::Result<()> {
        while let Some(message) = self.solana_listener_client.log_receiver.next().await {
            let total_cost = compute_total_gas(
                self.config.gateway_program_id,
                &message,
                &self.config.rpc,
                self.config.commitment,
            )
            .await?;
            let tx = convert_to_parser_transaction(&message);

            tracing::debug!(
                "Parser config: gateway={}, gas_service={}, account_keys={:?}",
                self.config.gateway_program_id,
                self.config.gas_service_program_id,
                tx.account_keys
            );

            let parser = solana_transaction_parser::parser::TransactionParser::new(
                self.config.source_chain_name.clone(),
                self.config.gas_service_program_id,
                self.config.gateway_program_id,
                Pubkey::default(), // Replace with ITS program once support is added
            );
            let events = parser
                .parse_transaction(serde_json::to_string(&tx)?)
                .await?;

            tracing::debug!("events: {:?}", events);

            tracing::info!(count = ?events.len(), "sending solana events to amplifier component");
            let command = AmplifierCommand::PublishEvents(PublishEventsRequest {
                events: map_core_events_to_amplifier(events, total_cost),
            });
            self.amplifier_client.sender.send(command).await?;
        }
        eyre::bail!("Listener has stopped unexpectedly");
    }
}

#[cfg(test)]
#[expect(clippy::unimplemented, reason = "needed for the test")]
#[expect(clippy::indexing_slicing, reason = "simpler code")]
#[expect(clippy::unreachable, reason = "simpler code")]
mod tests {
    use core::time::Duration;
    use std::path::PathBuf;
    use std::sync::Arc;

    use axelar_solana_encoding::types::execute_data::MerkleisedPayload;
    use axelar_solana_encoding::types::messages::{CrossChainId, Message, Messages};
    use axelar_solana_encoding::types::payload::Payload;
    use axelar_solana_gateway::executable::EncodingScheme;
    use axelar_solana_gateway::state::incoming_message::command_id;
    use axelar_solana_gateway::{get_incoming_message_pda, get_verifier_set_tracker_pda};
    use axelar_solana_gateway_test_fixtures::base::TestFixture;
    use axelar_solana_gateway_test_fixtures::gateway::make_verifiers_with_quorum;
    use axelar_solana_gateway_test_fixtures::SolanaAxelarIntegrationMetadata;
    use axelar_solana_memo_program::instruction::from_axelar_to_solana::build_memo;
    use futures::{stream, SinkExt as _, StreamExt as _, TryStreamExt as _};
    use pretty_assertions::assert_eq;
    use relayer_amplifier_api_integration::amplifier_api::types::{
        BigInt, CallEvent, CallEventMetadata, CommandId, Event, EventBase, EventMetadata,
        GasCreditEvent, GatewayV2Message, MessageApprovedEvent, MessageApprovedEventMetadata,
        MessageExecutedEvent, MessageExecutedEventMetadata, MessageExecutionStatus,
        PublishEventsRequest, Token, TxEvent, TxId,
    };
    use relayer_amplifier_api_integration::{AmplifierCommand, AmplifierCommandClient};
    use solana_listener::{fetch_transaction, SolanaListenerClient, TxStatus};
    use solana_rpc::rpc::JsonRpcConfig;
    use solana_rpc::rpc_pubsub_service::PubSubConfig;
    use solana_rpc_client::nonblocking::rpc_client::RpcClient;
    use solana_sdk::account::AccountSharedData;
    use solana_sdk::commitment_config::CommitmentConfig;
    use solana_sdk::compute_budget::ComputeBudgetInstruction;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::{Keypair, Signature};
    use solana_sdk::signer::Signer as _;
    use solana_sdk::{bpf_loader_upgradeable, keccak, system_program};
    use solana_test_validator::UpgradeableProgramInfo;

    use crate::SolanaEventForwarder;

    #[test_log::test(tokio::test)]
    async fn event_forwarding_only_call_contract() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let payload = "msg memo only".to_owned();
        let payload_hash = keccak::hash(payload.as_bytes()).0;
        let destination_chain = "evm".to_owned();
        let destination_contract = "0xdeadbeef".to_owned();
        let ix = axelar_solana_memo_program::instruction::call_gateway_with_memo(
            &fixture.gateway_root_pda,
            &counter_pda.0,
            payload.clone(),
            destination_chain.clone(),
            destination_contract.clone(),
            &axelar_solana_gateway::id(),
        )
        .unwrap();
        let only_call_contract_sig = fixture.send_tx_with_signatures(&[ix]).await.unwrap().0[0];

        let tx = fetch_transaction(
            CommitmentConfig::confirmed(),
            only_call_contract_sig,
            &rpc_client,
        )
        .await
        .unwrap()
        .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();
        // Extract the event_id and message_id from the received item
        let AmplifierCommand::PublishEvents(PublishEventsRequest { events }) = &item;
        let Some(Event::Call(event)) = events.first() else {
            panic!("Expected Call event");
        };

        let event_id = event.base.event_id.clone();
        let message_id = event.message.message_id.clone();

        let expected_event = CallEvent {
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(only_call_contract_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: None,
                    extra: CallEventMetadata {
                        parent_message_id: None,
                    },
                }),
            },
            message: GatewayV2Message {
                message_id,
                source_chain: "solana".to_owned(),
                source_address: axelar_solana_memo_program::ID.to_string(),
                destination_address: destination_contract.clone(),
                payload_hash: payload_hash.to_vec(),
            },
            destination_chain,
            payload: payload.into_bytes(),
        };

        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::Call(expected_event)])
                    .build()
            )
        );
    }

    #[test_log::test(tokio::test)]
    async fn event_forwarding_message_approved() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, _counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let mut signatures_to_sum = vec![];
        let payload = "msg memo only".to_owned();
        let payload_hash = keccak::hash(payload.as_bytes()).0;
        let source_address = "0xdeadbeef".to_owned();
        let cc_id_id = "0xhash-123".to_owned();
        let message = Message {
            cc_id: CrossChainId {
                chain: "ethereum".to_owned(),
                id: cc_id_id.clone(),
            },
            source_address: source_address.clone(),
            destination_chain: "solana".to_owned(),
            destination_address: axelar_solana_memo_program::ID.to_string(),
            payload_hash,
        };
        let messages = [message];
        let (execute_data, verification_pda, messages) =
            verify_signatures(&messages, &mut fixture, &mut signatures_to_sum).await;
        let message = messages[0].clone();
        let (command_id, approve_signature) = approve_message(
            message,
            &execute_data,
            &mut fixture,
            verification_pda,
            &mut signatures_to_sum,
        )
        .await;

        let tx = fetch_transaction(
            CommitmentConfig::confirmed(),
            approve_signature,
            &rpc_client,
        )
        .await
        .unwrap()
        .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();

        let mut expected_sum = 0_u64;
        for sig in signatures_to_sum {
            let tx = fetch_transaction(CommitmentConfig::confirmed(), sig, &rpc_client)
                .await
                .unwrap()
                .unwrap();
            expected_sum = expected_sum.saturating_add(tx.cost_in_lamports);
        }

        // Extract the event_id from the received item
        let AmplifierCommand::PublishEvents(PublishEventsRequest { events }) = &item;
        let Some(Event::MessageApproved(event)) = events.first() else {
            panic!("Expected MessageApproved event");
        };
        let event_id = event.base.event_id.clone();

        let event = MessageApprovedEvent {
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(approve_signature.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: None,
                    extra: MessageApprovedEventMetadata {
                        command_id: Some(CommandId(bs58::encode(command_id).into_string())),
                    },
                }),
            },
            message: GatewayV2Message {
                message_id: TxEvent(cc_id_id.clone()),
                source_chain: "ethereum".to_owned(),
                source_address: "0xdeadbeef".to_owned(),
                destination_address: axelar_solana_memo_program::ID.to_string(),
                payload_hash: payload_hash.to_vec(),
            },
            cost: Token {
                token_id: None,
                amount: BigInt::from_u64(expected_sum),
            },
        };
        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::MessageApproved(event)])
                    .build()
            )
        );
    }

    #[test_log::test(tokio::test)]
    async fn event_forwarding_two_message_approved() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, _counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let payload = "msg memo only".to_owned();
        let payload_hash = keccak::hash(payload.as_bytes()).0;
        let source_address = "0xdeadbeef".to_owned();
        let cc_id_id = "0xhash-123".to_owned();
        let message_one = Message {
            cc_id: CrossChainId {
                chain: "ethereum".to_owned(),
                id: cc_id_id.clone(),
            },
            source_address: source_address.clone(),
            destination_chain: "solana".to_owned(),
            destination_address: axelar_solana_memo_program::ID.to_string(),
            payload_hash,
        };
        let message_two = Message {
            cc_id: CrossChainId {
                chain: "ethereum".to_owned(),
                id: "0xhash-333".to_owned(),
            },
            source_address: source_address.clone(),
            destination_chain: "solana".to_owned(),
            destination_address: axelar_solana_memo_program::ID.to_string(),
            payload_hash,
        };
        let messages = [message_one, message_two];
        let mut verify_sigs = vec![];
        let (execute_data, verification_pda, messages) =
            verify_signatures(&messages, &mut fixture, &mut verify_sigs).await;
        let message_one = messages[0].clone();
        let message_two = messages[1].clone();
        let mut approve_sigs = vec![];
        let (command_id, approve_signature) = approve_message(
            message_one,
            &execute_data,
            &mut fixture,
            verification_pda,
            &mut approve_sigs,
        )
        .await;
        approve_message(
            message_two,
            &execute_data,
            &mut fixture,
            verification_pda,
            &mut Vec::new(),
        )
        .await;

        let tx = fetch_transaction(
            CommitmentConfig::confirmed(),
            approve_signature,
            &rpc_client,
        )
        .await
        .unwrap()
        .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();

        let mut expected_sum = 0_u64;
        for sig in &verify_sigs {
            let tx = fetch_transaction(CommitmentConfig::confirmed(), *sig, &rpc_client)
                .await
                .unwrap()
                .unwrap();
            // because we have 2 msgs in the array, the signature verification cost is split between
            // them
            expected_sum =
                expected_sum.saturating_add(tx.cost_in_lamports.checked_div(2).unwrap_or(0));
        }
        for sig in &approve_sigs {
            let tx = fetch_transaction(CommitmentConfig::confirmed(), *sig, &rpc_client)
                .await
                .unwrap()
                .unwrap();
            expected_sum = expected_sum.saturating_add(tx.cost_in_lamports);
        }

        // Extract the event_id from the received item
        let AmplifierCommand::PublishEvents(PublishEventsRequest { events }) = &item;
        let Some(Event::MessageApproved(event)) = events.first() else {
            panic!("Expected MessageApproved event");
        };
        let event_id = event.base.event_id.clone();

        let event = MessageApprovedEvent {
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(approve_signature.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: None,
                    extra: MessageApprovedEventMetadata {
                        command_id: Some(CommandId(bs58::encode(command_id).into_string())),
                    },
                }),
            },
            message: GatewayV2Message {
                message_id: TxEvent(cc_id_id.clone()),
                source_chain: "ethereum".to_owned(),
                source_address: "0xdeadbeef".to_owned(),
                destination_address: axelar_solana_memo_program::ID.to_string(),
                payload_hash: payload_hash.to_vec(),
            },
            cost: Token {
                token_id: None,
                amount: BigInt::from_u64(expected_sum),
            },
        };
        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::MessageApproved(event)])
                    .build()
            )
        );
    }

    #[test_log::test(tokio::test)]
    async fn event_forwarding_execute_message() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let bytes = b"msg memo only";
        let payload = build_memo(bytes, &counter_pda.0, &[], EncodingScheme::Borsh);
        let encoded_payload = payload.encode().unwrap();
        let payload_hash = keccak::hash(encoded_payload.as_slice()).0;
        let source_address = "0xdeadbeef".to_owned();
        let cc_id_id = "0xhash-123".to_owned();
        let message = Message {
            cc_id: CrossChainId {
                chain: "ethereum".to_owned(),
                id: cc_id_id.clone(),
            },
            source_address: source_address.clone(),
            destination_chain: "solana".to_owned(),
            destination_address: axelar_solana_memo_program::ID.to_string(),
            payload_hash,
        };

        let command_id = command_id(&message.cc_id.chain, &message.cc_id.id);
        let gateway_root_pda = fixture.gateway_root_pda;
        let payer = fixture.payer.pubkey();

        fixture
            .sign_session_and_approve_messages(&fixture.signers.clone(), &[message.clone()])
            .await
            .unwrap();
        let init_payload_sig = fixture
            .send_tx_with_signatures(&[
                axelar_solana_gateway::instructions::initialize_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                    encoded_payload
                        .len()
                        .try_into()
                        .expect("Unexpected u64 overflow in buffer size"),
                )
                .unwrap(),
            ])
            .await
            .unwrap()
            .0[0];

        let write_sig_1 = fixture
            .send_tx_with_signatures(
                &[axelar_solana_gateway::instructions::write_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                    &(encoded_payload[0..10]),
                    0,
                )
                .unwrap()],
            )
            .await
            .unwrap()
            .0[0];
        let write_sig_2 = fixture
            .send_tx_with_signatures(
                &[axelar_solana_gateway::instructions::write_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                    &(encoded_payload[10..]),
                    10,
                )
                .unwrap()],
            )
            .await
            .unwrap()
            .0[0];

        let commit_sig = fixture
            .send_tx_with_signatures(&[
                axelar_solana_gateway::instructions::commit_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                )
                .unwrap(),
            ])
            .await
            .unwrap()
            .0[0];

        let (incoming_message_pda, _bump) =
            axelar_solana_gateway::get_incoming_message_pda(&command_id);
        let (message_payload_pda, _bump) =
            axelar_solana_gateway::find_message_payload_pda(incoming_message_pda, payer);

        let (incoming_message_pda, _bump) = get_incoming_message_pda(&command_id);
        let (execute_sigs, _execute_tx) = fixture
            .send_tx_with_signatures(&[
                axelar_solana_gateway::executable::construct_axelar_executable_ix(
                    payer,
                    &message,
                    &encoded_payload,
                    incoming_message_pda,
                    message_payload_pda,
                )
                .unwrap(),
            ])
            .await
            .unwrap();
        let execute_sig = execute_sigs[0];

        // Close message payload and reclaim lamports
        let close_sig = fixture
            .send_tx_with_signatures(
                &[axelar_solana_gateway::instructions::close_message_payload(
                    gateway_root_pda,
                    payer,
                    command_id,
                )
                .unwrap()],
            )
            .await
            .unwrap()
            .0[0];

        let signatures = [
            close_sig,
            execute_sig,
            commit_sig,
            write_sig_1,
            write_sig_2,
            init_payload_sig,
        ];

        let total_cost = stream::iter(signatures)
            .then(|sig| fetch_transaction(CommitmentConfig::confirmed(), sig, &rpc_client))
            .try_fold(0_u64, |acc, tx_status| async move {
                match tx_status {
                    TxStatus::Successful(tx) => Ok(acc.saturating_add(tx.cost_in_lamports)),
                    TxStatus::Failed { tx, error } => {
                        panic!("Transaction {} failed with error: {}", tx.signature, error)
                    }
                }
            })
            .await
            .unwrap();

        let tx = fetch_transaction(CommitmentConfig::confirmed(), execute_sig, &rpc_client)
            .await
            .unwrap()
            .unwrap();

        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();
        // Extract the event_id and message_id from the received item
        let AmplifierCommand::PublishEvents(PublishEventsRequest { events }) = &item;
        let Some(Event::MessageExecuted(event)) = events.first() else {
            panic!("Expected MessageExecuted event");
        };

        let event_id = event.base.event_id.clone();
        let message_id = event.message_id.clone();
        let event = MessageExecutedEvent {
            status: MessageExecutionStatus::Successful,
            source_chain: "ethereum".to_owned(),
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(execute_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: None,
                    extra: MessageExecutedEventMetadata {
                        command_id: Some(CommandId(bs58::encode(command_id).into_string())),
                        child_message_ids: None,
                    },
                }),
            },
            message_id,
            cost: Token {
                token_id: None,
                amount: BigInt::from_u64(total_cost),
            },
        };

        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::MessageExecuted(event)])
                    .build()
            )
        );
    }
    async fn approve_message(
        message: axelar_solana_encoding::types::execute_data::MerkleisedMessage,
        execute_data: &axelar_solana_encoding::types::execute_data::ExecuteData,
        fixture: &mut SolanaAxelarIntegrationMetadata,
        verification_pda: Pubkey,
        signatures_to_sum: &mut Vec<Signature>,
    ) -> ([u8; 32], Signature) {
        let command_id = command_id(
            &message.leaf.message.cc_id.chain,
            &message.leaf.message.cc_id.id,
        );

        let (incoming_message_pda, _incoming_message_pda_bump) =
            get_incoming_message_pda(&command_id);

        let ix = axelar_solana_gateway::instructions::approve_message(
            message,
            execute_data.payload_merkle_root,
            fixture.gateway_root_pda,
            fixture.payer.pubkey(),
            verification_pda,
            incoming_message_pda,
        )
        .unwrap();
        let (sigs, ..) = fixture.send_tx_with_signatures(&[ix]).await.unwrap();
        let approve_signature = sigs[0];
        signatures_to_sum.push(approve_signature);
        (command_id, approve_signature)
    }

    async fn verify_signatures(
        messages: &[Message],
        fixture: &mut SolanaAxelarIntegrationMetadata,
        signatures_to_sum: &mut Vec<Signature>,
    ) -> (
        axelar_solana_encoding::types::execute_data::ExecuteData,
        Pubkey,
        Vec<axelar_solana_encoding::types::execute_data::MerkleisedMessage>,
    ) {
        let payload = Payload::Messages(Messages(messages.to_vec()));
        let execute_data = fixture.construct_execute_data(&fixture.signers.clone(), payload);
        let ix = axelar_solana_gateway::instructions::initialize_payload_verification_session(
            fixture.payer.pubkey(),
            fixture.gateway_root_pda,
            execute_data.payload_merkle_root,
            execute_data.signing_verifier_set_merkle_root,
        )
        .unwrap();
        let sigs = fixture.send_tx_with_signatures(&[ix]).await.unwrap().0;
        signatures_to_sum.push(sigs[0]);

        let (verifier_set_tracker_pda, _verifier_set_tracker_bump) =
            get_verifier_set_tracker_pda(execute_data.signing_verifier_set_merkle_root);

        let (verification_pda, _bump) = axelar_solana_gateway::get_signature_verification_pda(
            &execute_data.payload_merkle_root,
            &execute_data.signing_verifier_set_merkle_root,
        );

        for signature_leaves in &execute_data.signing_verifier_set_leaves {
            // Verify the signature
            let ix = axelar_solana_gateway::instructions::verify_signature(
                fixture.gateway_root_pda,
                verifier_set_tracker_pda,
                verification_pda,
                execute_data.payload_merkle_root,
                signature_leaves.clone(),
            )
            .unwrap();
            let (sigs, ..) = fixture
                .send_tx_with_signatures(&[
                    ComputeBudgetInstruction::set_compute_unit_limit(250_000),
                    ix,
                ])
                .await
                .unwrap();
            signatures_to_sum.push(sigs[0]);
        }

        // Check that the PDA contains the expected data
        let MerkleisedPayload::NewMessages { messages } = execute_data.payload_items.clone() else {
            unreachable!("we constructed a message batch");
        };

        (execute_data, verification_pda, messages)
    }

    fn setup_forwarder(
        rpc_client: &Arc<RpcClient>,
    ) -> (
        futures::channel::mpsc::UnboundedReceiver<AmplifierCommand>,
        futures::channel::mpsc::UnboundedSender<solana_listener::SolanaTransaction>,
    ) {
        let commitment = CommitmentConfig::confirmed();
        let config = crate::Config {
            source_chain_name: "solana".to_owned(),
            gateway_program_id: axelar_solana_gateway::id(),
            gas_service_program_id: axelar_solana_gas_service::id(),
            rpc: Arc::clone(rpc_client),
            commitment,
        };
        let (tx_amplifier, rx_amplifier) = futures::channel::mpsc::unbounded();
        let (tx_listener, rx_listener) = futures::channel::mpsc::unbounded();
        let amplifier_client = AmplifierCommandClient {
            sender: tx_amplifier,
        };
        let solana_listener_client = SolanaListenerClient {
            log_receiver: rx_listener,
        };
        let event_forwarder =
            SolanaEventForwarder::new(config, solana_listener_client, amplifier_client);
        let _task = tokio::spawn(event_forwarder.process_internal());
        (rx_amplifier, tx_listener)
    }

    #[test_log::test(tokio::test)]
    async fn event_forwrding_only_gas_event() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, _counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        // solana memo program to evm raw message
        let signature_to_fund = Signature::from([111; 64]);
        let ix_idx = 0;
        let event_idx = 0;
        let message_id = format!("{signature_to_fund}-{ix_idx}.{event_idx}");

        let refund_address = Pubkey::new_unique();
        let amount_to_refund = 5000;
        let gas_ix = axelar_solana_gas_service::instructions::add_gas_instruction(
            &fixture.payer.pubkey(),
            message_id.clone(),
            amount_to_refund,
            refund_address,
        )
        .unwrap();
        let only_gas_add_sig = *fixture
            .send_tx_with_signatures(&[gas_ix])
            .await
            .unwrap()
            .0
            .first()
            .unwrap();

        let rpc_client_url = match fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                ref validator,
                ..
            } => validator.rpc_url(),
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::ProgramTest { .. } => {
                unimplemented!()
            }
        };
        let rpc_client =
            retrying_solana_http_sender::new_client(&retrying_solana_http_sender::Config {
                max_concurrent_rpc_requests: 10,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });
        let tx = fetch_transaction(CommitmentConfig::confirmed(), only_gas_add_sig, &rpc_client)
            .await
            .unwrap()
            .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let item = rx_amplifier.next().await.unwrap();

        // Extract the event_id from the received item
        let AmplifierCommand::PublishEvents(PublishEventsRequest { events }) = &item;
        let Some(Event::GasCredit(event)) = events.first() else {
            panic!("Expected GasCredit event");
        };
        let event_id = event.base.event_id.clone();

        let expected_event = GasCreditEvent {
            base: EventBase {
                event_id,
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(only_gas_add_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: None,
                    extra: (),
                }),
            },
            message_id: TxEvent(message_id),
            refund_address: refund_address.to_string(),
            payment: Token {
                token_id: None,
                amount: BigInt::from_u64(amount_to_refund),
            },
        };

        assert_eq!(
            item,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![Event::GasCredit(expected_event)])
                    .build()
            )
        );
    }

    #[test_log::test(tokio::test)]
    async fn event_forwrding_with_gas_and_contract_call() {
        // setup
        let (mut fixture, rpc_client) = setup().await;
        let (_gas_config, _gas_init_sig, counter_pda, _init_memo_sig) =
            setup_aux_contracts(&mut fixture).await;
        let (mut rx_amplifier, mut tx_listener) = setup_forwarder(&rpc_client);

        let payload = "msg memo and gas".to_owned();
        let destination_chain_name = "evm".to_owned();
        let payload_hash = solana_sdk::keccak::hashv(&[payload.as_bytes()]).0;
        let destination_address = "0xdeadbeef".to_owned();
        let ix = axelar_solana_memo_program::instruction::call_gateway_with_memo(
            &fixture.gateway_root_pda,
            &counter_pda.0,
            payload.clone(),
            destination_chain_name.clone(),
            destination_address.clone(),
            &axelar_solana_gateway::id(),
        )
        .unwrap();
        let refund_address = Pubkey::new_unique();
        let gas_fee_amount = 5000;
        let gas_ix = axelar_solana_gas_service::instructions::pay_gas_instruction(
            &fixture.payer.pubkey(),
            destination_chain_name.clone(),
            destination_address.clone(),
            payload_hash,
            refund_address,
            gas_fee_amount,
        )
        .unwrap();
        let gas_and_call_contract_sig = fixture
            .send_tx_with_signatures(&[gas_ix, ix])
            .await
            .unwrap()
            .0[0];

        let rpc_client_url = match fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                ref validator,
                ..
            } => validator.rpc_url(),
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::ProgramTest { .. } => {
                unimplemented!()
            }
        };
        let rpc_client =
            retrying_solana_http_sender::new_client(&retrying_solana_http_sender::Config {
                max_concurrent_rpc_requests: 10,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });
        let tx = fetch_transaction(
            CommitmentConfig::confirmed(),
            gas_and_call_contract_sig,
            &rpc_client,
        )
        .await
        .unwrap()
        .unwrap();
        tx_listener.send(tx.clone()).await.unwrap();
        let items = rx_amplifier.next().await.unwrap();
        // Extract the event_id and message_id from the received item
        let AmplifierCommand::PublishEvents(PublishEventsRequest { events }) = &items;
        let mut events_iter = events.iter();
        let Some(Event::Call(call_event)) = events_iter.next() else {
            panic!("Expected Call event");
        };
        let Some(Event::GasCredit(gas_credit_event)) = events_iter.next() else {
            panic!("Expected GasCredit event");
        };
        let call_event_id = call_event.base.event_id.clone();
        let gas_credit_event_id = gas_credit_event.base.event_id.clone();
        let message_id = call_event.message.message_id.clone();
        let expected_call_event = CallEvent {
            base: EventBase {
                event_id: call_event_id.clone(),
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(gas_and_call_contract_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: None,
                    extra: CallEventMetadata {
                        parent_message_id: None,
                    },
                }),
            },
            message: GatewayV2Message {
                message_id: message_id.clone(),
                source_chain: "solana".to_owned(),
                source_address: axelar_solana_memo_program::ID.to_string(),
                destination_address: destination_address.clone(),
                payload_hash: payload_hash.to_vec(),
            },
            destination_chain: destination_chain_name.clone(),
            payload: payload.into_bytes(),
        };
        let expected_gas_event = GasCreditEvent {
            base: EventBase {
                event_id: gas_credit_event_id.clone(),
                meta: Some(EventMetadata {
                    tx_id: Some(TxId(gas_and_call_contract_sig.to_string())),
                    timestamp: tx.timestamp,
                    from_address: None,
                    finalized: None,
                    extra: (),
                }),
            },
            message_id,
            refund_address: refund_address.to_string(),
            payment: Token {
                token_id: None,
                amount: BigInt::from_u64(gas_fee_amount),
            },
        };

        assert_eq!(
            items,
            AmplifierCommand::PublishEvents(
                PublishEventsRequest::builder()
                    .events(vec![
                        Event::Call(expected_call_event),
                        Event::GasCredit(expected_gas_event)
                    ])
                    .build()
            )
        );
    }

    pub(crate) async fn setup_aux_contracts(
        fixture: &mut SolanaAxelarIntegrationMetadata,
    ) -> (
        axelar_solana_gateway_test_fixtures::gas_service::GasServiceUtils,
        Signature,
        (Pubkey, u8),
        Signature,
    ) {
        // init gas config
        let gas_service_upgr_auth = fixture.payer.insecure_clone();
        let gas_config = fixture.setup_default_gas_config(gas_service_upgr_auth.insecure_clone());
        let ix = axelar_solana_gas_service::instructions::init_config(
            &fixture.payer.pubkey(),
            &gas_config.operator.pubkey(),
        )
        .unwrap();
        let payer = fixture.payer.insecure_clone();
        let gas_init_sig = *fixture
            .send_tx_with_custom(
                &payer.pubkey(),
                &[ix],
                &[payer, gas_config.operator.insecure_clone()],
            )
            .await
            .unwrap()
            .0
            .first()
            .unwrap();

        // init memo program
        let counter_pda = axelar_solana_memo_program::get_counter_pda();
        let ix = axelar_solana_memo_program::instruction::initialize(
            &fixture.payer.pubkey(),
            &counter_pda,
        )
        .unwrap();
        let init_memo_sig = fixture.send_tx_with_signatures(&[ix]).await.unwrap().0[0];
        (gas_config, gas_init_sig, counter_pda, init_memo_sig)
    }

    /// Return the [`PathBuf`] that points to the `[repo]` folder
    #[must_use]
    pub(crate) fn workspace_root_dir() -> PathBuf {
        let dir = std::env::var("CARGO_MANIFEST_DIR")
            .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
        PathBuf::from(dir)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned()
    }

    pub(crate) async fn setup() -> (SolanaAxelarIntegrationMetadata, Arc<RpcClient>) {
        use solana_test_validator::TestValidatorGenesis;
        let mut validator = TestValidatorGenesis::default();

        let mut rpc_config = JsonRpcConfig::default_for_test();
        rpc_config.enable_rpc_transaction_history = true;
        rpc_config.enable_extended_tx_metadata_storage = true;
        validator.rpc_config(rpc_config);

        let mut pubsub_config = PubSubConfig::default_for_tests();
        pubsub_config.enable_block_subscription = true;
        validator.pubsub_config(pubsub_config);

        let upgrade_authority = Keypair::new();
        validator.add_account(
            upgrade_authority.pubkey(),
            AccountSharedData::new(u64::MAX, 0, &system_program::ID),
        );
        validator.add_upgradeable_programs_with_path(&[
            UpgradeableProgramInfo {
                program_id: axelar_solana_gateway::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_gateway.so"),
            },
            UpgradeableProgramInfo {
                program_id: axelar_solana_gas_service::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_gas_service.so"),
            },
            UpgradeableProgramInfo {
                program_id: axelar_solana_memo_program::id(),
                loader: bpf_loader_upgradeable::id(),
                upgrade_authority: upgrade_authority.pubkey(),
                program_path: workspace_root_dir()
                    .join("tests")
                    .join("fixtures")
                    .join("axelar_solana_memo_program.so"),
            },
        ]);

        let forced_sleep = if std::env::var("CI").is_ok() {
            Duration::from_millis(1000)
        } else {
            Duration::from_millis(500)
        };
        let mut fixture = TestFixture::new_test_validator(validator, forced_sleep).await;
        let init_payer = fixture.payer.insecure_clone();
        fixture.payer = upgrade_authority.insecure_clone();

        let operator = Keypair::new();
        let domain_separator = [42; 32];
        let initial_signers = make_verifiers_with_quorum(&[42, 33, 26], 0, 100, domain_separator);
        let mut fixture = SolanaAxelarIntegrationMetadata {
            domain_separator,
            upgrade_authority,
            fixture,
            signers: initial_signers,
            gateway_root_pda: axelar_solana_gateway::get_gateway_root_config_pda().0,
            operator,
            previous_signers_retention: 16,
            minimum_rotate_signers_delay_seconds: 1,
        };

        fixture.initialize_gateway_config_account().await.unwrap();
        fixture.payer = init_payer;

        let rpc_client_url = match fixture.fixture.test_node {
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::TestValidator {
                ref validator,
                ..
            } => validator.rpc_url(),
            axelar_solana_gateway_test_fixtures::base::TestNodeMode::ProgramTest { .. } => {
                unimplemented!()
            }
        };
        let rpc_client =
            retrying_solana_http_sender::new_client(&retrying_solana_http_sender::Config {
                max_concurrent_rpc_requests: 10,
                solana_http_rpc: rpc_client_url.parse().unwrap(),
                commitment: CommitmentConfig::confirmed(),
            });

        (fixture, rpc_client)
    }
}

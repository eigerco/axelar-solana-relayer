use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use relayer_amplifier_api_integration::amplifier_api::types as amp_types;
use solana_listener::SolanaTransaction as ListenerTransaction;
use solana_transaction_parser::gmp_types as core_types;
use solana_transaction_parser::types::SolanaTransaction as ParserTransaction;
use tracing::warn;

/// Convert from the listener's SolanaTransaction to the parser's Transaction type
pub fn convert_to_parser_transaction(tx: &ListenerTransaction) -> ParserTransaction {
    ParserTransaction {
        signature: tx.signature,
        timestamp: tx.timestamp,
        logs: tx.logs.clone(),
        slot: tx.slot as i64,
        cost_units: tx.cost_in_lamports,
        account_keys: tx.account_keys.iter().map(|k| k.to_string()).collect(),
        ixs: tx.inner_ixs.clone(),
    }
}

/// Convert a list of core (parser-produced) events into Amplifier API events
#[must_use]
pub fn map_core_events_to_amplifier(
    core_events: Vec<core_types::Event>,
    total_cost: u64,
) -> Vec<amp_types::Event> {
    let price_per_event = total_cost
        .checked_div(
            core_events
                .len()
                .try_into()
                .expect("number of events should fit into u64"),
        )
        .unwrap_or(0);

    core_events
        .into_iter()
        .filter_map(|e| convert_core_event_to_amp(e, price_per_event))
        .collect()
}

fn convert_core_event_to_amp(
    event: core_types::Event,
    adjusted_cost: u64,
) -> Option<amp_types::Event> {
    match event {
        core_types::Event::Call {
            common,
            message,
            destination_chain,
            payload,
        } => convert_call(common, message, destination_chain, payload).map(amp_types::Event::Call),

        core_types::Event::GasCredit {
            common,
            message_id,
            refund_address,
            payment,
        } => Some(amp_types::Event::GasCredit(amp_types::GasCreditEvent {
            base: convert_base_meta(common.event_id, common.meta),
            message_id: amp_types::TxEvent(message_id),
            refund_address,
            payment: token_from_amount(payment),
        })),

        core_types::Event::GasRefunded {
            common,
            message_id,
            recipient_address,
            refunded_amount,
            mut cost,
        } => {
            cost.amount = adjusted_cost.to_string();

            Some(amp_types::Event::GasRefunded(amp_types::GasRefundedEvent {
                base: convert_base_meta(common.event_id, common.meta),
                message_id: amp_types::TxEvent(message_id),
                recipient_address,
                refunded_amount: token_from_amount(refunded_amount),
                cost: token_from_amount(cost),
            }))
        }

        core_types::Event::MessageApproved {
            common,
            message,
            mut cost,
        } => {
            cost.amount = adjusted_cost.to_string();
            convert_message_approved(common, message, cost).map(amp_types::Event::MessageApproved)
        }

        core_types::Event::MessageExecuted {
            common,
            message_id,
            source_chain,
            status,
            mut cost,
        } => {
            cost.amount = adjusted_cost.to_string();
            convert_message_executed(common, message_id, source_chain, status, cost)
                .map(amp_types::Event::MessageExecuted)
        }

        core_types::Event::SignersRotated { common, message_id } => {
            convert_signers_rotated(common, message_id).map(amp_types::Event::SignersRotated)
        }

        core_types::Event::ITSInterchainTransfer {
            common,
            message_id,
            destination_chain,
            token_spent,
            source_address,
            destination_address,
            data_hash,
        } => convert_its_interchain_transfer(
            common,
            message_id,
            destination_chain,
            token_spent,
            source_address,
            destination_address,
            data_hash,
        )
        .map(amp_types::Event::ItsInterchainTransfer),

        core_types::Event::ITSTokenMetadataRegistered {
            common,
            message_id,
            address,
            decimals,
        } => convert_its_token_metadata_registered(common, message_id, address, decimals)
            .map(amp_types::Event::ItsTokenMetadataRegistered),

        core_types::Event::ITSLinkTokenStarted {
            common,
            message_id,
            token_id,
            destination_chain,
            source_token_address,
            destination_token_address,
            token_manager_type,
        } => convert_its_link_token_started(
            common,
            message_id,
            token_id,
            destination_chain,
            source_token_address,
            destination_token_address,
            token_manager_type,
        )
        .map(amp_types::Event::ItsLinkTokenStarted),

        core_types::Event::ITSInterchainTokenDeploymentStarted {
            common,
            message_id,
            destination_chain,
            token,
        } => convert_its_interchain_token_deployment_started(
            common,
            message_id,
            destination_chain,
            token,
        )
        .map(amp_types::Event::ItsInterchainTokenDeploymentStarted),

        core_types::Event::CannotExecuteMessageV2 { .. } => {
            warn!("Skipping CannotExecuteMessageV2, not part of the Programs");
            None
        }
    }
}

fn convert_call(
    common: core_types::CommonEventFields<core_types::EventMetadata>,
    message: core_types::GatewayV2Message,
    destination_chain: String,
    payload_b64: String,
) -> Option<amp_types::CallEvent> {
    let payload = match BASE64_STANDARD.decode(payload_b64) {
        Ok(p) => p,
        Err(err) => {
            warn!(?err, "invalid base64 payload for Call event; skipping");
            return None;
        }
    };

    let message = convert_gateway_message(message)?;
    let event_id = amp_types::TxEvent(common.event_id);
    let meta = convert_event_metadata(common.meta).map(|m| amp_types::EventMetadata::<
        amp_types::CallEventMetadata,
    > {
        tx_id: m.tx_id,
        timestamp: m.timestamp,
        from_address: m.from_address,
        finalized: m.finalized,
        // TODO: Eventually support this for multihop calls
        extra: amp_types::CallEventMetadata {
            parent_message_id: None,
        },
    });

    Some(amp_types::CallEvent {
        base: amp_types::EventBase::<amp_types::CallEventMetadata> { event_id, meta },
        message,
        destination_chain,
        payload,
    })
}

fn convert_message_approved(
    common: core_types::CommonEventFields<core_types::MessageApprovedEventMetadata>,
    message: core_types::GatewayV2Message,
    cost: core_types::Amount,
) -> Option<amp_types::MessageApprovedEvent> {
    let (meta_common, command_id) = match common.meta {
        Some(m) => (Some(m.common_meta), m.command_id),
        None => (None, None),
    };

    let message = convert_gateway_message(message)?;
    let event_id = amp_types::TxEvent(common.event_id);
    let cmd = command_id.map(amp_types::CommandId);

    let meta = convert_event_metadata(meta_common).map(|m| amp_types::EventMetadata::<
        amp_types::MessageApprovedEventMetadata,
    > {
        tx_id: m.tx_id,
        timestamp: m.timestamp,
        from_address: m.from_address,
        finalized: m.finalized,
        extra: amp_types::MessageApprovedEventMetadata { command_id: cmd },
    });

    Some(amp_types::MessageApprovedEvent {
        base: amp_types::EventBase { event_id, meta },
        message,
        cost: token_from_amount(cost),
    })
}

fn convert_message_executed(
    common: core_types::CommonEventFields<core_types::MessageExecutedEventMetadata>,
    message_id: String,
    source_chain: String,
    status: core_types::MessageExecutionStatus,
    cost: core_types::Amount,
) -> Option<amp_types::MessageExecutedEvent> {
    let (meta_common, command_id, child_message_ids) = match common.meta {
        Some(m) => (Some(m.common_meta), m.command_id, m.child_message_ids),
        None => (None, None, None),
    };

    let event_id = amp_types::TxEvent(common.event_id);
    let cmd = command_id.map(amp_types::CommandId);
    let child = child_message_ids.map(|v| v.into_iter().map(amp_types::TxEvent).collect());

    let meta = convert_event_metadata(meta_common).map(|m| amp_types::EventMetadata::<
        amp_types::MessageExecutedEventMetadata,
    > {
        tx_id: m.tx_id,
        timestamp: m.timestamp,
        from_address: m.from_address,
        finalized: m.finalized,
        extra: amp_types::MessageExecutedEventMetadata {
            command_id: cmd,
            child_message_ids: child,
        },
    });

    Some(amp_types::MessageExecutedEvent {
        base: amp_types::EventBase { event_id, meta },
        message_id: amp_types::TxEvent(message_id),
        source_chain,
        status: match status {
            core_types::MessageExecutionStatus::SUCCESSFUL => {
                amp_types::MessageExecutionStatus::Successful
            }
            core_types::MessageExecutionStatus::REVERTED => {
                amp_types::MessageExecutionStatus::Reverted
            }
        },
        cost: token_from_amount(cost),
    })
}

fn convert_signers_rotated(
    common: core_types::CommonEventFields<core_types::SignersRotatedEventMetadata>,
    message_id: String,
) -> Option<amp_types::SignersRotatedEvent> {
    let (meta_common, signers_hash, epoch) = match common.meta {
        Some(m) => (Some(m.common_meta), m.signers_hash, m.epoch),
        None => (None, None, None),
    };

    let event_id = amp_types::TxEvent(common.event_id);

    let signer_hash = match signers_hash {
        Some(hash_b64) => match BASE64_STANDARD.decode(hash_b64) {
            Ok(hash) => hash,
            Err(err) => {
                warn!(
                    ?err,
                    "invalid base64 signers_hash in SignersRotated event; skipping"
                );
                return None;
            }
        },
        None => {
            warn!("missing signers_hash in SignersRotated event; skipping");
            return None;
        }
    };

    let epoch = match epoch {
        Some(e) => e,
        None => {
            warn!("missing epoch in SignersRotated event; skipping");
            return None;
        }
    };

    let meta = convert_event_metadata(meta_common).map(|m| amp_types::EventMetadata::<
        amp_types::SignersRotatedMetadata,
    > {
        tx_id: m.tx_id,
        timestamp: m.timestamp,
        from_address: m.from_address,
        finalized: m.finalized,
        extra: amp_types::SignersRotatedMetadata { signer_hash, epoch },
    });

    Some(amp_types::SignersRotatedEvent {
        base: amp_types::EventBase { event_id, meta },
        message_id: amp_types::TxEvent(message_id),
    })
}

fn convert_its_interchain_transfer(
    common: core_types::CommonEventFields<core_types::EventMetadata>,
    message_id: String,
    destination_chain: String,
    token_spent: core_types::Amount,
    source_address: String,
    destination_address: String,
    data_hash: String,
) -> Option<amp_types::ItsInterchainTransferEvent> {
    let token_id = match &token_spent.token_id {
        Some(id) => amp_types::TokenId(id.clone()),
        None => {
            warn!("missing token_id in ITSInterchainTransfer event; skipping");
            return None;
        }
    };

    let token = token_from_amount(token_spent);

    Some(amp_types::ItsInterchainTransferEvent {
        base: convert_base_meta(common.event_id, common.meta),
        message_id: amp_types::TxEvent(message_id),
        destination_chain,
        token_spent: amp_types::InterchainTransferTokenWithId {
            token_id,
            amount: token.amount,
        },
        source_address,
        destination_address,
        data_hash,
    })
}

fn convert_its_token_metadata_registered(
    common: core_types::CommonEventFields<core_types::EventMetadata>,
    message_id: String,
    address: String,
    decimals: u8,
) -> Option<amp_types::ItsTokenMetadataRegisteredEvent> {
    Some(amp_types::ItsTokenMetadataRegisteredEvent {
        base: convert_base_meta(common.event_id, common.meta),
        message_id: amp_types::TxEvent(message_id),
        address,
        decimals,
    })
}

fn convert_its_link_token_started(
    common: core_types::CommonEventFields<core_types::EventMetadata>,
    message_id: String,
    token_id: String,
    destination_chain: String,
    source_token_address: String,
    destination_token_address: String,
    token_manager_type: core_types::TokenManagerType,
) -> Option<amp_types::ItsLinkTokenStartedEvent> {
    Some(amp_types::ItsLinkTokenStartedEvent {
        base: convert_base_meta(common.event_id, common.meta),
        message_id: amp_types::TxEvent(message_id),
        token_id: amp_types::TokenId(token_id),
        destination_chain,
        source_token_address,
        destination_token_address,
        token_manager_type: convert_token_manager_type(token_manager_type),
    })
}

fn convert_its_interchain_token_deployment_started(
    common: core_types::CommonEventFields<core_types::EventMetadata>,
    message_id: String,
    destination_chain: String,
    token: core_types::InterchainTokenDefinition,
) -> Option<amp_types::ItsInterchainTokenDeploymentStartedEvent> {
    Some(amp_types::ItsInterchainTokenDeploymentStartedEvent {
        base: convert_base_meta(common.event_id, common.meta),
        message_id: amp_types::TxEvent(message_id),
        destination_chain,
        token: amp_types::InterchainTokenDefinition {
            id: amp_types::TokenId(token.id),
            name: token.name,
            symbol: token.symbol,
            decimals: token.decimals,
        },
    })
}

fn convert_token_manager_type(
    token_manager_type: core_types::TokenManagerType,
) -> amp_types::TokenManagerType {
    match token_manager_type {
        core_types::TokenManagerType::NativeInterchainToken => {
            amp_types::TokenManagerType::NativeInterchainToken
        }
        core_types::TokenManagerType::MintBurnFrom => amp_types::TokenManagerType::MintBurnFrom,
        core_types::TokenManagerType::LockUnlock => amp_types::TokenManagerType::LockUnlock,
        core_types::TokenManagerType::LockUnlockFee => amp_types::TokenManagerType::LockUnlockFee,
        core_types::TokenManagerType::MintBurn => amp_types::TokenManagerType::MintBurn,
    }
}

fn convert_gateway_message(
    msg: core_types::GatewayV2Message,
) -> Option<amp_types::GatewayV2Message> {
    let payload_hash = match BASE64_STANDARD.decode(msg.payload_hash) {
        Ok(p) => p,
        Err(err) => {
            warn!(
                ?err,
                "invalid base64 payloadHash in GatewayV2Message; skipping event"
            );
            return None;
        }
    };
    Some(amp_types::GatewayV2Message {
        message_id: amp_types::TxEvent(msg.message_id),
        source_chain: msg.source_chain,
        source_address: msg.source_address,
        destination_address: msg.destination_address,
        payload_hash,
    })
}

/// Helper to create base event metadata with event_id
fn convert_base_meta(
    event_id: String,
    meta: Option<core_types::EventMetadata>,
) -> amp_types::EventBase<()> {
    amp_types::EventBase {
        event_id: amp_types::TxEvent(event_id),
        meta: convert_event_metadata(meta),
    }
}

/// Helper to convert event metadata (without the event-specific extras)
fn convert_event_metadata(
    meta: Option<core_types::EventMetadata>,
) -> Option<amp_types::EventMetadata<()>> {
    meta.map(|m| {
        let tx_id = m.tx_id.map(amp_types::TxId);
        let timestamp = parse_timestamp(&m.timestamp);
        amp_types::EventMetadata::<()> {
            tx_id,
            timestamp,
            from_address: m.from_address,
            finalized: m.finalized,
            extra: (),
        }
    })
}

fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|x| x.to_utc())
}

fn token_from_amount(amount: core_types::Amount) -> amp_types::Token {
    let num = amp_types::bnum::types::I512::parse_str_radix(amount.amount.as_str(), 10);
    amp_types::Token {
        token_id: amount.token_id.map(amp_types::TokenId),
        amount: amp_types::BigInt::new(num),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Signature;
    use solana_transaction_status::{UiCompiledInstruction, UiInnerInstructions, UiInstruction};

    use super::*;

    #[test]
    fn test_transaction_conversion() {
        let sig = Signature::default();
        let timestamp = DateTime::<Utc>::from_timestamp(1678886400, 0);
        let logs = vec!["log1".to_string(), "log2".to_string()];
        let slot = 12345u64;
        let cost = 5000u64;
        let account_keys = vec![Pubkey::new_unique(), Pubkey::new_unique()];

        let inner_ixs = vec![UiInnerInstructions {
            index: 0,
            instructions: vec![UiInstruction::Compiled(UiCompiledInstruction {
                program_id_index: 1,
                accounts: vec![0, 1],
                data: "test_data".to_string(),
                stack_height: None,
            })],
        }];

        let listener_tx = ListenerTransaction {
            signature: sig,
            timestamp,
            logs: logs.clone(),
            ixs: vec![(
                Pubkey::new_unique(),
                vec![Pubkey::new_unique()],
                vec![1, 2, 3],
            )],
            inner_ixs: inner_ixs.clone(),
            slot,
            cost_in_lamports: cost,
            account_keys: account_keys.clone(),
        };

        let parser_tx = convert_to_parser_transaction(&listener_tx);

        assert_eq!(parser_tx.signature, sig);
        assert_eq!(parser_tx.timestamp, timestamp);
        assert_eq!(parser_tx.logs, logs);
        assert_eq!(parser_tx.slot, slot as i64);
        assert_eq!(parser_tx.cost_units, cost);
        assert_eq!(parser_tx.ixs, inner_ixs);
        assert_eq!(parser_tx.account_keys.len(), account_keys.len());

        for (idx, key) in account_keys.iter().enumerate() {
            assert_eq!(parser_tx.account_keys[idx], key.to_string());
        }
    }

    #[test]
    fn test_conversion_handles_missing_optional_fields() {
        let listener_tx = ListenerTransaction {
            signature: Signature::default(),
            timestamp: None,
            logs: vec![],
            ixs: vec![],
            inner_ixs: vec![],
            slot: 0,
            cost_in_lamports: 0,
            account_keys: vec![],
        };

        let parser_tx = convert_to_parser_transaction(&listener_tx);

        assert_eq!(parser_tx.signature, listener_tx.signature);
        assert!(parser_tx.timestamp.is_none());
        assert!(parser_tx.logs.is_empty());
        assert!(parser_tx.ixs.is_empty());
        assert_eq!(parser_tx.slot, 0);
        assert_eq!(parser_tx.cost_units, 0);
        assert!(parser_tx.account_keys.is_empty());
    }

    #[test]
    fn test_gas_credit_event_conversion() {
        let core_event = core_types::Event::GasCredit {
            common: core_types::CommonEventFields {
                r#type: "GAS_CREDIT".to_string(),
                event_id: "0xabc-1".to_string(),
                meta: Some(core_types::EventMetadata {
                    tx_id: Some("0xabc".to_string()),
                    from_address: Some("0x123".to_string()),
                    finalized: Some(true),
                    source_context: None,
                    timestamp: "2024-01-01T00:00:00Z".to_string(),
                }),
            },
            message_id: "0xabc-2".to_string(),
            refund_address: "0xrefund".to_string(),
            payment: core_types::Amount {
                token_id: None,
                amount: "1000".to_string(),
            },
        };

        let amp_events = map_core_events_to_amplifier(vec![core_event], 0);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::GasCredit(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xabc-1");
            assert_eq!(event.message_id.0, "0xabc-2");
            assert_eq!(event.refund_address, "0xrefund");
            assert_eq!(event.payment.amount.0.to_string(), "1000");
            assert!(event.base.meta.is_some());
            let meta = event.base.meta.as_ref().unwrap();
            assert_eq!(meta.tx_id.as_ref().unwrap().0, "0xabc");
            assert_eq!(meta.from_address.as_ref().unwrap(), "0x123");
            assert_eq!(meta.finalized, Some(true));
        } else {
            panic!("Expected GasCredit event");
        }
    }

    #[test]
    fn test_call_event_conversion() {
        let payload = vec![1, 2, 3, 4, 5];
        let payload_b64 = BASE64_STANDARD.encode(&payload);
        let payload_hash = vec![0xaa; 32];
        let payload_hash_b64 = BASE64_STANDARD.encode(&payload_hash);

        let core_event = core_types::Event::Call {
            common: core_types::CommonEventFields {
                r#type: "CALL".to_string(),
                event_id: "0xdef-3".to_string(),
                meta: Some(core_types::EventMetadata {
                    tx_id: Some("0xdef".to_string()),
                    from_address: Some("0x456".to_string()),
                    finalized: Some(true),
                    source_context: None,
                    timestamp: "2024-01-02T00:00:00Z".to_string(),
                }),
            },
            message: core_types::GatewayV2Message {
                message_id: "0xdef-3".to_string(),
                source_chain: "ethereum".to_string(),
                source_address: "0xsource".to_string(),
                destination_address: "0xdest".to_string(),
                payload_hash: payload_hash_b64,
            },
            destination_chain: "avalanche".to_string(),
            payload: payload_b64,
        };

        let amp_events = map_core_events_to_amplifier(vec![core_event], 0);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::Call(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xdef-3");
            assert_eq!(event.message.message_id.0, "0xdef-3");
            assert_eq!(event.message.source_chain, "ethereum");
            assert_eq!(event.message.source_address, "0xsource");
            assert_eq!(event.message.destination_address, "0xdest");
            assert_eq!(event.message.payload_hash, payload_hash);
            assert_eq!(event.destination_chain, "avalanche");
            assert_eq!(event.payload, payload);
            assert!(event.base.meta.is_some());
        } else {
            panic!("Expected Call event");
        }
    }

    #[test]
    fn test_message_approved_event_conversion() {
        let payload_hash = vec![0xbb; 32];
        let payload_hash_b64 = BASE64_STANDARD.encode(&payload_hash);

        let core_event = core_types::Event::MessageApproved {
            common: core_types::CommonEventFields {
                r#type: "MESSAGE_APPROVED".to_string(),
                event_id: "0xghi-4".to_string(),
                meta: Some(core_types::MessageApprovedEventMetadata {
                    common_meta: core_types::EventMetadata {
                        tx_id: Some("0xghi".to_string()),
                        from_address: Some("0x789".to_string()),
                        finalized: Some(false),
                        source_context: None,
                        timestamp: "2024-01-03T00:00:00Z".to_string(),
                    },
                    command_id: Some("cmd-123".to_string()),
                }),
            },
            message: core_types::GatewayV2Message {
                message_id: "0xghi-4".to_string(),
                source_chain: "polygon".to_string(),
                source_address: "0xpoly".to_string(),
                destination_address: "0xsol".to_string(),
                payload_hash: payload_hash_b64,
            },
            cost: core_types::Amount {
                token_id: None,
                amount: "500".to_string(),
            },
        };

        let amp_events = map_core_events_to_amplifier(vec![core_event], 500);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::MessageApproved(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xghi-4");
            assert!(event.base.meta.is_some());
            let meta = event.base.meta.as_ref().unwrap();
            assert_eq!(meta.extra.command_id.as_ref().unwrap().0, "cmd-123");
            assert_eq!(event.cost.amount.0.to_string(), "500");
        } else {
            panic!("Expected MessageApproved event");
        }
    }

    #[test]
    fn test_message_executed_event_conversion() {
        let core_event = core_types::Event::MessageExecuted {
            common: core_types::CommonEventFields {
                r#type: "MESSAGE_EXECUTED".to_string(),
                event_id: "0xjkl-5".to_string(),
                meta: Some(core_types::MessageExecutedEventMetadata {
                    common_meta: core_types::EventMetadata {
                        tx_id: Some("0xjkl".to_string()),
                        from_address: Some("0xabc".to_string()),
                        finalized: Some(true),
                        source_context: None,
                        timestamp: "2024-01-04T00:00:00Z".to_string(),
                    },
                    command_id: Some("cmd-456".to_string()),
                    child_message_ids: Some(vec!["child-1".to_string(), "child-2".to_string()]),
                    revert_reason: None,
                }),
            },
            message_id: "0xjkl-5".to_string(),
            source_chain: "arbitrum".to_string(),
            status: core_types::MessageExecutionStatus::SUCCESSFUL,
            cost: core_types::Amount {
                token_id: None,
                amount: "250".to_string(),
            },
        };

        let amp_events = map_core_events_to_amplifier(vec![core_event], 550);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::MessageExecuted(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xjkl-5");
            assert_eq!(event.message_id.0, "0xjkl-5");
            assert_eq!(event.source_chain, "arbitrum");
            assert!(matches!(
                event.status,
                amp_types::MessageExecutionStatus::Successful
            ));
            assert_eq!(event.cost.amount.0.to_string(), "550");
            assert!(event.base.meta.is_some());
            let meta = event.base.meta.as_ref().unwrap();
            assert_eq!(meta.extra.command_id.as_ref().unwrap().0, "cmd-456");
            assert_eq!(meta.extra.child_message_ids.as_ref().unwrap().len(), 2);
        } else {
            panic!("Expected MessageExecuted event");
        }
    }

    #[test]
    fn test_gas_refunded_event_conversion() {
        let core_event = core_types::Event::GasRefunded {
            common: core_types::CommonEventFields {
                r#type: "GAS_REFUNDED".to_string(),
                event_id: "0xmno-6".to_string(),
                meta: None,
            },
            message_id: "0xmno-7".to_string(),
            recipient_address: "0xrecipient".to_string(),
            refunded_amount: core_types::Amount {
                token_id: Some("USDC".to_string()),
                amount: "100".to_string(),
            },
            cost: core_types::Amount {
                token_id: None,
                amount: "10".to_string(),
            },
        };

        let amp_events = map_core_events_to_amplifier(vec![core_event], 30);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::GasRefunded(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xmno-6");
            assert_eq!(event.message_id.0, "0xmno-7");
            assert_eq!(event.recipient_address, "0xrecipient");
            assert_eq!(event.refunded_amount.amount.0.to_string(), "100");
            assert_eq!(event.refunded_amount.token_id.as_ref().unwrap().0, "USDC");
            assert_eq!(event.cost.amount.0.to_string(), "30");
            assert!(event.base.meta.is_none());
        } else {
            panic!("Expected GasRefunded event");
        }
    }

    #[test]
    fn test_skip_unsupported_events() {
        // ITS event without token_id should be skipped
        let its_event_missing_token = core_types::Event::ITSInterchainTransfer {
            common: core_types::CommonEventFields {
                r#type: "ITS_INTERCHAIN_TRANSFER".to_string(),
                event_id: "test".to_string(),
                meta: None,
            },
            message_id: "test".to_string(),
            destination_chain: "test".to_string(),
            token_spent: core_types::Amount {
                token_id: None,
                amount: "0".to_string(),
            },
            source_address: "test".to_string(),
            destination_address: "test".to_string(),
            data_hash: "test".to_string(),
        };

        let amp_events = map_core_events_to_amplifier(vec![its_event_missing_token], 0);
        assert_eq!(
            amp_events.len(),
            0,
            "ITS events without token_id should be skipped"
        );
    }

    #[test]
    fn test_signers_rotated_event_conversion() {
        let signer_hash = vec![0xaa; 32];
        let signer_hash_b64 = BASE64_STANDARD.encode(&signer_hash);

        let core_event = core_types::Event::SignersRotated {
            common: core_types::CommonEventFields {
                r#type: "SIGNERS_ROTATED".to_string(),
                event_id: "0xrot-1".to_string(),
                meta: Some(core_types::SignersRotatedEventMetadata {
                    common_meta: core_types::EventMetadata {
                        tx_id: Some("0xrot".to_string()),
                        from_address: Some("0xrotator".to_string()),
                        finalized: Some(true),
                        source_context: None,
                        timestamp: "2024-01-09T00:00:00Z".to_string(),
                    },
                    signers_hash: Some(signer_hash_b64),
                    epoch: Some(42),
                }),
            },
            message_id: "0xrot-msg".to_string(),
        };

        let amp_events = map_core_events_to_amplifier(vec![core_event], 0);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::SignersRotated(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xrot-1");
            assert_eq!(event.message_id.0, "0xrot-msg");
            assert!(event.base.meta.is_some());
            let meta = event.base.meta.as_ref().unwrap();
            assert_eq!(meta.extra.signer_hash, signer_hash);
            assert_eq!(meta.extra.epoch, 42);
            assert_eq!(meta.tx_id.as_ref().unwrap().0, "0xrot");
            assert_eq!(meta.from_address.as_ref().unwrap(), "0xrotator");
        } else {
            panic!("Expected SignersRotated event");
        }
    }

    #[test]
    fn test_its_interchain_transfer_event_conversion() {
        let its_event = core_types::Event::ITSInterchainTransfer {
            common: core_types::CommonEventFields {
                r#type: "ITS_INTERCHAIN_TRANSFER".to_string(),
                event_id: "0xits-1".to_string(),
                meta: Some(core_types::EventMetadata {
                    tx_id: Some("0xits".to_string()),
                    from_address: Some("0xsender".to_string()),
                    finalized: Some(true),
                    source_context: None,
                    timestamp: "2024-01-05T00:00:00Z".to_string(),
                }),
            },
            message_id: "0xits-msg".to_string(),
            destination_chain: "avalanche".to_string(),
            token_spent: core_types::Amount {
                token_id: Some("token123".to_string()),
                amount: "1000".to_string(),
            },
            source_address: "0xsrc".to_string(),
            destination_address: "0xdest".to_string(),
            data_hash: "0xhash".to_string(),
        };

        let amp_events = map_core_events_to_amplifier(vec![its_event], 0);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::ItsInterchainTransfer(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xits-1");
            assert_eq!(event.message_id.0, "0xits-msg");
            assert_eq!(event.destination_chain, "avalanche");
            assert_eq!(event.token_spent.token_id.0, "token123");
            assert_eq!(event.token_spent.amount.0.to_string(), "1000");
            assert_eq!(event.source_address, "0xsrc");
            assert_eq!(event.destination_address, "0xdest");
            assert_eq!(event.data_hash, "0xhash");
        } else {
            panic!("Expected ItsInterchainTransfer event");
        }
    }

    #[test]
    fn test_its_token_metadata_registered_event_conversion() {
        let its_event = core_types::Event::ITSTokenMetadataRegistered {
            common: core_types::CommonEventFields {
                r#type: "ITS_TOKEN_METADATA_REGISTERED".to_string(),
                event_id: "0xits-2".to_string(),
                meta: Some(core_types::EventMetadata {
                    tx_id: Some("0xits2".to_string()),
                    from_address: Some("0xregistrar".to_string()),
                    finalized: Some(true),
                    source_context: None,
                    timestamp: "2024-01-06T00:00:00Z".to_string(),
                }),
            },
            message_id: "0xits-msg-2".to_string(),
            address: "0xtokenaddr".to_string(),
            decimals: 18,
        };

        let amp_events = map_core_events_to_amplifier(vec![its_event], 0);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::ItsTokenMetadataRegistered(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xits-2");
            assert_eq!(event.message_id.0, "0xits-msg-2");
            assert_eq!(event.address, "0xtokenaddr");
            assert_eq!(event.decimals, 18);
            assert!(event.base.meta.is_some());
        } else {
            panic!("Expected ItsTokenMetadataRegistered event");
        }
    }

    #[test]
    fn test_its_link_token_started_event_conversion() {
        let its_event = core_types::Event::ITSLinkTokenStarted {
            common: core_types::CommonEventFields {
                r#type: "ITS_LINK_TOKEN_STARTED".to_string(),
                event_id: "0xits-3".to_string(),
                meta: Some(core_types::EventMetadata {
                    tx_id: Some("0xits3".to_string()),
                    from_address: Some("0xlinker".to_string()),
                    finalized: Some(false),
                    source_context: None,
                    timestamp: "2024-01-07T00:00:00Z".to_string(),
                }),
            },
            message_id: "0xits-msg-3".to_string(),
            token_id: "token-id-123".to_string(),
            destination_chain: "polygon".to_string(),
            source_token_address: "0xsrctoken".to_string(),
            destination_token_address: "0xdesttoken".to_string(),
            token_manager_type: core_types::TokenManagerType::LockUnlock,
        };

        let amp_events = map_core_events_to_amplifier(vec![its_event], 0);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::ItsLinkTokenStarted(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xits-3");
            assert_eq!(event.message_id.0, "0xits-msg-3");
            assert_eq!(event.token_id.0, "token-id-123");
            assert_eq!(event.destination_chain, "polygon");
            assert_eq!(event.source_token_address, "0xsrctoken");
            assert_eq!(event.destination_token_address, "0xdesttoken");
            assert!(matches!(
                event.token_manager_type,
                amp_types::TokenManagerType::LockUnlock
            ));
            assert!(event.base.meta.is_some());
        } else {
            panic!("Expected ItsLinkTokenStarted event");
        }
    }

    #[test]
    fn test_its_interchain_token_deployment_started_event_conversion() {
        let its_event = core_types::Event::ITSInterchainTokenDeploymentStarted {
            common: core_types::CommonEventFields {
                r#type: "ITS_INTERCHAIN_TOKEN_DEPLOYMENT_STARTED".to_string(),
                event_id: "0xits-4".to_string(),
                meta: Some(core_types::EventMetadata {
                    tx_id: Some("0xits4".to_string()),
                    from_address: Some("0xdeployer".to_string()),
                    finalized: Some(true),
                    source_context: None,
                    timestamp: "2024-01-08T00:00:00Z".to_string(),
                }),
            },
            message_id: "0xits-msg-4".to_string(),
            destination_chain: "arbitrum".to_string(),
            token: core_types::InterchainTokenDefinition {
                id: "token-def-456".to_string(),
                name: "Test Token".to_string(),
                symbol: "TEST".to_string(),
                decimals: 6,
            },
        };

        let amp_events = map_core_events_to_amplifier(vec![its_event], 0);
        assert_eq!(amp_events.len(), 1);

        if let amp_types::Event::ItsInterchainTokenDeploymentStarted(event) = &amp_events[0] {
            assert_eq!(event.base.event_id.0, "0xits-4");
            assert_eq!(event.message_id.0, "0xits-msg-4");
            assert_eq!(event.destination_chain, "arbitrum");
            assert_eq!(event.token.id.0, "token-def-456");
            assert_eq!(event.token.name, "Test Token");
            assert_eq!(event.token.symbol, "TEST");
            assert_eq!(event.token.decimals, 6);
            assert!(event.base.meta.is_some());
        } else {
            panic!("Expected ItsInterchainTokenDeploymentStarted event");
        }
    }
}

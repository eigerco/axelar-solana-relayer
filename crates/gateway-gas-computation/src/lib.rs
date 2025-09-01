//! Utility crate to compute the total gas used for multi-transaction operations on the Gateway

use axelar_solana_gateway::executable::AxelarMessagePayload;
use axelar_solana_gateway::instructions::GatewayInstruction;
use futures::stream::FuturesUnordered;
use futures::TryStreamExt as _;
use solana_listener::{fetch_logs, SolanaTransaction, TxStatus};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

/// Computes the total gas cost (in lamports) for a given Solana transaction, factoring in:
/// - Direct transaction cost.
/// - Additional costs from multi-step gateway instructions (e.g., verification sessions, message
///   payload uploads).
///
/// Specifically, this function:
/// 1. Checks each instruction in the transaction.
/// 2. If it's a gateway instruction (`ApproveMessage` or `RotateSigners`), it fetches and sums:
///    - The cost of initializing and verifying signatures associated with the verification session.
///    - Spreads these costs across messages in a batch if `ApproveMessage` is used.
/// 3. If it's an Axelar executable instruction that uploads a message payload, the function fetches
///    and sums:
///    - The costs of all transactions that initialized, wrote, committed, or closed the payload.
///
/// # Arguments
///
/// * `gateway_program_id` - Public key for the gateway program.
/// * `tx` - The Solana transaction to inspect.
/// * `rpc` - RPC client used to query transaction logs and signatures.
/// * `commitment` - Commitment level for RPC requests.
///
/// # Returns
///
/// Returns the accumulated cost in lamports, accounting for both the base transaction cost and any
/// necessary multi-transaction workflows (like verification sessions or payload uploads).
///
/// # Errors
///
/// May return an error if:
/// * RPC calls fail (e.g., fetching signatures or logs).
/// * Parsing of instruction data from transaction logs fails.
pub async fn compute_total_gas(
    gateway_program_id: Pubkey,
    tx: &SolanaTransaction,
    rpc: &RpcClient,
    commitment: CommitmentConfig,
) -> eyre::Result<u64> {
    let mut total_gas_cost = tx.cost_in_lamports;

    for (program_id, accounts, payload) in &tx.ixs {
        match program_id {
            id if *id == gateway_program_id => {
                let Ok(ix) = borsh::from_slice::<GatewayInstruction>(payload) else {
                    continue;
                };

                match ix {
                    GatewayInstruction::ApproveMessage { message, .. } => {
                        const VERIFICATION_SESSION_PDA_IDX: usize = 2;
                        let Some(verification_session_pda) =
                            accounts.get(VERIFICATION_SESSION_PDA_IDX).copied()
                        else {
                            continue;
                        };
                        let mut verify_signatures_costs = cost_of_signature_verification(
                            rpc,
                            commitment,
                            verification_session_pda,
                            gateway_program_id,
                        )
                        .await?;

                        // the cost per signature is spread out between the amount of messages that
                        // were approved
                        verify_signatures_costs = verify_signatures_costs
                            .checked_div(message.leaf.set_size.into())
                            .unwrap_or(0);
                        total_gas_cost = total_gas_cost.saturating_add(verify_signatures_costs);
                    }
                    GatewayInstruction::RotateSigners { .. } => {
                        const VERIFICATION_SESSION_PDA_IDX: usize = 1;
                        let Some(verification_session_pda) =
                            accounts.get(VERIFICATION_SESSION_PDA_IDX).copied()
                        else {
                            continue;
                        };

                        let verify_signatures_costs = cost_of_signature_verification(
                            rpc,
                            commitment,
                            verification_session_pda,
                            gateway_program_id,
                        )
                        .await?;

                        total_gas_cost = total_gas_cost.saturating_add(verify_signatures_costs);
                    }
                    GatewayInstruction::CallContract { .. } |
                    GatewayInstruction::InitializeConfig(_) |
                    GatewayInstruction::InitializePayloadVerificationSession { .. } |
                    GatewayInstruction::VerifySignature { .. } |
                    GatewayInstruction::InitializeMessagePayload { .. } |
                    GatewayInstruction::WriteMessagePayload { .. } |
                    GatewayInstruction::CommitMessagePayload { .. } |
                    GatewayInstruction::CloseMessagePayload { .. } |
                    GatewayInstruction::ValidateMessage { .. } |
                    GatewayInstruction::TransferOperatorship => {
                        continue;
                    }
                }
            }
            _other => {
                const MESSAGE_PAYLOAD_PDA_IDX: usize = 1;
                // check if this is `axelar_executable` call
                let Ok(_message) = AxelarMessagePayload::decode(payload) else {
                    continue;
                };

                let Some(message_payload_pda) = accounts.get(MESSAGE_PAYLOAD_PDA_IDX).copied()
                else {
                    continue;
                };
                let upload_payload_costs = cost_of_payload_uploading(
                    rpc,
                    commitment,
                    message_payload_pda,
                    gateway_program_id,
                )
                .await?;

                total_gas_cost = total_gas_cost.saturating_add(upload_payload_costs);
            }
        }
    }
    Ok(total_gas_cost)
}

async fn cost_of_signature_verification(
    rpc: &RpcClient,
    commitment: CommitmentConfig,
    verification_session_pda: Pubkey,
    gateway_program_id: Pubkey,
) -> Result<u64, eyre::Error> {
    let signatures = fetch_signatures(rpc, commitment, &verification_session_pda).await?;
    let tx_logs = signatures
        .into_iter()
        .map(|x| fetch_logs(commitment, x, rpc))
        .collect::<FuturesUnordered<_>>();
    let tx_logs = tx_logs.try_collect::<Vec<_>>().await?;
    let mut verify_signatures_costs = 0_u64;
    for tx in tx_logs {
        let TxStatus::Successful(tx) = tx else {
            continue;
        };
        for (program_id, _accounts, payload) in tx.ixs {
            let Ok(instruction_data) = borsh::from_slice::<GatewayInstruction>(&payload) else {
                continue;
            };

            if program_id != gateway_program_id {
                continue;
            }

            match instruction_data {
                GatewayInstruction::InitializePayloadVerificationSession { .. } |
                GatewayInstruction::VerifySignature { .. } => {
                    verify_signatures_costs =
                        verify_signatures_costs.saturating_add(tx.cost_in_lamports);
                }
                // no action to take
                GatewayInstruction::ApproveMessage { .. } |
                GatewayInstruction::RotateSigners { .. } |
                GatewayInstruction::CallContract { .. } |
                GatewayInstruction::InitializeConfig(_) |
                GatewayInstruction::InitializeMessagePayload { .. } |
                GatewayInstruction::WriteMessagePayload { .. } |
                GatewayInstruction::CommitMessagePayload { .. } |
                GatewayInstruction::CloseMessagePayload { .. } |
                GatewayInstruction::ValidateMessage { .. } |
                GatewayInstruction::TransferOperatorship => (),
            }
        }
    }
    Ok(verify_signatures_costs)
}

/// Computes the cost of uploading the message payload.
///
/// # Errors
///
/// - if no transactions are found.
/// - if no logs are found for a transaction.
pub async fn cost_of_payload_uploading(
    rpc: &RpcClient,
    commitment: CommitmentConfig,
    message_payload_pda: Pubkey,
    gateway_program_id: Pubkey,
) -> Result<u64, eyre::Error> {
    let signatures = fetch_signatures(rpc, commitment, &message_payload_pda).await?;
    let tx_logs = signatures
        .into_iter()
        .map(|x| fetch_logs(commitment, x, rpc))
        .collect::<FuturesUnordered<_>>();
    let tx_logs = tx_logs.try_collect::<Vec<_>>().await?;
    let mut total_gas_costs = 0_u64;
    for tx in tx_logs {
        let TxStatus::Successful(tx) = tx else {
            continue;
        };
        for (program_id, _accounts, payload) in tx.ixs {
            let Ok(instruction_data) = borsh::from_slice::<GatewayInstruction>(&payload) else {
                continue;
            };

            if program_id != gateway_program_id {
                continue;
            }

            match instruction_data {
                GatewayInstruction::InitializeMessagePayload { .. } |
                GatewayInstruction::WriteMessagePayload { .. } |
                GatewayInstruction::CommitMessagePayload { .. } |
                GatewayInstruction::CloseMessagePayload { .. } => {
                    total_gas_costs = total_gas_costs.saturating_add(tx.cost_in_lamports);
                }
                // no actoin to take
                GatewayInstruction::ApproveMessage { .. } |
                GatewayInstruction::RotateSigners { .. } |
                GatewayInstruction::CallContract { .. } |
                GatewayInstruction::InitializeConfig(_) |
                GatewayInstruction::InitializePayloadVerificationSession { .. } |
                GatewayInstruction::VerifySignature { .. } |
                GatewayInstruction::ValidateMessage { .. } |
                GatewayInstruction::TransferOperatorship => {}
            }
        }
    }
    Ok(total_gas_costs)
}

async fn fetch_signatures(
    client: &RpcClient,
    commitment: CommitmentConfig,
    address: &Pubkey,
) -> eyre::Result<Vec<Signature>> {
    let mut all_signatures = Vec::new();
    let mut before_sig = None;

    loop {
        let config = GetConfirmedSignaturesForAddress2Config {
            before: before_sig,
            limit: Some(1000),
            commitment: Some(commitment),
            ..Default::default()
        };

        let page = client
            .get_signatures_for_address_with_config(address, config)
            .await?;
        if page.is_empty() {
            break;
        }
        before_sig = page
            .last()
            .map(|info| info.signature.clone().parse())
            .transpose()?;
        all_signatures.extend(
            page.into_iter()
                .filter_map(|info| info.signature.parse::<Signature>().ok()),
        );
    }

    Ok(all_signatures)
}

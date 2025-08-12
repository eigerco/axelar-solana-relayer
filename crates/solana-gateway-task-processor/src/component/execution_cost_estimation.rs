//! Gas cost estimation for executing tasks on Solana
//!
//! This module estimates gas costs by broadcasting transactions on a forked
//! Solana cluster state (e.g., using surfpool as an external RPC service).

use std::collections::HashSet;
use std::sync::Arc;

use axelar_solana_encoding::types::messages::Message;
use axelar_solana_gateway::state::incoming_message::command_id;
use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_request::RpcRequest;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

use super::message_payload::{self, MAX_CHUNK_SIZE};

#[derive(Debug, Clone)]
pub(crate) struct StageMetadata {
    compute_units_consumed: u64,
    accounts: Vec<Pubkey>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecuteTaskConsumptionBreakdown {
    initialization: StageMetadata,
    upload: Vec<StageMetadata>,
    commitment: StageMetadata,
    execution: StageMetadata,
    closure: StageMetadata,
}

impl ExecuteTaskConsumptionBreakdown {
    const fn new() -> Self {
        Self {
            initialization: StageMetadata {
                compute_units_consumed: 0,
                accounts: vec![],
            },
            upload: vec![],
            commitment: StageMetadata {
                compute_units_consumed: 0,
                accounts: vec![],
            },
            execution: StageMetadata {
                compute_units_consumed: 0,
                accounts: vec![],
            },
            closure: StageMetadata {
                compute_units_consumed: 0,
                accounts: vec![],
            },
        }
    }

    pub(crate) async fn calculate_cost(&self, rpc_client: &RpcClient) -> eyre::Result<u64> {
        let calculate_stage_fee = |metadata: &StageMetadata, compute_unit_price: u64| -> u64 {
            let base_fee = 5000_u64; // Base transaction fee in lamports
            let priority_fee = metadata
                .compute_units_consumed
                .saturating_mul(compute_unit_price);
            base_fee.saturating_add(priority_fee)
        };

        let mut total_fee = 0_u64;

        if !self.initialization.accounts.is_empty() {
            let compute_unit_price =
                calculate_compute_unit_price(&self.initialization.accounts, rpc_client).await?;
            let stage_fee = calculate_stage_fee(&self.initialization, compute_unit_price);
            total_fee = total_fee.saturating_add(stage_fee);
        }

        for metadata in &self.upload {
            if !metadata.accounts.is_empty() {
                let compute_unit_price =
                    calculate_compute_unit_price(&metadata.accounts, rpc_client).await?;
                let stage_fee = calculate_stage_fee(metadata, compute_unit_price);
                total_fee = total_fee.saturating_add(stage_fee);
            }
        }

        if !self.commitment.accounts.is_empty() {
            let compute_unit_price =
                calculate_compute_unit_price(&self.commitment.accounts, rpc_client).await?;
            let stage_fee = calculate_stage_fee(&self.commitment, compute_unit_price);
            total_fee = total_fee.saturating_add(stage_fee);
        }

        if !self.execution.accounts.is_empty() {
            let compute_unit_price =
                calculate_compute_unit_price(&self.execution.accounts, rpc_client).await?;
            let stage_fee = calculate_stage_fee(&self.execution, compute_unit_price);
            total_fee = total_fee.saturating_add(stage_fee);
        }

        if !self.closure.accounts.is_empty() {
            let compute_unit_price =
                calculate_compute_unit_price(&self.closure.accounts, rpc_client).await?;
            let stage_fee = calculate_stage_fee(&self.closure, compute_unit_price);
            total_fee = total_fee.saturating_add(stage_fee);
        }

        Ok(total_fee)
    }
}

struct AccountsReset {
    accounts: HashSet<Pubkey>,
    rpc_client: Arc<RpcClient>,
}

impl Drop for AccountsReset {
    fn drop(&mut self) {
        let accounts: Vec<Pubkey> = self.accounts.drain().collect();
        let rpc_client = Arc::clone(&self.rpc_client);

        // Sets accounts lamports to 0 so they're garbage collected within surfpool
        tokio::spawn(async move {
            for account in accounts {
                if let Ok(request_body) = serde_json::from_value(json!(
                [
                    account.to_string(),
                    {
                        "lamports": 0_u64
                    }
                ])) {
                    let _result = rpc_client
                        .send::<()>(
                            RpcRequest::Custom {
                                method: "surfnet_setAccount",
                            },
                            request_body,
                        )
                        .await;
                }
            }
        });
    }
}

/// Estimates the total gas cost for executing a task
///
/// This includes:
/// 1. Uploading the message payload (init, write, commit)
/// 2. Executing the message (sending to destination)
/// 3. Closing the payload account
///
/// The `simnet_rpc_client` should be connected to a surfpool/solana-test-validator instance that
/// has forked the current chain state to properly process transactions with dependencies.
pub(crate) async fn estimate_total_execute_cost(
    simnet_rpc_client: Arc<RpcClient>,
    rpc_client: &RpcClient,
    keypair: &Keypair,
    gateway_root_pda: Pubkey,
    message: &Message,
    payload: &[u8],
    destination_address: Pubkey,
) -> eyre::Result<(u64, ExecuteTaskConsumptionBreakdown)> {
    let mut total_fee: u64 = 0;
    let mut cost_breakdown = ExecuteTaskConsumptionBreakdown::new();
    let mut account_cleanup = AccountsReset {
        accounts: HashSet::new(),
        rpc_client: Arc::clone(&simnet_rpc_client),
    };

    let msg_command_id = message_payload::message_to_command_id(message);

    // 1. Initialize payload account
    let init_ix = axelar_solana_gateway::instructions::initialize_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        msg_command_id,
        payload.len().try_into()?,
    )?;

    let (init_cost, init_metadata) =
        execute_and_get_cost(&simnet_rpc_client, rpc_client, keypair, vec![init_ix]).await?;
    account_cleanup
        .accounts
        .extend(init_metadata.accounts.iter());
    cost_breakdown.initialization = init_metadata;
    total_fee = total_fee.saturating_add(init_cost);

    // 2. Write payload in chunks
    for (index, chunk) in payload.chunks(*MAX_CHUNK_SIZE).enumerate() {
        let offset = index.saturating_mul(*MAX_CHUNK_SIZE);
        let write_ix = axelar_solana_gateway::instructions::write_message_payload(
            gateway_root_pda,
            keypair.pubkey(),
            msg_command_id,
            chunk,
            offset.try_into()?,
        )?;

        let (write_cost, write_metadata) =
            execute_and_get_cost(&simnet_rpc_client, rpc_client, keypair, vec![write_ix]).await?;
        account_cleanup
            .accounts
            .extend(write_metadata.accounts.iter());
        cost_breakdown.upload.push(write_metadata);
        total_fee = total_fee.saturating_add(write_cost);
    }

    // 3. Commit payload
    let commit_ix = axelar_solana_gateway::instructions::commit_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        msg_command_id,
    )?;

    let (commit_cost, commit_metadata) =
        execute_and_get_cost(&simnet_rpc_client, rpc_client, keypair, vec![commit_ix]).await?;
    account_cleanup
        .accounts
        .extend(commit_metadata.accounts.iter());
    cost_breakdown.commitment = commit_metadata;
    total_fee = total_fee.saturating_add(commit_cost);

    // 4. Execute message
    let execute_ix = build_execute_instruction(
        keypair.pubkey(),
        message,
        payload,
        destination_address,
        &simnet_rpc_client,
    )
    .await?;

    let (execute_cost, execute_metadata) =
        execute_and_get_cost(&simnet_rpc_client, rpc_client, keypair, vec![execute_ix]).await?;
    account_cleanup
        .accounts
        .extend(execute_metadata.accounts.iter());
    cost_breakdown.execution = execute_metadata;
    total_fee = total_fee.saturating_add(execute_cost);

    // 5. Close payload account
    let close_ix = axelar_solana_gateway::instructions::close_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        msg_command_id,
    )?;

    let (close_cost, close_metadata) =
        execute_and_get_cost(&simnet_rpc_client, rpc_client, keypair, vec![close_ix]).await?;
    account_cleanup
        .accounts
        .extend(close_metadata.accounts.iter());
    cost_breakdown.closure = close_metadata;
    total_fee = total_fee.saturating_add(close_cost);

    Ok((total_fee, cost_breakdown))
}

/// Executes a transaction on surfpool and returns the actual gas cost with metadata
async fn execute_and_get_cost(
    simnet_rpc_client: &RpcClient,
    rpc_client: &RpcClient,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
) -> eyre::Result<(u64, StageMetadata)> {
    let blockhash = simnet_rpc_client
        .get_latest_blockhash()
        .await
        .map_err(|err| eyre::eyre!("Failed to get blockhash: {}", err))?;

    let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    instructions.insert(0, compute_budget_ix);

    // NOTE: For the priority fee, we fetch it from the actual chain since it depends on network
    // congestion, etc.
    let priority_fee_ix = build_compute_unit_price_instruction(
        &instructions
            .iter()
            .flat_map(|ix| ix.accounts.iter())
            .map(|acc| acc.pubkey)
            .collect::<Vec<_>>(),
        rpc_client,
    )
    .await?;
    instructions.insert(0, priority_fee_ix);

    // Extract accounts and compute budget for metadata
    let accounts: Vec<Pubkey> = instructions
        .iter()
        .flat_map(|ix| ix.accounts.iter().map(|acc| acc.pubkey))
        .collect();

    let tx = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &[keypair],
        blockhash,
    );

    let signature = simnet_rpc_client
        .send_and_confirm_transaction(&tx)
        .await
        .map_err(|err| eyre::eyre!("Failed to send transaction: {}", err))?;

    let tx_status = simnet_rpc_client
        .get_transaction(
            &signature,
            solana_transaction_status::UiTransactionEncoding::Base64,
        )
        .await
        .map_err(|err| eyre::eyre!("Failed to get transaction status: {}", err))?;

    let fee = tx_status
        .transaction
        .meta
        .as_ref()
        .map(|meta| meta.fee)
        .ok_or_else(|| eyre::eyre!("Transaction metadata not available"))?;

    let compute_units_consumed = tx_status
        .transaction
        .meta
        .ok_or_else(|| eyre::eyre!("Transaction metadata not available"))?
        .compute_units_consumed
        .ok_or_else(|| eyre::eyre!("Missing compute units consumption details"))?;

    let metadata = StageMetadata {
        compute_units_consumed,
        accounts,
    };

    Ok((fee, metadata))
}

/// Build execute instruction
async fn build_execute_instruction(
    signer: Pubkey,
    message: &Message,
    payload: &[u8],
    destination_address: Pubkey,
    rpc_client: &RpcClient,
) -> eyre::Result<Instruction> {
    let (gateway_incoming_message_pda, _) = axelar_solana_gateway::get_incoming_message_pda(
        &command_id(&message.cc_id.chain, &message.cc_id.id),
    );
    let (gateway_message_payload_pda, _) =
        axelar_solana_gateway::find_message_payload_pda(gateway_incoming_message_pda);

    match destination_address {
        axelar_solana_its::ID => Ok(its_instruction_builder::build_its_gmp_instruction(
            signer,
            gateway_incoming_message_pda,
            gateway_message_payload_pda,
            message.clone(),
            payload.to_vec(),
            rpc_client,
        )
        .await
        .map_err(|err| eyre::eyre!("Failed to build ITS instruction: {:?}", err))?),
        axelar_solana_governance::ID => Ok(
            axelar_solana_governance::instructions::builder::calculate_gmp_ix(
                signer,
                gateway_incoming_message_pda,
                gateway_message_payload_pda,
                message,
                payload,
            )?,
        ),
        _ => Ok(axelar_executable::construct_axelar_executable_ix(
            message,
            payload,
            gateway_incoming_message_pda,
            gateway_message_payload_pda,
        )?),
    }
}

/// Calculate compute unit price based on recent prioritization fees
async fn build_compute_unit_price_instruction(
    accounts: &[Pubkey],
    rpc_client: &RpcClient,
) -> eyre::Result<Instruction> {
    let average_fee = calculate_compute_unit_price(accounts, rpc_client).await?;
    Ok(ComputeBudgetInstruction::set_compute_unit_price(
        average_fee,
    ))
}

async fn calculate_compute_unit_price(
    accounts: &[Pubkey],
    rpc_client: &RpcClient,
) -> eyre::Result<u64> {
    const MAX_ACCOUNTS: usize = 128;
    const N_SLOTS_TO_CHECK: usize = 10;

    if accounts.len() > MAX_ACCOUNTS {
        eyre::bail!("Too many accounts, cannot calculate compute unit price");
    }

    // Get recent prioritization fees
    let fees = rpc_client
        .get_recent_prioritization_fees(accounts)
        .await
        .map_err(|err| eyre::eyre!("Failed to get prioritization fees: {}", err))?;

    // Calculate average fee from recent slots
    let (sum, count) = fees
        .into_iter()
        .rev()
        .take(N_SLOTS_TO_CHECK)
        .map(|fee_info| fee_info.prioritization_fee)
        .fold((0_u64, 0_u64), |(sum, count), fee| {
            (sum.saturating_add(fee), count.saturating_add(1))
        });

    let average_fee = if count > 0 {
        sum.checked_div(count).unwrap_or(0)
    } else {
        0
    };

    Ok(average_fee)
}

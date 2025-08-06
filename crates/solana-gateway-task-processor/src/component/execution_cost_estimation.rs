//! Gas cost estimation for executing tasks on Solana
//!
//! This module estimates gas costs by broadcasting transactions on a forked
//! Solana cluster state (e.g., using surfpool as an external RPC service).

use axelar_solana_encoding::types::messages::Message;
use axelar_solana_gateway::state::incoming_message::command_id;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

use super::message_payload::{self, MAX_CHUNK_SIZE};

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
    simnet_rpc_client: &RpcClient,
    rpc_client: &RpcClient,
    keypair: &Keypair,
    gateway_root_pda: Pubkey,
    message: &Message,
    payload: &[u8],
    destination_address: Pubkey,
) -> eyre::Result<u64> {
    let mut total_cost = 0_u64;
    let msg_command_id = message_payload::message_to_command_id(message);

    // Fetch the incoming message account to ensure the fork updates its ledger
    let (incoming_message_pda, _) =
        axelar_solana_gateway::get_incoming_message_pda(&msg_command_id);

    simnet_rpc_client
        .get_account_with_commitment(&incoming_message_pda, CommitmentConfig::confirmed())
        .await?;

    simnet_rpc_client
        .get_account_with_commitment(&destination_address, CommitmentConfig::confirmed())
        .await?;
    simnet_rpc_client
        .get_program_accounts(&destination_address)
        .await?;

    // 1. Initialize payload account
    let init_ix = axelar_solana_gateway::instructions::initialize_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        msg_command_id,
        payload.len().try_into()?,
    )?;

    let init_cost =
        execute_and_get_cost(simnet_rpc_client, rpc_client, keypair, vec![init_ix]).await?;
    total_cost = total_cost.saturating_add(init_cost);

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

        let write_cost =
            execute_and_get_cost(simnet_rpc_client, rpc_client, keypair, vec![write_ix]).await?;
        total_cost = total_cost.saturating_add(write_cost);
    }

    // 3. Commit payload
    let commit_ix = axelar_solana_gateway::instructions::commit_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        msg_command_id,
    )?;

    let commit_cost =
        execute_and_get_cost(simnet_rpc_client, rpc_client, keypair, vec![commit_ix]).await?;
    total_cost = total_cost.saturating_add(commit_cost);

    // 4. Execute message
    let execute_ix = build_execute_instruction(
        keypair.pubkey(),
        message,
        payload,
        destination_address,
        simnet_rpc_client,
    )
    .await?;

    let execute_cost =
        execute_and_get_cost(simnet_rpc_client, rpc_client, keypair, vec![execute_ix]).await?;
    total_cost = total_cost.saturating_add(execute_cost);

    // 5. Close payload account
    let close_ix = axelar_solana_gateway::instructions::close_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        msg_command_id,
    )?;

    let close_cost =
        execute_and_get_cost(simnet_rpc_client, rpc_client, keypair, vec![close_ix]).await?;
    total_cost = total_cost.saturating_add(close_cost);

    Ok(total_cost)
}

/// Executes a transaction on surfpool and returns the actual gas cost
async fn execute_and_get_cost(
    simnet_rpc_client: &RpcClient,
    rpc_client: &RpcClient,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
) -> eyre::Result<u64> {
    let blockhash = simnet_rpc_client
        .get_latest_blockhash()
        .await
        .map_err(|err| eyre::eyre!("Failed to get blockhash: {}", err))?;

    let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    instructions.insert(0, compute_budget_ix);

    // NOTE: For the priority fee, we fetch it from the actual chain since it depends on network
    // congestion, etc.
    let priority_fee_ix = calculate_compute_unit_price(&instructions, rpc_client).await?;
    instructions.insert(0, priority_fee_ix);

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
        .map(|meta| meta.fee)
        .ok_or_else(|| eyre::eyre!("Transaction metadata not available"))?;

    Ok(fee)
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
async fn calculate_compute_unit_price(
    instructions: &[Instruction],
    rpc_client: &RpcClient,
) -> eyre::Result<Instruction> {
    const MAX_ACCOUNTS: usize = 128;
    const N_SLOTS_TO_CHECK: usize = 10;

    // Collect all accounts touched by the instructions
    let all_touched_accounts: Vec<Pubkey> = instructions
        .iter()
        .flat_map(|ix| ix.accounts.iter())
        .take(MAX_ACCOUNTS)
        .map(|acc| acc.pubkey)
        .collect();

    // Get recent prioritization fees
    let fees = rpc_client
        .get_recent_prioritization_fees(&all_touched_accounts)
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

    Ok(ComputeBudgetInstruction::set_compute_unit_price(
        average_fee,
    ))
}

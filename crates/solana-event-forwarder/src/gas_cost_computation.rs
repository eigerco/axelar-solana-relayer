use std::future;

use futures::stream::FuturesUnordered;
use futures::{StreamExt, TryStreamExt};
use gateway_event_stack::{build_program_event_stack, MatchContext, ProgramInvocationState};
use solana_listener::{fetch_logs, TxStatus};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

// filter only successful txs
//  message approved / verifier set rotated
//  filter gateway txs where we:
// "Instruction: Initialize Verification Session"
//    -- we need to divide the cost across all message count in the batch
// "Instruction: Verify Signature"
//    -- we need to divide the cost across all message count in the batch
// "Instruction: Approve Messages" / "Instruction: Rotate Signers"
//
//
// message executed:
// "Instruction: Initialize Message Payload"
// "Instruction: Write Message Payload"
// "Instruction: Commit Message Payload"
// "Instruction: Close Message Payload"
/// For “approve message” and “rotate signers” events we cannot just report the gas costs for that 1 tx where this even was emitted. It’s because to get to that state, we (the relayer) needs to send:
/// 1 init verification session transactoin
/// N “verify signature” tx
/// 1 approve message tx
/// This means that the amount of gas that was consumed is total sum of all of these events.
/// But we capture an event from the last one (approve message). So what I need to do:
/// parse the ix accounts that were passed for the “approve message” ix (because this is the event I get), extract the “verification session” PDA from the accounts array (can be done with ease but code is just indexing into some global arrays all over the place)
/// then query all signatures where the “verification session” PDA appeared, aggregate the costs
/// divide the costs by the amount of messages in a batch.
/// How do I know how many messages are in a batch? I have to parse raw ix arguments and try to extract set_size from MessageLeaf
/// these steps need to be done for each message that gets approved, and for “signers rotated”. The whole goal is to scout all txs that the relayer sent for a specific action that requires many txs, and count it all together.
/// But what about “message executed” event? We are initialising large payloads and the relayer sends many small txs again.
/// The same approach as before, we just parse for different accounts.
pub async fn compute_total_gas(
    gateway_message_pda: Pubkey,
    gateway_match_context: &MatchContext,
    rpc: RpcClient,
    commitment: CommitmentConfig,
) -> eyre::Result<u64> {
    let signatures = fetch_signatures(&rpc, commitment, &gateway_message_pda).await?;
    let mut tx_logs = signatures
        .into_iter()
        .map(|x| fetch_logs(commitment, x, &rpc))
        .collect::<FuturesUnordered<_>>();
    let tx_logs = tx_logs.try_collect::<Vec<_>>().await?;

    for tx in tx_logs {
        let TxStatus::Successful(tx) = tx else {
            continue;
        };
        let approve_msg_invocation = build_program_event_stack(gateway_match_context, &tx.logs, |log: &String| {
            if log.starts_with("Program log: Instruction: Approve Messages") {
                return eyre::Ok(log)
            };
            eyre::bail!("not matching log")
        });

        // first ix is the compute budget ix
        // second ix the Gateway invocation
        const GATEWAY_IX_IDX: usize = 1;
        const VERIFICATION_SESSION_PDA: usize = 1;
        let (program_id, ix_accounts) = tx.instruction_accounts.get(GATEWAY_IX_IDX).unwrap();
        for invocation in approve_msg_invocation {
            let ProgramInvocationState::Succeeded(invocation) =  invocation else {
                
            }
        }

        // find where we initialize the verification session
        // find all signature verifications
        //
    }

    Ok(0_u64)
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

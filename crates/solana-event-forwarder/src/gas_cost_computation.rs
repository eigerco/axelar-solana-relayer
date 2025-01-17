use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

pub async fn compute_total_gas(
    address: Pubkey,
    rpc: RpcClient,
    commitment: CommitmentConfig,
) -> eyre::Result<u64> {
    let signatures = fetch_signatures(&rpc, commitment, &address).await?;
    // for every signature fetch logs
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

    Ok(0_u64)
}

async fn fetch_signatures(
    client: &RpcClient,
    commitment: CommitmentConfig,
    address: &Pubkey,
) -> eyre::Result<Vec<String>> {
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
        all_signatures.extend(page.into_iter().map(|info| info.signature));
    }

    Ok(all_signatures)
}

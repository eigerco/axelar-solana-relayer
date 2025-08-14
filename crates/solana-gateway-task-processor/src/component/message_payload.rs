//! Message payload management module for Axelar Gateway integration.
//!
//! This module provides functionality to handle message payloads in the Solana blockchain,
//! including initialization, writing, committing, and closing of message payload accounts.

use std::sync::LazyLock;

use axelar_solana_encoding::types::messages::Message;
use axelar_solana_gateway::state::incoming_message::command_id;
use eyre::Context as _;
use futures::stream::FuturesUnordered;
use futures::StreamExt as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::message::legacy::Message as SolanaMessage;
use solana_sdk::packet::PACKET_DATA_SIZE;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

use super::send_gateway_tx;

/// Maximum number of bytes we can pack into each `GatewayInstruction::WriteMessagePayload`
/// instruction.
///
/// Calculates the maximum payload size that can fit in a Solana transaction for the
/// `WriteMessagePayload` instruction. This is done by creating a baseline transaction with empty
/// payload, measuring its size, and subtracting it from the maximum Solana packet size.
///
/// The calculation is performed once on first access and cached, using random data since we only
/// care about the structure size, not the actual values.
///
/// # Panics
///
/// Will panic during initialization if:
/// - Fails to create the `WriteMessagePayload` instruction.
/// - Fails to serialize the transaction with `bincode`.
/// - Fails to convert the size from a u64 value to a usize.
///
/// Based on: `https://github.com/solana-labs/solana/pull/19654`
pub(crate) static MAX_CHUNK_SIZE: LazyLock<usize> = LazyLock::new(|| {
    // Generate a random pubkey for all fields since we only care about size
    let random_pubkey = Pubkey::new_unique();

    // Create baseline instruction with empty payload data
    let write_instruction = axelar_solana_gateway::instructions::write_message_payload(
        random_pubkey,
        random_pubkey,
        random_pubkey.to_bytes(),
        &[], // empty data
        0,
    )
    .expect("Failed to create baseline WriteMessagePayload instruction");

    // Include compute budget instructions that EffectiveTxSender adds in production
    let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    let priority_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(1000);

    let baseline_msg = SolanaMessage::new_with_blockhash(
        &[compute_budget_ix, priority_fee_ix, write_instruction],
        Some(&random_pubkey),
        &Hash::default(),
    );

    let tx = Transaction {
        signatures: vec![Signature::default(); baseline_msg.header.num_required_signatures.into()],
        message: baseline_msg,
    };

    let tx_size = bincode::serialized_size(&tx)
        .expect("Failed to serialize transaction")
        .try_into()
        .expect("Failed to convert u64 value to usize");

    // Based on empirical testing, there seems to be additional overhead that we could not find the
    // source
    let additional_overhead = 35;

    // Subtract baseline size, 1 byte for shortvec encoding, and additional overhead
    PACKET_DATA_SIZE
        .saturating_sub(tx_size)
        .saturating_sub(1)
        .saturating_sub(additional_overhead)
});

/// Handles the upload of a message payload to a Program Derived Address (PDA) account.
///
/// This function involves three main steps:
/// 1. Initialize the payload account
/// 2. Write the payload data
/// 3. Commit the payload
///
/// Make sure to close the account afterward to recover the allocated funds.
pub(crate) async fn upload(
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    gateway_root_pda: Pubkey,
    message: &Message,
    payload: &[u8],
) -> eyre::Result<Pubkey> {
    let msg_command_id = message_to_command_id(message);

    initialize(
        solana_rpc_client,
        keypair,
        gateway_root_pda,
        msg_command_id,
        payload,
    )
    .await?;
    write(
        solana_rpc_client,
        keypair,
        gateway_root_pda,
        msg_command_id,
        payload,
    )
    .await?;
    commit(solana_rpc_client, keypair, gateway_root_pda, msg_command_id).await?;

    let (incoming_message_pda, _bump) =
        axelar_solana_gateway::get_incoming_message_pda(&message_to_command_id(message));
    let (message_payload_pda, _bump) =
        axelar_solana_gateway::find_message_payload_pda(incoming_message_pda, keypair.pubkey());

    Ok(message_payload_pda)
}

/// Initializes a new message payload account.
async fn initialize(
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    gateway_root_pda: Pubkey,
    command_id: [u8; 32],
    payload: &[u8],
) -> eyre::Result<()> {
    let ix = axelar_solana_gateway::instructions::initialize_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        command_id,
        payload
            .len()
            .try_into()
            .context("Unexpected u64 overflow in buffer size")?,
    )
    .context("failed to construct an instruction to initialize the message payload pda")?;
    send_gateway_tx(solana_rpc_client, keypair, ix)
        .await
        .context("faled to initialize the message payload pda")?;
    Ok(())
}

/// Writes payload data to an initialized account in chunks concurrently.
///
/// This function takes the raw payload bytes and writes them to a `MessagePayload`
/// PDA account by:
/// 1. Splitting the payload into fixed-size chunks.
/// 2. Creating concurrent write transactions for each chunk.
/// 3. Executing all writes in concurrently using [`FuturesUnordered`]
///
///
/// # Errors
///
/// Returns an error if:
/// * Instruction construction fails
/// * Any chunk write transaction fails
///
/// # Note
///
/// Chunks can be written out of order since they target different parts of the
/// `MessagePayload` account's data.
async fn write(
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    gateway_root_pda: Pubkey,
    command_id: [u8; 32],
    payload: &[u8],
) -> eyre::Result<()> {
    let mut futures = FuturesUnordered::new();
    for ChunkWithOffset { bytes, offset } in chunks_with_offset(payload, *MAX_CHUNK_SIZE) {
        let ix = axelar_solana_gateway::instructions::write_message_payload(
            gateway_root_pda,
            keypair.pubkey(),
            command_id,
            bytes,
            offset
                .try_into()
                .context("Unexpected u64 overflow in offset")?,
        )
        .context("failed to construct an instruction to write to the message payload pda")?;
        futures.push(async move {
            let tx = send_gateway_tx(solana_rpc_client, keypair, ix).await;
            (offset, tx)
        });
    }

    while let Some((offset, tx)) = futures.next().await {
        tx.with_context(|| format!("failed to  write message payload at offset {offset}"))?;
    }

    Ok(())
}

/// Commits the message payload, finalizing the upload process.
async fn commit(
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    gateway_root_pda: Pubkey,
    command_id: [u8; 32],
) -> eyre::Result<()> {
    let ix = axelar_solana_gateway::instructions::commit_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        command_id,
    )
    .context("failed to construct an instruction to commit the message payload pda")?;
    send_gateway_tx(solana_rpc_client, keypair, ix)
        .await
        .context("failed to commit the message payload pda")?;
    Ok(())
}

/// Helper function to generate a command ID from a message.
pub(crate) fn message_to_command_id(message: &Message) -> [u8; 32] {
    command_id(&message.cc_id.chain, &message.cc_id.id)
}

/// Represents a chunk of data with its offset in the original data slice.
#[cfg_attr(test, derive(Debug, Clone, Eq, PartialEq))]
struct ChunkWithOffset<'a> {
    /// The actual chunk of data
    bytes: &'a [u8],
    /// Offset position in the original data
    offset: usize,
}

/// Creates an iterator that yields fixed-size chunks with their offsets.
fn chunks_with_offset(
    data: &[u8],
    chunk_size: usize,
) -> impl Iterator<Item = ChunkWithOffset<'_>> + '_ {
    data.chunks(chunk_size)
        .enumerate()
        .map(move |(index, chunk)| ChunkWithOffset {
            bytes: chunk,
            offset: index.saturating_mul(chunk_size),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunks_with_offset() {
        let data = b"12345678";
        let chunks: Vec<_> = chunks_with_offset(data, 3).collect();

        assert_eq!(
            chunks,
            vec![
                ChunkWithOffset {
                    bytes: b"123",
                    offset: 0
                },
                ChunkWithOffset {
                    bytes: b"456",
                    offset: 3
                },
                ChunkWithOffset {
                    bytes: b"78",
                    offset: 6
                },
            ]
        );
    }

    #[test]
    fn test_empty_input() {
        let data = b"";
        assert!(chunks_with_offset(data, 3).next().is_none());
    }

    #[test]
    fn test_chunk_size_larger_than_input() {
        let data = b"123";
        let chunks: Vec<_> = chunks_with_offset(data, 5).collect();
        assert_eq!(
            chunks,
            vec![ChunkWithOffset {
                bytes: b"123",
                offset: 0
            },]
        );
    }

    #[test]
    fn test_calculate_max_chunk_size() {
        let chunk_size = *MAX_CHUNK_SIZE;
        assert!(chunk_size > 0);
        assert!(chunk_size < PACKET_DATA_SIZE);
    }
}

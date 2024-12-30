//! Message payload management module for Axelar Gateway integration.
//!
//! This module provides functionality to handle message payloads in the Solana blockchain,
//! including initialization, writing, committing, and closing of message payload accounts.

use axelar_solana_encoding::types::messages::Message;
use axelar_solana_gateway::state::incoming_message::command_id;
use eyre::Context as _;
use futures::stream::FuturesUnordered;
use futures::StreamExt as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use solana_sdk::message::legacy::Message as SolanaMessage;
use solana_sdk::packet::PACKET_DATA_SIZE;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;

use super::send_transaction;

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

    let (message_payload_pda, _bump) = axelar_solana_gateway::find_message_payload_pda(
        gateway_root_pda,
        message_to_command_id(message),
        keypair.pubkey(),
    );

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
    send_transaction(solana_rpc_client, keypair, ix)
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
    let chunk_size = calculate_max_chunk_size(WriteInstructionParams {
        gateway_root_pda,
        authority: keypair.pubkey(),
        command_id,
    })?;

    let mut futures = FuturesUnordered::new();
    for ChunkWithOffset { bytes, offset } in chunks_with_offset(payload, chunk_size) {
        let ix = axelar_solana_gateway::instructions::write_message_payload(
            gateway_root_pda,
            keypair.pubkey(),
            command_id,
            bytes,
            offset,
        )
        .context("failed to construct an instruction to write to the message payload pda")?;
        futures.push(async move {
            let tx = send_transaction(solana_rpc_client, keypair, ix).await;
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
    send_transaction(solana_rpc_client, keypair, ix)
        .await
        .context("failed to commit the message payload pda")?;
    Ok(())
}

/// Closes the message payload account and reclaims its rent.
pub(crate) async fn close(
    solana_rpc_client: &RpcClient,
    keypair: &Keypair,
    gateway_root_pda: Pubkey,
    message: &Message,
) -> eyre::Result<()> {
    let msg_command_id = message_to_command_id(message);
    let ix = axelar_solana_gateway::instructions::close_message_payload(
        gateway_root_pda,
        keypair.pubkey(),
        msg_command_id,
    )
    .context("failed to construct an instruction to close the message payload pda")?;
    send_transaction(solana_rpc_client, keypair, ix)
        .await
        .context("failed to close the message payload pda")?;
    Ok(())
}

/// Helper function to generate a command ID from a message.
fn message_to_command_id(message: &Message) -> [u8; 32] {
    command_id(&message.cc_id.chain, &message.cc_id.id)
}

#[derive(Clone, Copy)]
struct WriteInstructionParams {
    gateway_root_pda: Pubkey,
    authority: Pubkey,
    command_id: [u8; 32],
}

/// Based on <https://github.com/solana-labs/solana/pull/19654>
/// TODO: Should we turn this into a static calculation?
fn calculate_max_chunk_size(params: WriteInstructionParams) -> eyre::Result<usize> {
    // Create baseline message directly with empty data
    let ix = axelar_solana_gateway::instructions::write_message_payload(
        params.gateway_root_pda,
        params.authority,
        params.command_id,
        &[], // empty data
        0,
    )?;

    let baseline_msg =
        SolanaMessage::new_with_blockhash(&[ix], Some(&params.authority), &Hash::default());

    let tx_size = bincode::serialized_size(&Transaction {
        signatures: vec![Signature::default(); baseline_msg.header.num_required_signatures.into()],
        message: baseline_msg,
    })
    .context("failed to calculate max chunk size")?
    .try_into()?;

    // add 1 byte buffer to account for shortvec encoding
    Ok(PACKET_DATA_SIZE.saturating_sub(tx_size).saturating_sub(1))
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
        let chunk_size = calculate_max_chunk_size(WriteInstructionParams {
            gateway_root_pda: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            command_id: rand::random(),
        })
        .unwrap();

        assert!(chunk_size > 0);
        assert!(chunk_size < PACKET_DATA_SIZE);
    }
}

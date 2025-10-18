use core::fmt;
use std::collections::HashMap;
use std::sync::Arc;

use eyre::OptionExt as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

/// Maximum compute units to request for a transaction
pub const MAX_COMPUTE_UNITS: u32 = 1_400;

const SOLANA_LAMPORTS_PER_SIGNATURE_COST: u64 = 5_000;
const MICRO_LAMPORTS_PER_LAMPORT: u64 = 1_000_000;

#[derive(Error, Debug)]
pub(crate) struct InsufficientGasBalance {
    cost: u64,
    available: u64,
}

impl fmt::Display for InsufficientGasBalance {
    #[expect(
        clippy::min_ident_chars,
        reason = "either this or clippy::renamed_function_params"
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Insufficient Gas Balance. Required: {}. Available: {}",
            self.cost, self.available
        )
    }
}

/// Trait for gas estimation to allow mocking in tests
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait GasEstimator: Send + Sync {
    /// Estimates the total cost of executing a gateway transaction
    async fn ensure_enough_gas(
        &self,
        ixs: Vec<Instruction>,
        available_gas: u64,
    ) -> eyre::Result<GasEstimatorResult>;
}

pub struct GasEstimatorResult {
    pub required_gas: u64,
    pub available_gas: u64,
    pub average_piority_fee_micro_lamports: u64,
}

/// Actual implementation of `GasEstimator`
pub struct PriorityFeeGasEstimator {
    rpc_client: Arc<RpcClient>,
    max_compute_units: u32,
}

impl PriorityFeeGasEstimator {
    /// Creates a new `PriorityFeeGasEstimator` with the specified RPC URL
    #[must_use]
    pub fn new(rpc_url: String, max_compute_units: u32) -> Self {
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            rpc_url,
            CommitmentConfig::processed(),
        ));
        Self {
            rpc_client,
            max_compute_units,
        }
    }
}

#[async_trait::async_trait]
impl GasEstimator for PriorityFeeGasEstimator {
    async fn ensure_enough_gas(
        &self,
        ixs: Vec<Instruction>,
        available_gas: u64,
    ) -> eyre::Result<GasEstimatorResult> {
        let accounts = ix_unique_account_addreses(&ixs);

        let average_piority_fee_micro_lamports =
            average_priorization_fee_micro_lamports(&accounts, &self.rpc_client).await?;

        let cost = calculate_fees(
            ixs,
            average_piority_fee_micro_lamports,
            self.max_compute_units,
        )?;

        if cost > available_gas {
            return Err(InsufficientGasBalance {
                cost,
                available: available_gas,
            }
            .into());
        }

        Ok(GasEstimatorResult {
            required_gas: cost,
            available_gas,
            average_piority_fee_micro_lamports,
        })
    }
}

/// Calculate total basic fees + priority fees + margin
fn calculate_fees(
    instructions: Vec<Instruction>,
    average_piority_fee_micro_lamports: u64,
    max_budget_units: u32,
) -> eyre::Result<u64> {
    // Calculate basic fee (unique signatures count * base fee)
    let mut fee = ixs_unique_signers_count(instructions)
        .checked_mul(SOLANA_LAMPORTS_PER_SIGNATURE_COST)
        .ok_or_eyre("Overflow when calculating basic fee")?;

    // Add priority compute units fee.
    // Convert from micro-lamports to lamports using fixed-point arithmetic (round-half-up)
    let average_piority_fee_lamports = average_piority_fee_micro_lamports
        .saturating_add(
            MICRO_LAMPORTS_PER_LAMPORT
                .checked_div(2)
                .ok_or_eyre("Overflow when calculating average priority fee in lamports")?,
        )
        .checked_div(MICRO_LAMPORTS_PER_LAMPORT)
        .ok_or_eyre("Overflow when calculating average priority fee in lamports")?;

    fee = fee
        .saturating_add((u64::from(max_budget_units)).saturating_mul(average_piority_fee_lamports));

    // Add Solana refcommended 10 % margin to fee
    let margin = fee
        .checked_mul(110)
        .ok_or_eyre("Overflow when calculating fee margin")?
        .checked_div(100)
        .ok_or_eyre("Overflow when calculating fee margin")?;

    fee.checked_add(margin)
        .ok_or_eyre("Overflow when calculating total fee")
}

/// Get unique account addresses from instructions
fn ix_unique_account_addreses(instructions: &[Instruction]) -> Vec<Pubkey> {
    instructions
        .iter()
        .flat_map(|ix| ix.accounts.iter().map(|acc| (acc.pubkey, acc)))
        .collect::<HashMap<_, _>>() // Use a HashMap to get unique accounts
        .keys()
        .copied()
        .collect::<Vec<Pubkey>>()
}

/// Calculate the number of unique signers across all instructions
#[allow(
    clippy::as_conversions,
    reason = "len() return value should fit in u64 all the time (even arch dependent)"
)]
fn ixs_unique_signers_count(instructions: Vec<Instruction>) -> u64 {
    instructions
        .into_iter()
        .flat_map(|ix| ix.accounts.into_iter())
        .filter(|account| account.is_signer)
        .map(|account| account.pubkey)
        .collect::<std::collections::HashSet<_>>()
        .len() as u64
}

/// Calculate average prioritization fee in micro-lamports for the given accounts
async fn average_priorization_fee_micro_lamports(
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

#[cfg(test)]
mod tests {
    use solana_sdk::instruction::AccountMeta;

    use super::*;

    #[test]
    fn test_calculate_ixs_signatures_empty() {
        let count = ixs_unique_signers_count(vec![]);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_calculate_ixs_signatures_unique_across_instructions() {
        let p1 = Pubkey::new_unique();
        let p2 = Pubkey::new_unique();
        let p3 = Pubkey::new_unique();

        // First instruction: p1 (signer), p2 (non-signer)
        let ix1 = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![
                AccountMeta::new(p1, true),           // signer
                AccountMeta::new_readonly(p2, false), // non-signer
            ],
            data: vec![],
        };

        // Second instruction: p1 again (signer), p3 (signer)
        let ix2 = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![
                AccountMeta::new_readonly(p1, true), // signer again
                AccountMeta::new(p3, true),          // signer
            ],
            data: vec![],
        };

        let count = ixs_unique_signers_count(vec![ix1, ix2]);
        // Unique signers are p1 and p3 => 2
        assert_eq!(count, 2);
    }

    #[test]
    fn test_calculate_ixs_signatures_ignores_non_signers() {
        let s1 = Pubkey::new_unique();
        let ns1 = Pubkey::new_unique();
        let ns2 = Pubkey::new_unique();

        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![
                AccountMeta::new_readonly(s1, true),   // signer
                AccountMeta::new(ns1, false),          // non-signer
                AccountMeta::new_readonly(ns2, false), // non-signer
            ],
            data: vec![],
        };

        let count = ixs_unique_signers_count(vec![ix]);
        assert_eq!(count, 1);
    }

    // -------- calculate_fees tests --------

    #[test]
    fn test_calculate_fees_zero_everything() {
        // No instructions (no signers), zero priority fee, zero budget
        let res = calculate_fees(vec![], 0, 0).unwrap();
        assert_eq!(res, 0);
    }

    #[test]
    fn test_calculate_fees_one_signer_no_priority() {
        // One signer, 0 micro-lamports priority fee => 0 lamports
        let p1 = Pubkey::new_unique();

        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new(p1, true)],
            data: vec![],
        };

        let base_fee = SOLANA_LAMPORTS_PER_SIGNATURE_COST; // 5_000
        let fee_before_margin = base_fee; // no priority
        let margin = fee_before_margin * 110 / 100; // per implementation (1.1x)
        let expected_total = fee_before_margin + margin; // 5_000 + 5_500 = 10_500

        let res = calculate_fees(vec![ix], 0, 0).unwrap();
        assert_eq!(res, expected_total);
    }

    #[test]
    fn test_calculate_fees_rounding_up_priority_fee() {
        // One signer, priority fee rounds up from 500_000 micro-lamports => 1 lamport
        let p1 = Pubkey::new_unique();
        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new_readonly(p1, true)],
            data: vec![],
        };

        let base_fee = SOLANA_LAMPORTS_PER_SIGNATURE_COST; // 5_000
        let avg_priority_fee_micro = MICRO_LAMPORTS_PER_LAMPORT / 2; // 500_000
        let max_budget_units = 1_000u32;

        // Rounds to 1 lamport per compute unit
        let priority_fee = u64::from(max_budget_units) * 1;
        let fee_before_margin = base_fee + priority_fee; // 5_000 + 1_000 = 6_000
        let margin = fee_before_margin * 110 / 100; // 6_600
        let expected_total = fee_before_margin + margin; // 12_600

        let res = calculate_fees(vec![ix], avg_priority_fee_micro, max_budget_units).unwrap();
        assert_eq!(res, expected_total);
    }

    #[test]
    fn test_calculate_fees_two_signers_with_priority() {
        // Two unique signers across two instructions, priority fee = 1_000_000 micro => 1 lamport
        let s1 = Pubkey::new_unique();
        let s2 = Pubkey::new_unique();

        let ix1 = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new_readonly(s1, true)],
            data: vec![],
        };
        let ix2 = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new(s2, true)],
            data: vec![],
        };

        let signers = 2u64;
        let base_fee = signers * SOLANA_LAMPORTS_PER_SIGNATURE_COST; // 10_000
        let avg_priority_fee_micro = MICRO_LAMPORTS_PER_LAMPORT; // 1 lamport per CU
        let max_budget_units = 1_400u32; // uses crate constant value style

        let priority_fee = u64::from(max_budget_units) * 1; // 1_400
        let fee_before_margin = base_fee + priority_fee; // 11_400
        let margin = fee_before_margin * 110 / 100; // 12_540
        let expected_total = fee_before_margin + margin; // 23_940

        let res = calculate_fees(vec![ix1, ix2], avg_priority_fee_micro, max_budget_units).unwrap();
        assert_eq!(res, expected_total);
    }
}

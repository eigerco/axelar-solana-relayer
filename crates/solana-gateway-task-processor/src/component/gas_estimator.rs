use core::fmt;
use std::collections::HashMap;
use std::sync::Arc;

use eyre::OptionExt as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_compute_budget::compute_budget_limits::MAX_COMPUTE_UNIT_LIMIT;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;
use solana_sdk::transaction::Transaction;
use thiserror::Error;

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
        keypair: &Keypair,
        available_gas: u64,
    ) -> eyre::Result<GasEstimatorResult>;
}

/// Result of gas estimation
pub struct GasEstimatorResult {
    /// Total required gas in lamports after estimation
    pub required_gas: u64,
    /// Available gas in lamports before estimation. This is returned for convenience.
    pub available_gas: u64,
    /// Calculated Instructions to set priority fees for the transaction
    /// Caller must add these to the transaction.
    pub priority_fee_ixs: Vec<Instruction>,
}

/// Actual implementation of `GasEstimator`
pub struct PriorityFeeGasEstimator {
    rpc_client: Arc<RpcClient>,
}

impl PriorityFeeGasEstimator {
    /// Creates a new `PriorityFeeGasEstimator` with the specified RPC URL
    #[must_use]
    pub fn new(rpc_url: String) -> Self {
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            rpc_url,
            CommitmentConfig::processed(),
        ));
        Self { rpc_client }
    }
}

#[async_trait::async_trait]
impl GasEstimator for PriorityFeeGasEstimator {
    async fn ensure_enough_gas(
        &self,
        ixs: Vec<Instruction>,
        keypair: &Keypair,
        available_gas: u64,
    ) -> eyre::Result<GasEstimatorResult> {
        let mut ixs = ixs.clone(); // Clone to avoid modifying the original instructions

        // Get average prioritization fee for involved accounts, this will be used to calculate CU
        // unit price
        let accounts = ix_unique_account_addreses(&ixs);
        let average_piority_fee_micro_lamports =
            average_priorization_fee_micro_lamports(&accounts, &self.rpc_client).await?;

        // Simulate transaction to estimate compute unit consumption, so we can set appropriate
        // limits
        let blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .await
            .map_err(|err| eyre::eyre!("Failed to get blockhash: {}", err))?;

        ixs.insert(
            0,
            ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNIT_LIMIT),
        );
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&keypair.pubkey()),
            &[keypair],
            blockhash,
        );

        let rpc_simulation_result =
            self.rpc_client
                .simulate_transaction(&tx)
                .await
                .map_err(|err| {
                    eyre::eyre!("Failed to simulate transaction for gas estimation: {}", err)
                })?;

        let consumed_cu_units = rpc_simulation_result
            .value
            .units_consumed
            .ok_or_eyre("Failed to get consumed compute units from simulation result")?;

        // Add Solana refcommended 10 % margin to CU unit consumption
        let effective_limit_cu_units = consumed_cu_units
            .checked_mul(110)
            .ok_or_eyre("Overflow when calculating CU margin")?
            .checked_div(100)
            .ok_or_eyre("Overflow when calculating CU margin")?;

        // With all the data gathered, calculate total fees
        let cost = calculate_fees(
            ixs,
            average_piority_fee_micro_lamports,
            effective_limit_cu_units,
        )?;

        // Return error if not enough gas is available
        if cost > available_gas {
            return Err(InsufficientGasBalance {
                cost,
                available: available_gas,
            }
            .into());
        }

        // Prepare priority fee instructions, so the caller can add these to the transaction.
        let priority_fee_ixs = vec![
            ComputeBudgetInstruction::set_compute_unit_price(average_piority_fee_micro_lamports),
            ComputeBudgetInstruction::set_compute_unit_limit(
                effective_limit_cu_units.try_into().map_err(|err| {
                    eyre::eyre!("Effective compute units exceed u32 max value: {}", err) // Weird case. This is a mismatch in Solana API.
                })?,
            ),
        ];

        Ok(GasEstimatorResult {
            required_gas: cost,
            available_gas,
            priority_fee_ixs,
        })
    }
}

/// Calculate total basic fees + priority fees
/// Returns total fee in lamports
///
/// # Arguments
///
/// * `instructions` - Vector of instructions for basing fee calculation
/// * `average_piority_fee_micro_lamports` - Average priority fee in micro-lamports, thats what
///   [`average_priorization_fee_micro_lamports`] returns.
/// * `effective_limit_cu_units` - Effective compute units to be used as limit. This usually already
///   includes some margin over the actual consumption.
fn calculate_fees(
    instructions: Vec<Instruction>,
    average_priority_fee_cu_cost_micro_lamports: u64,
    effective_limit_cu_units: u64,
) -> eyre::Result<u64> {
    // Calculate basic fee (unique signatures count * base fee)
    let fee = ixs_unique_signers_count(instructions)
        .checked_mul(SOLANA_LAMPORTS_PER_SIGNATURE_COST)
        .ok_or_eyre("Overflow when calculating basic fee")?;

    // Add priority compute units fee.
    // Convert from micro-lamports to lamports using fixed-point arithmetic (round-half-up)
    let average_priority_fee_cu_cost_lamports = average_priority_fee_cu_cost_micro_lamports
        .saturating_add(
            MICRO_LAMPORTS_PER_LAMPORT
                .checked_div(2)
                .ok_or_eyre("Overflow when calculating average priority fee in lamports")?,
        )
        .checked_div(MICRO_LAMPORTS_PER_LAMPORT)
        .ok_or_eyre("Overflow when calculating average priority fee in lamports")?;

    Ok(fee.saturating_add(
        effective_limit_cu_units.saturating_mul(average_priority_fee_cu_cost_lamports),
    ))
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

    // -------- calculate_fees tests (current signature) --------

    #[test]
    fn test_calculate_fees_zero_everything() {
        let res = calculate_fees(vec![], 0, 0).unwrap();
        assert_eq!(res, 0);
    }

    #[test]
    fn test_calculate_fees_one_signer_no_priority() {
        // One signer, 0 micro-lamports priority fee => 0 lamports per CU
        let p1 = Pubkey::new_unique();
        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new(p1, true)],
            data: vec![],
        };

        let expected = SOLANA_LAMPORTS_PER_SIGNATURE_COST; // base fee only
        let res = calculate_fees(vec![ix], 0, 1_000).unwrap();
        assert_eq!(res, expected);
    }

    #[test]
    fn test_calculate_fees_rounding_half_up_priority_fee() {
        // No signers, priority fee rounds from 500_000 micro to 1 lamport per CU
        let effective_limit_cu_units = 1_000u64;
        let avg_priority_micro = MICRO_LAMPORTS_PER_LAMPORT / 2; // 500_000
        let expected = effective_limit_cu_units * 1; // 1 lamport/CU

        let res = calculate_fees(vec![], avg_priority_micro, effective_limit_cu_units).unwrap();
        assert_eq!(res, expected);
    }

    #[test]
    fn test_calculate_fees_rounding_down_priority_fee() {
        // 499_999 micro-lamports should round down to 0 lamports per CU
        let effective_limit_cu_units = 2_000u64;
        let avg_priority_micro = (MICRO_LAMPORTS_PER_LAMPORT / 2) - 1; // 499_999

        let res = calculate_fees(vec![], avg_priority_micro, effective_limit_cu_units).unwrap();
        assert_eq!(res, 0);
    }

    #[test]
    fn test_calculate_fees_two_signers_with_priority() {
        // Two unique signers, priority = 1_000_000 micro => 1 lamport per CU
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

        let effective_limit_cu_units = 1_400u64;
        let base = 2 * SOLANA_LAMPORTS_PER_SIGNATURE_COST; // 10_000
        let priority = effective_limit_cu_units * 1; // 1 lamport/CU
        let expected = base + priority; // no extra add in current implementation

        let res = calculate_fees(
            vec![ix1, ix2],
            MICRO_LAMPORTS_PER_LAMPORT,
            effective_limit_cu_units,
        )
        .unwrap();
        assert_eq!(res, expected);
    }
}

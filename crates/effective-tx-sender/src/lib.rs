//! Utility that estimates the optimal compute budget and compute unit price to maximize the
//! chances of successfully submitting a transaction on the Solana network.
//!
//! This module provides the `EffectiveTxSender` struct, which helps simulate transactions
//! to determine the required compute units and appropriate compute unit price based on recent
//! network conditions.

use core::marker::PhantomData;
use std::collections::VecDeque;

use futures::TryFutureExt as _;
use itertools::Itertools as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_response::RpcSimulateTransactionResult;
use solana_compute_budget::compute_budget_processor::MAX_COMPUTE_UNIT_LIMIT;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer as _;

/// Typestate representing the transaction sender before simulation to evaluate compute limits.
pub struct Unevaluated;

/// Typestate representing the transaction sender after evaluating the best compute limits.
pub struct Evaluated;

/// A transaction sender that pre-calculates the optimal compute budget and compute unit price
/// to maximize the chances of transaction inclusion.
///
/// Internally, it maintains a deque of instructions, prepending compute budget and unit price
/// instructions after evaluation.
pub struct EffectiveTxSender<'a, T> {
    solana_rpc_client: &'a RpcClient,
    ixs: VecDeque<Instruction>,
    solana_keypair: &'a Keypair,
    hash: solana_sdk::hash::Hash,
    _type: PhantomData<T>,
}

impl<'a> EffectiveTxSender<'a, Unevaluated> {
    /// Creates a new `EffectiveTxSender` instance with the provided instructions.
    ///
    /// After initialization, the internal instruction deque contains the provided instructions.
    #[tracing::instrument(skip_all)]
    pub fn new(
        solana_rpc_client: &'a RpcClient,
        solana_keypair: &'a Keypair,
        ixs: VecDeque<Instruction>,
    ) -> Self {
        Self {
            solana_rpc_client,
            hash: solana_sdk::hash::Hash::new_from_array([0; 32]),
            ixs,
            solana_keypair,
            _type: PhantomData,
        }
    }

    /// Evaluates the compute budget and compute unit price instructions based on simulation.
    ///
    /// This method simulates the transaction to determine the optimal compute budget and compute
    /// unit price. It updates the instruction deque by prepending the computed instructions.
    ///
    /// Returns an error if the simulation fails.
    #[tracing::instrument(skip_all)]
    pub async fn evaluate_compute_ixs(
        mut self,
    ) -> Result<EffectiveTxSender<'a, Evaluated>, ComputeBudgetError> {
        const MAX_COMPUTE_BUDGET: u32 = 1_399_850;
        // Add the max possible compute budget instruction for simulation
        let cu_budget_for_simulation =
            ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_BUDGET);

        self.ixs.push_front(cu_budget_for_simulation);
        let valid_slice = self.ixs.make_contiguous();

        let compute_budget =
            compute_budget(valid_slice, self.solana_keypair, self.solana_rpc_client);
        let compute_unit_price = compute_unit_price(valid_slice, self.solana_rpc_client)
            .map_err(eyre::Error::from)
            .map_err(ComputeBudgetError::Generic);

        let (compute_unit_price, (compute_budget, hash)) =
            futures::try_join!(compute_unit_price, compute_budget)?;

        self.ixs.push_front(compute_unit_price);
        let valid_slice = self.ixs.make_contiguous();
        #[expect(
            clippy::indexing_slicing,
            reason = "we're guaranteed to have 2 elements at this point"
        )]
        {
            valid_slice[1] = compute_budget;
        };

        Ok(EffectiveTxSender {
            solana_rpc_client: self.solana_rpc_client,
            ixs: self.ixs,
            hash,
            solana_keypair: self.solana_keypair,
            _type: PhantomData,
        })
    }
}

impl<'a> EffectiveTxSender<'a, Evaluated> {
    /// Signs and sends the transaction.
    #[tracing::instrument(skip_all, err)]
    pub async fn send_tx(self) -> Result<Signature, ComputeBudgetError> {
        let valid_slice = self.ixs.as_slices().0;
        let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
            valid_slice,
            Some(&self.solana_keypair.pubkey()),
            &[self.solana_keypair],
            self.hash,
        );

        let signature = match self
            .solana_rpc_client
            .send_and_confirm_transaction(&tx)
            .await
        {
            Ok(signature) => signature,
            Err(err) if err.get_transaction_error().is_some() => {
                let signature = tx
                    .signatures
                    .first()
                    .expect("Signed transaction should have a signature");

                return Err(ComputeBudgetError::TransactionError {
                    source: err
                        .get_transaction_error()
                        .expect("Value shouldn't disappear after is_some returns true"),
                    signature: *signature,
                });
            }
            Err(err) => return Err(ComputeBudgetError::Generic(eyre::Error::from(err))),
        };

        Ok(signature)
    }
}

/// Error type representing possible failures during compute budget evaluation.
#[derive(thiserror::Error, Debug)]
pub enum ComputeBudgetError {
    /// Error occurred during transaction simulation.
    #[error("Simulation error: {0:?}")]
    SimulationError(RpcSimulateTransactionResult),

    /// An error occurred during the transaction RPC call.
    #[error("TransactionError: {source}")]
    TransactionError {
        /// The transaction error that occurred.
        source: solana_sdk::transaction::TransactionError,
        /// The signature of the transaction that caused the error.
        signature: Signature,
    },

    /// Generic, non-recoverable error.
    #[error("Generic error: {0}")]
    Generic(eyre::Error),
}

/// Computes the optimal compute budget instruction based on transaction simulation.
///
/// Simulates the transaction to estimate the compute units consumed, then adds a top-up percentage
/// to ensure sufficient compute units during execution.
///
/// Returns the compute budget instruction and the latest blockhash.
pub(crate) async fn compute_budget(
    ixs: &[Instruction],
    solana_keypair: &Keypair,
    solana_rpc_client: &RpcClient,
) -> Result<(Instruction, solana_sdk::hash::Hash), ComputeBudgetError> {
    const PERCENT_POINTS_TO_TOP_UP: u64 = 10;

    let hash = solana_rpc_client
        .get_latest_blockhash()
        .await
        .map_err(eyre::Error::from)
        .map_err(ComputeBudgetError::Generic)?;
    let tx_to_simulate = solana_sdk::transaction::Transaction::new_signed_with_payer(
        ixs,
        Some(&solana_keypair.pubkey()),
        &[solana_keypair],
        hash,
    );
    let simulation_result = solana_rpc_client
        .simulate_transaction(&tx_to_simulate)
        .await
        .map_err(eyre::Error::from)
        .map_err(ComputeBudgetError::Generic)?;
    if simulation_result.value.err.is_some() {
        return Err(ComputeBudgetError::SimulationError(simulation_result.value));
    }
    let computed_units = simulation_result.value.units_consumed.unwrap_or(0);
    let top_up = computed_units
        .checked_div(PERCENT_POINTS_TO_TOP_UP)
        .unwrap_or(0);
    let compute_budget = computed_units.saturating_add(top_up);
    let compute_budget = compute_budget.min(u64::from(MAX_COMPUTE_UNIT_LIMIT)); // Safe conversion since we move from an u32 to u64
    let ix = ComputeBudgetInstruction::set_compute_unit_limit(
        compute_budget
            .try_into()
            .map_err(eyre::Error::from)
            .map_err(ComputeBudgetError::Generic)?,
    );
    Ok((ix, hash))
}

/// Computes the optimal compute unit price instruction based on recent prioritization fees.
///
/// Analyzes recent prioritization fees for accounts involved in the transaction to calculate an
/// average fee, which is then used to set the compute unit price.
///
/// Returns the compute unit price instruction.
pub(crate) async fn compute_unit_price(
    ixs: &[Instruction],
    solana_rpc_client: &RpcClient,
) -> Result<Instruction, eyre::Error> {
    const MAX_ACCOUNTS: usize = 128;
    const N_SLOTS_TO_CHECK: usize = 10;

    let all_touched_accounts = ixs
        .iter()
        .flat_map(|x| x.accounts.as_slice())
        .take(MAX_ACCOUNTS)
        .map(|x| x.pubkey)
        .collect_vec();
    let fees = solana_rpc_client
        .get_recent_prioritization_fees(&all_touched_accounts)
        .await?;
    let (sum, count) = fees
        .into_iter()
        .rev()
        .take(N_SLOTS_TO_CHECK)
        .map(|x| x.prioritization_fee)
        // Simple rolling average of the last `N_SLOTS_TO_CHECK` items.
        .fold((0_u64, 0_u64), |(sum, count), fee| {
            (sum.saturating_add(fee), count.saturating_add(1))
        });
    let average = if count > 0 {
        sum.checked_div(count).unwrap_or(0)
    } else {
        0
    };
    Ok(ComputeBudgetInstruction::set_compute_unit_price(average))
}

//! Parse Amplifier API events, translate them to transaction actions to exesute on the Solana
//! blockchain

mod component;
mod config;
pub use component::{PriorityFeeGasEstimator, SolanaTxPusher, MAX_COMPUTE_UNITS};
pub use config::Config;

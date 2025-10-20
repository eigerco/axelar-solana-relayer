//! Parse Amplifier API events, translate them to transaction actions to exesute on the Solana
//! blockchain

mod component;
mod config;
pub use component::{PriorityFeeGasEstimator, SolanaTxPusher};
pub use config::Config;

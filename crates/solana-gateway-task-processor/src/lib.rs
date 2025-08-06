//! Parse Amplifier API events, translate them to transaction actions to exesute on the Solana
//! blockchain

mod component;
mod config;
pub use component::{GasEstimator, RealGasEstimator, SolanaTxPusher};
pub use config::Config;

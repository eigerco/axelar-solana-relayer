//! Parse Solana events, trnasform them into Amplifier API events
//! forward the Amplifier API events over to the Amplifier API

mod component;
mod config;
pub use component::SolanaEventForwarder;
pub use config::Config;

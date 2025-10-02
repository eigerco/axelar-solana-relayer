//! Parse Solana events, trnasform them into Amplifier API events
//! forward the Amplifier API events over to the Amplifier API

mod component;
mod config;
mod utils;

pub use component::SolanaEventForwarder;
pub use config::Config;
pub use utils::{convert_to_parser_transaction, map_core_events_to_amplifier};

//! This crate provides a REST service component for the relayer.
pub mod component;
mod config;
pub mod endpoints;

pub use component::RestService;
pub use config::Config;

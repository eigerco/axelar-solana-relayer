//! This crate provides a REST service component for the relayer.
mod component;
mod config;
mod endpoints;

pub use component::RestService;
pub use config::Config;

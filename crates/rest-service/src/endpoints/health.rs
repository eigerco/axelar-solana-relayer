//! Health endpoint, returns 200 OK if the service is up and running.
use axum::routing::{get, MethodRouter};

pub(crate) const PATH: &str = "/health";
pub(crate) fn handlers() -> MethodRouter {
    get(get_handler)
}

async fn get_handler() {}

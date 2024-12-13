//! Health endpoint, returns 200 OK if the service is up and running.
use std::sync::Arc;

use axum::routing::{get, MethodRouter};

use crate::component::ServiceState;

pub(crate) const PATH: &str = "/health";
pub(crate) fn handlers() -> MethodRouter<Arc<ServiceState>> {
    get(get_handler)
}

async fn get_handler() {}

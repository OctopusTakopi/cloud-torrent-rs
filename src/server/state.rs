use crate::engine::Engine;
use axum::extract::FromRef;
use std::sync::Arc;
use tokio::sync::broadcast;

pub struct AppState {
    pub engine: Engine,
    pub expected_auth: Option<String>,
    pub sys: tokio::sync::Mutex<sysinfo::System>,
}

#[derive(Clone)]
pub struct SharedState {
    pub app_state: Arc<AppState>,
    pub broadcast_tx: broadcast::Sender<()>,
}

impl FromRef<SharedState> for Arc<AppState> {
    fn from_ref(state: &SharedState) -> Self {
        state.app_state.clone()
    }
}

impl FromRef<SharedState> for broadcast::Sender<()> {
    fn from_ref(state: &SharedState) -> Self {
        state.broadcast_tx.clone()
    }
}

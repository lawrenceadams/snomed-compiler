use std::sync::Arc;

use axum::extract::State;
use axum::{Json, extract::Path};
use serde_json::{Value, json};
use tracing::info;

use crate::serve::AppState;

pub async fn get_parents(
    Path(sctid): Path<u64>,
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    info!("get_parents: {sctid}");

    let snomed_db = state.snomed_db.clone();
    let ids = snomed_db.parents(sctid);

    json!(ids).into()
}

pub async fn get_children(
    Path(sctid): Path<u64>,
    State(state): State<Arc<AppState>>,
) -> Json<Value> {
    info!("get_children: {sctid}");

    let snomed_db = state.snomed_db.clone();
    let ids = snomed_db.children(sctid);

    json!(ids).into()
}

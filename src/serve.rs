use std::{path::PathBuf, sync::Arc};

use axum::{Router, routing::get};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::query::SnomedDb;
mod handlers;

#[derive(Clone)]
#[allow(dead_code)]
/// AppState that can be passed around to different
struct AppState {
    snomed_db: Arc<SnomedDb>,
}

pub async fn run_server(db_path: PathBuf) {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .init();

    // env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let snomed_db = SnomedDb::open(&db_path).unwrap();

    let app_state = AppState {
        snomed_db: Arc::new(snomed_db),
    };

    let app: Router = Router::new()
        .route("/", get(|| async { "snomed_compiler" }))
        .route("/parents/{sctid}", get(handlers::get_parents))
        .route("/children/{sctid}", get(handlers::get_children))
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(app_state));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    info!("starting listener");
    axum::serve(listener, app).await.unwrap();
}

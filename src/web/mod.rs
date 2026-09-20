//! Web dashboard: auth, room list/detail, personality/mode assignment, SSE.

pub mod auth;
pub mod handlers;

use crate::personalities::Personalities;
use crate::store::Store;
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use time::Duration;
use tokio::sync::broadcast;
use tower_sessions::cookie::SameSite;
use tower_sessions::{Expiry, MemoryStore, SessionManagerLayer};

/// A single chat message published for live-updating room views over SSE.
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub room_id: String,
    pub sender: String,
    pub body: String,
}

/// Shared application state for the web dashboard.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub personalities: Arc<Personalities>,
    pub tx: broadcast::Sender<SseEvent>,
    pub(crate) login_limiter: Arc<auth::LoginLimiter>,
}

impl AppState {
    pub fn new(store: Store, personalities: Arc<Personalities>) -> Self {
        let (tx, _rx) = broadcast::channel(256);
        Self { store, personalities, tx, login_limiter: Arc::new(auth::LoginLimiter::default()) }
    }
}

/// Build the full axum router: public auth routes + session-authenticated
/// dashboard routes, wrapped in the session management layer.
pub fn build_router(state: AppState) -> Router {
    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(true)
        .with_http_only(true)
        .with_same_site(SameSite::Strict)
        .with_expiry(Expiry::OnInactivity(Duration::hours(12)));

    let public = Router::new()
        .route("/login", get(handlers::login_page).post(handlers::login_submit))
        .route("/logout", post(handlers::logout));

    let protected = Router::new()
        .route("/", get(handlers::rooms_index))
        .route("/rooms/:id", get(handlers::room_detail))
        .route("/rooms/:id/personality", post(handlers::set_personality))
        .route("/rooms/:id/mode", post(handlers::set_mode))
        .route("/events", get(handlers::sse_events))
        .route_layer(middleware::from_fn(auth::require_admin));

    Router::new()
        .merge(public)
        .merge(protected)
        .with_state(state)
        .layer(session_layer)
}

/// Serve the dashboard over HTTPS using the given PEM cert/key pair.
pub async fn serve_tls(
    state: AppState,
    bind: &str,
    cert: &Path,
    key: &Path,
) -> anyhow::Result<()> {
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
    let addr: SocketAddr = bind.parse()?;
    let app = build_router(state);
    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;
    Ok(())
}

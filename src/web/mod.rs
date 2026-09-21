//! Web dashboard: auth, room list/detail, personality/mode assignment, SSE.

pub mod auth;
pub mod handlers;

use crate::llm::OllamaClient;
use crate::metrics::Metrics;
use crate::orchestrator::Dispatcher;
use crate::personalities::Personalities;
use crate::store::Store;
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, OnceLock};
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
    pub metrics: Arc<Metrics>,
    pub ollama: Arc<OllamaClient>,
    /// Filled in once `main` builds the `Dispatcher` (which itself needs a
    /// `Router` that in turn needs signal/LLM wiring built up later than the
    /// web server is spawned). `/api/metrics` reads through this cell and
    /// reports empty orchestration gauges until it is set.
    pub dispatcher: Arc<OnceLock<Arc<Dispatcher>>>,
}

impl AppState {
    pub fn new(
        store: Store,
        personalities: Arc<Personalities>,
        metrics: Arc<Metrics>,
        ollama: Arc<OllamaClient>,
        dispatcher: Arc<OnceLock<Arc<Dispatcher>>>,
    ) -> Self {
        let (tx, _rx) = broadcast::channel(256);
        Self {
            store,
            personalities,
            tx,
            login_limiter: Arc::new(auth::LoginLimiter::default()),
            metrics,
            ollama,
            dispatcher,
        }
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
        .route("/settings", get(handlers::settings_page).post(handlers::settings_submit))
        .route("/events", get(handlers::sse_events))
        .route("/health", get(handlers::health_page))
        .route("/api/metrics", get(handlers::api_metrics))
        .route_layer(middleware::from_fn(auth::require_admin));

    Router::new()
        .merge(public)
        .merge(protected)
        .with_state(state)
        .layer(session_layer)
}

/// Build the rustls TLS config from a PEM cert/key pair, installing the process
/// crypto provider first.
///
/// rustls 0.23 requires a process-level `CryptoProvider` to be installed before any
/// TLS config is built. Both aws-lc-rs (via axum-server) and ring (via rcgen) are in
/// the dependency tree, so rustls cannot auto-select one — we install aws-lc-rs
/// explicitly. The install is idempotent: a second call (e.g. on the supervised retry
/// loop) returns an error we deliberately ignore.
pub async fn load_tls_config(
    cert: &Path,
    key: &Path,
) -> anyhow::Result<axum_server::tls_rustls::RustlsConfig> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    Ok(axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?)
}

/// Serve the dashboard over HTTPS using the given PEM cert/key pair.
pub async fn serve_tls(
    state: AppState,
    bind: &str,
    cert: &Path,
    key: &Path,
) -> anyhow::Result<()> {
    let config = load_tls_config(cert, key).await?;
    let addr: SocketAddr = bind.parse()?;
    let app = build_router(state);
    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;
    Ok(())
}

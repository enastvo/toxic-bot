use axum::body::Body;
use axum::http::{Request, StatusCode};
use signal_bot::llm::OllamaClient;
use signal_bot::metrics::Metrics;
use signal_bot::personalities::Personalities;
use signal_bot::store::{SettingsRow, Store};
use signal_bot::web::{build_router, AppState};
use std::io::Write;
use std::sync::{Arc, OnceLock};
use tower::ServiceExt;

fn default_settings() -> SettingsRow {
    SettingsRow {
        keep_alive: "30m".into(),
        ollama_timeout_secs: 300,
        repeat_penalty: 1.3,
        repeat_last_n: 256,
        num_predict: 512,
        num_ctx: 8192,
        default_temperature: 0.7,
        default_top_p: 0.9,
        summary_enabled: true,
        summary_interval_hours: 6,
    }
}

async fn state() -> (AppState, Store) {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.set_admin("admin", "pw").await.unwrap();
    store.upsert_settings(&default_settings()).await.unwrap();
    let d = tempfile::tempdir().unwrap();
    let body = "label=\"D\"\nsystem_prompt=\"x\"\nmodel=\"m\"\ntemperature=0.5\ntop_p=0.9\nnum_ctx=4096\n[proactive]\nrelevance_threshold=0.7\ncooldown_secs=60\nmax_per_hour=5\n";
    std::fs::File::create(d.path().join("default.toml")).unwrap().write_all(body.as_bytes()).unwrap();
    let p = Arc::new(Personalities::load_dir(d.path()).unwrap());
    std::mem::forget(d);
    let st = store.clone();
    let ollama = Arc::new(OllamaClient::new("http://127.0.0.1:0", 300));
    (AppState::new(store, p, Metrics::new(), ollama, Arc::new(OnceLock::new())), st)
}

/// Extract the `Set-Cookie` header value (name=value only, no attributes)
/// so it can be replayed as a `Cookie` request header.
fn session_cookie(resp: &axum::http::Response<Body>) -> String {
    let set_cookie = resp
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .expect("Set-Cookie header present")
        .to_str()
        .unwrap();
    set_cookie.split(';').next().unwrap().to_string()
}

async fn login_cookie(app: axum::Router) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(axum::http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("username=admin&password=pw"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    session_cookie(&resp)
}

fn valid_form_body(num_predict: i64) -> String {
    format!(
        "keep_alive=30m&ollama_timeout_secs=300&repeat_penalty=1.3&repeat_last_n=256&\
         num_predict={num_predict}&num_ctx=8192&default_temperature=0.7&default_top_p=0.9&\
         summary_enabled=true&summary_interval_hours=6"
    )
}

#[tokio::test]
async fn get_settings_authed_renders_current_values() {
    let (state, _store) = state().await;
    let app = build_router(state);
    let cookie = login_cookie(app.clone()).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/settings")
                .header(axum::http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("num_predict"), "body should mention num_predict: {text}");
}

#[tokio::test]
async fn get_settings_unauthenticated_is_redirected() {
    let (state, _store) = state().await;
    let app = build_router(state);
    let resp = app
        .oneshot(Request::builder().uri("/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn post_settings_valid_updates_store_and_redirects() {
    let (state, store) = state().await;
    let app = build_router(state);
    let cookie = login_cookie(app.clone()).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings")
                .header(axum::http::header::COOKIE, cookie)
                .header(axum::http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(valid_form_body(1024)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let row = store.get_settings().await.unwrap();
    assert_eq!(row.num_predict, 1024);
}

#[tokio::test]
async fn get_api_metrics_authed_returns_json_with_expected_keys() {
    let (state, _store) = state().await;
    let app = build_router(state);
    let cookie = login_cookie(app.clone()).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/metrics")
                .header(axum::http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .expect("content-type header present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(content_type.starts_with("application/json"), "content-type was: {content_type}");

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("system").is_some(), "missing 'system' key: {json}");
    assert!(json.get("ollama").is_some(), "missing 'ollama' key: {json}");
    assert!(json.get("llm").is_some(), "missing 'llm' key: {json}");
    assert!(json.get("orchestration").is_some(), "missing 'orchestration' key: {json}");
    // No real Ollama in the test env: `ps()` against 127.0.0.1:0 must fail and
    // degrade gracefully rather than error the handler.
    assert_eq!(json["ollama"]["reachable"], serde_json::json!(false));
}

#[tokio::test]
async fn post_settings_invalid_rerenders_with_error_and_leaves_store_unchanged() {
    let (state, store) = state().await;
    let app = build_router(state);
    let cookie = login_cookie(app.clone()).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings")
                .header(axum::http::header::COOKIE, cookie)
                .header(axum::http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(valid_form_body(99999)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let row = store.get_settings().await.unwrap();
    assert_eq!(row.num_predict, 512, "store must be unchanged on validation failure");
}

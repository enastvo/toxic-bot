use axum::body::Body;
use axum::http::{Request, StatusCode};
use signal_bot::llm::OllamaClient;
use signal_bot::metrics::Metrics;
use signal_bot::personalities::Personalities;
use signal_bot::store::Store;
use signal_bot::types::ReplyMode;
use signal_bot::web::{build_router, AppState};
use std::io::Write;
use std::sync::{Arc, OnceLock};
use tower::ServiceExt;

async fn state() -> (AppState, Store) {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.set_admin("admin", "pw").await.unwrap();
    store.ensure_room("G", Some("Grp"), true).await.unwrap();
    let d = tempfile::tempdir().unwrap();
    let body = "label=\"D\"\nsystem_prompt=\"x\"\nmodel=\"m\"\ntemperature=0.5\ntop_p=0.9\nnum_ctx=4096\n[proactive]\nrelevance_threshold=0.7\ncooldown_secs=60\nmax_per_hour=5\n";
    std::fs::File::create(d.path().join("default.toml")).unwrap().write_all(body.as_bytes()).unwrap();
    let p = Arc::new(Personalities::load_dir(d.path()).unwrap());
    std::mem::forget(d);
    let st = store.clone();
    let ollama = Arc::new(OllamaClient::new("http://127.0.0.1:0", 300));
    (AppState::new(store, p, Metrics::new(), ollama, Arc::new(OnceLock::new())), st)
}

#[tokio::test]
async fn unauthenticated_root_redirects_to_login() {
    let (state, _store) = state().await;
    let app = build_router(state);
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER); // 303 -> /login
}

#[tokio::test]
async fn login_page_is_public() {
    let (state, _store) = state().await;
    let app = build_router(state);
    let resp = app
        .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
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

#[tokio::test]
async fn authenticated_flow_login_view_and_assign_mode() {
    let (state, store) = state().await;
    let app = build_router(state);

    // POST /login with correct creds -> redirect + session cookie
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from("username=admin&password=pw"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cookie = session_cookie(&resp);

    // GET / with cookie -> 200
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(axum::http::header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // POST /rooms/G/mode mode=proactive -> 303 and store reflects change
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rooms/G/mode")
                .header(axum::http::header::COOKIE, cookie.clone())
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from("mode=proactive"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let room = store.get_room("G").await.unwrap().unwrap();
    assert_eq!(room.reply_mode, ReplyMode::Proactive);
}

#[tokio::test]
async fn set_persona_sources_updates_db_and_renders() {
    let (state, store) = state().await;
    let app = build_router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder().method("POST").uri("/login")
                .header(axum::http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("username=admin&password=pw")).unwrap(),
        ).await.unwrap();
    let cookie = session_cookie(&resp);

    // POST domains for room G's current persona ("default") -> 303 + DB updated
    let resp = app
        .clone()
        .oneshot(
            Request::builder().method("POST").uri("/rooms/G/persona-sources")
                .header(axum::http::header::COOKIE, cookie.clone())
                .header(axum::http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("domains=cnn.com%2C%20foxnews.com")).unwrap(),
        ).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        store.get_persona_domains("default").await.unwrap(),
        vec!["cnn.com".to_string(), "foxnews.com".to_string()]
    );

    // GET /rooms/G shows the configured domains
    let resp = app
        .clone()
        .oneshot(
            Request::builder().uri("/rooms/G")
                .header(axum::http::header::COOKIE, cookie.clone())
                .body(Body::empty()).unwrap(),
        ).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("cnn.com"), "room page should show configured sources");
}

/// Regression test: `set_mode`/`set_personality` used to build the redirect
/// `Location` header from the raw, unvalidated `:id` path segment via
/// `Redirect::to(&format!("/rooms/{room_id}"))`. Since `Redirect::to` panics
/// (`HeaderValue::try_from(..).expect(..)`) on a string containing control
/// characters, and a nonexistent/attacker-controlled room id was never
/// checked for existence first, an authenticated admin could crash the
/// request task by posting to an unknown room. The fix checks
/// `store.get_room` first and returns 404 before doing anything else.
#[tokio::test]
async fn set_mode_on_unknown_room_is_not_found_not_a_panic() {
    let (state, _store) = state().await;
    let app = build_router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from("username=admin&password=pw"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cookie = session_cookie(&resp);

    // "does-not-exist" was never created via ensure_room; the handler must
    // 404 rather than proceed to build a redirect from an unvalidated id.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rooms/does-not-exist/mode")
                .header(axum::http::header::COOKIE, cookie.clone())
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from("mode=proactive"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

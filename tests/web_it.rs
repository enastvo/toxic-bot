use axum::body::Body;
use axum::http::{Request, StatusCode};
use signal_bot::personalities::Personalities;
use signal_bot::store::Store;
use signal_bot::types::ReplyMode;
use signal_bot::web::{build_router, AppState};
use std::io::Write;
use std::sync::Arc;
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
    (AppState::new(store, p), st)
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

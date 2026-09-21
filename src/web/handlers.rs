//! Askama template structs and axum handlers for the web dashboard.

use super::auth::{self, LoginLimiter};
use super::{AppState, SseEvent};
use crate::metrics;
use crate::settings;
use crate::store::SettingsRow;
use crate::types::ReplyMode;
use askama::Template;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Form, Json};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::sync::broadcast;
use tower_sessions::Session;

// ---- templates ----------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

struct RoomRow {
    room_id: String,
    display_name: String,
    personality: String,
    reply_mode: String,
}

#[derive(Template)]
#[template(path = "rooms.html")]
struct RoomsTemplate {
    rooms: Vec<RoomRow>,
}

struct MessageRow {
    sender: String,
    body: String,
    role: String,
}

struct PersonalityOption {
    name: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "room.html")]
struct RoomTemplate {
    room_id: String,
    display_name: String,
    messages: Vec<MessageRow>,
    personalities: Vec<PersonalityOption>,
    mode_addressed: bool,
    mode_always: bool,
    mode_proactive: bool,
}

#[derive(Template)]
#[template(path = "health.html")]
struct HealthTemplate;

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    keep_alive: String,
    ollama_timeout_secs: i64,
    repeat_penalty: f64,
    repeat_last_n: i64,
    num_predict: i64,
    num_ctx: i64,
    default_temperature: f64,
    default_top_p: f64,
    summary_enabled: bool,
    summary_interval_hours: i64,
    error: Option<String>,
    saved: bool,
}

impl SettingsTemplate {
    fn from_row(row: &SettingsRow, error: Option<String>, saved: bool) -> Self {
        Self {
            keep_alive: row.keep_alive.clone(),
            ollama_timeout_secs: row.ollama_timeout_secs,
            repeat_penalty: row.repeat_penalty,
            repeat_last_n: row.repeat_last_n,
            num_predict: row.num_predict,
            num_ctx: row.num_ctx,
            default_temperature: row.default_temperature,
            default_top_p: row.default_top_p,
            summary_enabled: row.summary_enabled,
            summary_interval_hours: row.summary_interval_hours,
            error,
            saved,
        }
    }
}

// ---- forms ----------------------------------------------------------

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
pub struct PersonalityForm {
    personality: String,
}

#[derive(Deserialize)]
pub struct ModeForm {
    mode: String,
}

#[derive(Deserialize)]
pub struct SettingsForm {
    keep_alive: String,
    ollama_timeout_secs: i64,
    repeat_penalty: f64,
    repeat_last_n: i64,
    num_predict: i64,
    num_ctx: i64,
    default_temperature: f64,
    default_top_p: f64,
    // HTML checkboxes post "on"/nothing, which serde's bool parser rejects.
    // Rendered as a <select> with values "true"/"false" instead, then mapped
    // to bool here (anything other than the literal "true" is false).
    summary_enabled: String,
    summary_interval_hours: i64,
}

impl From<SettingsForm> for SettingsRow {
    fn from(f: SettingsForm) -> Self {
        SettingsRow {
            keep_alive: f.keep_alive,
            ollama_timeout_secs: f.ollama_timeout_secs,
            repeat_penalty: f.repeat_penalty,
            repeat_last_n: f.repeat_last_n,
            num_predict: f.num_predict,
            num_ctx: f.num_ctx,
            default_temperature: f.default_temperature,
            default_top_p: f.default_top_p,
            summary_enabled: f.summary_enabled == "true",
            summary_interval_hours: f.summary_interval_hours,
        }
    }
}

#[derive(Deserialize)]
pub struct SettingsQuery {
    saved: Option<String>,
}

fn html(body: String) -> Response {
    Html(body).into_response()
}

fn client_ip(connect_info: Option<ConnectInfo<SocketAddr>>) -> IpAddr {
    connect_info
        .map(|ConnectInfo(addr)| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

// ---- auth handlers ----------------------------------------------------------

pub async fn login_page() -> Response {
    html(LoginTemplate { error: None }.render().unwrap())
}

pub async fn login_submit(
    State(state): State<AppState>,
    session: Session,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    Form(form): Form<LoginForm>,
) -> Response {
    let limiter: &LoginLimiter = &state.login_limiter;
    if !limiter.allow(client_ip(connect_info)) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            html(
                LoginTemplate { error: Some("Too many attempts, try again later.".into()) }
                    .render()
                    .unwrap(),
            ),
        )
            .into_response();
    }

    match state.store.verify_admin(&form.username, &form.password).await {
        Ok(true) => {
            if auth::mark_admin(&session).await.is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            Redirect::to("/").into_response()
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            html(LoginTemplate { error: Some("Invalid username or password.".into()) }.render().unwrap()),
        )
            .into_response(),
    }
}

pub async fn logout(session: Session) -> Response {
    let _ = session.flush().await;
    Redirect::to("/login").into_response()
}

// ---- dashboard handlers ----------------------------------------------------------

pub async fn rooms_index(State(state): State<AppState>) -> Response {
    let rooms = match state.store.list_rooms().await {
        Ok(r) => r,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let rows = rooms
        .into_iter()
        .map(|r| RoomRow {
            room_id: r.room_id.clone(),
            display_name: r.display_name.unwrap_or(r.room_id),
            personality: r.personality.unwrap_or_else(|| "default".to_string()),
            reply_mode: r.reply_mode.as_str().to_string(),
        })
        .collect();
    html(RoomsTemplate { rooms: rows }.render().unwrap())
}

pub async fn room_detail(State(state): State<AppState>, Path(room_id): Path<String>) -> Response {
    let room = match state.store.get_room(&room_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let history = state.store.history(&room_id, 100).await.unwrap_or_default();
    let messages = history
        .into_iter()
        .map(|m| MessageRow {
            sender: m.sender_name.unwrap_or(m.sender_id),
            body: m.body,
            role: m.role.as_str().to_string(),
        })
        .collect();
    let current_personality = room.personality.clone().unwrap_or_else(|| "default".to_string());
    let personalities = state
        .personalities
        .list()
        .into_iter()
        .map(|p| PersonalityOption { selected: p.name == current_personality, name: p.name.clone() })
        .collect();
    let reply_mode = room.reply_mode;

    html(
        RoomTemplate {
            room_id: room.room_id.clone(),
            display_name: room.display_name.unwrap_or(room.room_id),
            messages,
            personalities,
            mode_addressed: reply_mode == ReplyMode::Addressed,
            mode_always: reply_mode == ReplyMode::Always,
            mode_proactive: reply_mode == ReplyMode::Proactive,
        }
        .render()
        .unwrap(),
    )
}

pub async fn set_personality(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Form(form): Form<PersonalityForm>,
) -> Response {
    // Validate the room exists before doing anything else: `room_id` comes
    // straight from the URL path (percent-decoded) and is later reflected
    // into a `Location` header via `Redirect::to`, which panics if the
    // string contains control characters (e.g. CR/LF). Real room ids come
    // from `ensure_room`/Signal group ids and never contain such bytes, so
    // this existence check both gives a sane 404 for bogus ids and closes
    // off that panic/DoS path.
    match state.store.get_room(&room_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }

    let name = form.personality.trim();
    let value: Option<&str> = if name.is_empty() || name.eq_ignore_ascii_case("default") {
        None
    } else {
        Some(name)
    };
    if let Some(n) = value {
        if state.personalities.get(n).is_none() {
            return (StatusCode::BAD_REQUEST, "unknown personality").into_response();
        }
    }
    if state.store.set_personality(&room_id, value).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Redirect::to(&format!("/rooms/{room_id}")).into_response()
}

pub async fn set_mode(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    Form(form): Form<ModeForm>,
) -> Response {
    // See set_personality: existence check first, both for a sane 404 and to
    // avoid building a Location header from an unvalidated room id.
    match state.store.get_room(&room_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }

    let Some(mode) = ReplyMode::parse(form.mode.trim()) else {
        return (StatusCode::BAD_REQUEST, "unknown reply mode").into_response();
    };
    if state.store.set_reply_mode(&room_id, mode).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Redirect::to(&format!("/rooms/{room_id}")).into_response()
}

pub async fn settings_page(State(state): State<AppState>, Query(q): Query<SettingsQuery>) -> Response {
    let row = match state.store.get_settings().await {
        Ok(r) => r,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    html(SettingsTemplate::from_row(&row, None, q.saved.is_some()).render().unwrap())
}

pub async fn settings_submit(
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Response {
    let row: SettingsRow = form.into();
    if let Err(msg) = settings::validate(&row) {
        return html(SettingsTemplate::from_row(&row, Some(msg), false).render().unwrap());
    }
    if state.store.upsert_settings(&row).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Redirect::to("/settings?saved=1").into_response()
}

// ---- health / metrics ----------------------------------------------------------

pub async fn health_page() -> Response {
    html(HealthTemplate.render().unwrap())
}

/// `GET /api/metrics`: a snapshot combining system resources, Ollama's loaded
/// models, the LLM turn-metrics ring, and the dispatcher's orchestration
/// gauges. Degrades gracefully: an unreachable Ollama or a not-yet-built
/// dispatcher (the web server starts before the dispatcher does, see `main`)
/// never fails the request, they just report as absent/empty.
pub async fn api_metrics(State(state): State<AppState>) -> Response {
    let mut system = metrics::system_snapshot();
    system.uptime_secs = state.metrics.uptime_secs();

    let llm = state.metrics.snapshot();

    let ollama = match state.ollama.ps().await {
        Ok(ps) => json!({"reachable": true, "models": ps.models}),
        Err(_) => json!({"reachable": false}),
    };

    let orchestration = state.dispatcher.get().map(|d| d.gauges()).unwrap_or_default();

    Json(json!({
        "system": system,
        "ollama": ollama,
        "llm": llm,
        "orchestration": orchestration,
    }))
    .into_response()
}

// ---- SSE ----------------------------------------------------------

pub async fn sse_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.tx.subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let event = sse_event_for(&ev);
                    return Some((Ok(event), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn sse_event_for(ev: &SseEvent) -> Event {
    let payload = serde_json::json!({
        "room_id": ev.room_id,
        "sender": ev.sender,
        "body": ev.body,
    });
    Event::default().event("message").data(payload.to_string())
}

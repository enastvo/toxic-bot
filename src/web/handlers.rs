//! Askama template structs and axum handlers for the web dashboard.

use super::auth::{self, LoginLimiter};
use super::{AppState, SseEvent};
use crate::types::ReplyMode;
use askama::Template;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use futures::stream::Stream;
use serde::Deserialize;
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

//! Core domain types shared across modules: reply modes, rooms, incoming and
//! stored messages, and LLM chat turns.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyMode { Addressed, Always, Proactive }

impl ReplyMode {
    pub fn as_str(self) -> &'static str {
        match self { Self::Addressed => "addressed", Self::Always => "always", Self::Proactive => "proactive" }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s { "addressed" => Some(Self::Addressed), "always" => Some(Self::Always), "proactive" => Some(Self::Proactive), _ => None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role { User, Assistant }

impl Role {
    pub fn as_str(self) -> &'static str { match self { Self::User => "user", Self::Assistant => "assistant" } }
    pub fn parse(s: &str) -> Role { if s == "assistant" { Role::Assistant } else { Role::User } }
}

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub room_id: String,
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub body: String,
    pub is_group: bool,
    pub is_mention: bool,
    pub quoted_msg: Option<String>,
    pub timestamp: i64, // ms since epoch (signal server ts)
}

#[derive(Debug, Clone)]
pub struct Room {
    pub room_id: String,
    pub display_name: Option<String>,
    pub is_group: bool,
    pub personality: Option<String>,
    pub reply_mode: ReplyMode,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: i64,
    pub room_id: String,
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub role: Role,
    pub body: String,
    pub ts: i64,
    pub personality: Option<String>,
    pub is_mention: bool,
}

/// One turn as sent to the LLM.
#[derive(Debug, Clone)]
pub struct ChatTurn { pub role: Role, pub name: Option<String>, pub content: String }

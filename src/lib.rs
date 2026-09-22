//! `signal-bot`: a self-hosted Signal chat bot backed by a local Ollama LLM,
//! with per-room personalities, reply modes, optional tools, and a LAN-only
//! TLS admin dashboard. See `README.md` for deployment and architecture.
//!
//! Copyright (C) 2026 enastvo. Licensed under the GNU GPL v3.0 or later; see
//! `LICENSE`.

pub mod types;
pub mod context;
pub mod store;
pub mod personalities;
pub mod personalities_watch;
pub mod repl;
pub mod settings;
pub mod llm;
pub mod signal;
pub mod router;
pub mod orchestrator;
pub mod config;
pub mod web;
pub mod metrics;
pub mod summarizer;
pub mod search;
pub mod tools;

# toxic-bot (`signal-bot`)

A self-hosted Signal chat bot written in Rust. It runs a local LLM through
[Ollama](https://ollama.com/) (`qwen3:8b` by default), gives each chat room
its own personality and reply mode, and comes with a LAN-only HTTPS dashboard
for administration.

**Author:** [enastvo](https://github.com/enastvo)
**License:** [GNU GPL v3.0 or later](LICENSE)

> **New install?** Follow **[INSTALLATION.md](INSTALLATION.md)**. It is a
> step-by-step guide covering the accounts you need (a Google Voice number for
> Signal, an optional Tavily API key), host prep, build, Signal registration
> and first login.

---

## Contents

- [Features](#features)
- [How it works](#how-it-works)
- [Recommended specs](#recommended-specs)
- [Project layout](#project-layout)
- [Build](#build)
- [Deploy & setup](#deploy--setup)
- [Register the bot's Signal account](#register-the-bots-signal-account)
- [Configuration](#configuration)
- [Web dashboard](#web-dashboard)
- [Tools and web search](#tools-and-web-search)
- [Personalities](#personalities)
- [Command-line flags](#command-line-flags)
- [REPL (testing without Signal)](#repl-testing-without-signal)
- [On-host smoke test](#on-host-smoke-test)
- [Development](#development)
- [Security notes](#security-notes)
- [Troubleshooting](#troubleshooting)
- [Author](#author)
- [License](#license)

## Features

- **Local inference only.** Replies come from an Ollama model bound to
  `127.0.0.1`. No chat content goes to a cloud LLM.
- **Per-room personalities.** Personalities are TOML files, hot-reloaded when
  files change or on `SIGHUP`.
- **Three reply modes per room:** `addressed`, `always` and `proactive`.
  Proactive mode uses a relevance gate plus per-room rate limits.
- **Burst coalescing.** Each room has its own actor, which merges rapid-fire
  messages into one turn. A single global inference permit keeps the model
  from being oversubscribed.
- **Rolling room summaries.** A background sweep folds older history into a
  short per-room summary, so long conversations keep their context.
- **Optional LLM tools:** a calculator, the current time, search over the
  room's own history, and web search limited to a domain whitelist (via
  Tavily).
- **Admin dashboard** over TLS: rooms, live message view (SSE), runtime
  settings, and a health/metrics page.

## How it works

```
Signal ⇄ signal-cli (JSON-RPC daemon, UNIX socket)
            │
            ▼
      SignalCli ──► Dispatcher ──► RoomActor (one per room, coalesces bursts)
                                     │  global inference permit
                                     ▼
                                   Router ──► decide(): reply / proactive / silent
                                     │         context window + room summary
                                     │         optional tool loop (tools.rs / search.rs)
                                     ▼
                                   Ollama (/api/chat, 127.0.0.1:11434)
                                     │
                                     ▼
                           SQLite store (rooms, messages, settings, summaries)
                                     │
                                     ▼
                       axum HTTPS dashboard (sessions, SSE, /api/metrics)
```

1. `signal-cli` runs as a supervised child process in daemon mode. It is
   restarted with capped exponential backoff if it dies.
2. Each incoming data message is parsed (`signal.rs`) and handed to the
   `Dispatcher` (`orchestrator.rs`), which forwards it to that room's actor.
3. The actor drains everything already queued, dedupes it, takes the global
   inference permit, and calls `Router::handle_burst` (`router.rs`).
4. The router records the messages and decides whether to reply. It builds a
   speaker-labelled context window (`context.rs`) plus the room summary, then
   generates a reply (optionally through the tool loop) and sends it back
   through `signal-cli`.

## Recommended specs

The bot process itself is lightweight; resource needs are driven almost entirely
by the local LLM (Ollama running `qwen3:8b`). The values below match the
reference deployment and give comfortable headroom.

| Resource | Recommended | Notes |
|----------|-------------|-------|
| CPU | 6+ x86-64 cores | Inference runs on CPU by default; more cores means faster replies. Reference host: Intel Core i9-12900HK |
| RAM | 16 GB | `qwen3:8b` (Q4_K_M) holds ~7 GB resident; the rest covers the OS, signal-cli's JVM, and the bot |
| Disk | 40 GB+ free | OS plus the Ollama model (~5 GB), a JRE, and a slowly growing SQLite database |
| GPU | none required | Inference is CPU-only out of the box. A CUDA/ROCm GPU is optional and makes replies dramatically faster |
| OS | 64-bit Linux with systemd | Tested on Ubuntu 24.04 / 26.04 LTS. Needs a JRE for signal-cli and UNIX-socket support |

On CPU-only hosts, reply latency scales with core count and with model and
prompt size. If replies feel slow, prefer leaner personalities (shorter
`system_prompt`s), lower `num_predict`, or a smaller model before reaching for
more hardware.

## Project layout

| Path | Purpose |
|------|---------|
| `src/main.rs` | Wiring: config, store, web server, signal-cli, dispatcher, summarizer |
| `src/config.rs` | CLI flags and `config.toml` schema (including settings seed defaults) |
| `src/signal.rs` | signal-cli daemon supervisor, JSON-RPC envelope parsing, send |
| `src/orchestrator.rs` | Per-room actors, burst coalescing, global inference permit |
| `src/router.rs` | Reply decision, prompt composition, tool loop, proactive rate limiting |
| `src/context.rs` | Context-window assembly and speaker labelling |
| `src/llm.rs` | Ollama client (`/api/chat`, `/api/ps`), relevance check, summarization |
| `src/tools.rs` | Tool schemas and execution (calculator, time, room search, web search) |
| `src/search.rs` | Whitelist-restricted Tavily web search provider |
| `src/summarizer.rs` | Periodic per-room summary sweep |
| `src/personalities.rs`, `src/personalities_watch.rs` | Personality loading and hot reload |
| `src/settings.rs` | Resolving effective generation params, settings validation |
| `src/store.rs` | SQLite persistence (sqlx) and admin credential hashing (argon2) |
| `src/metrics.rs` | Turn metrics ring and system snapshot for the health page |
| `src/web/` | axum dashboard: auth, handlers, SSE |
| `templates/` | Askama HTML templates for the dashboard |
| `migrations/` | SQLite schema migrations (applied automatically at startup) |
| `personalities/` | Bundled personality TOML files |
| `deploy/` | Setup script, systemd unit, TLS cert helper, example config, registration guide |
| `INSTALLATION.md` | Step-by-step install guide, including account prerequisites |

## Build

```bash
cargo build --release
```

The binary is written to `target/release/signal-bot`. The bot targets Linux:
it uses UNIX sockets to talk to signal-cli and systemd for supervision.

## Deploy & setup

A summary is below. For the full walkthrough, including account setup, see
[INSTALLATION.md](INSTALLATION.md).

Run the deployment script from the repo root as a user with `sudo`:

```bash
./deploy/setup.sh
```

The script:

1. **Installs dependencies:** a Java runtime (`default-jre`) for signal-cli,
   Ollama, and the `qwen3:8b` model.
2. **Creates the service user:** an unprivileged `signal-bot` system user,
   plus `/var/lib/signal-bot` (mode 0700) for data.
3. **Installs config, personalities and binary:**
   - `deploy/config.example.toml` → `/etc/signal-bot/config.toml`
   - `personalities/*.toml` → `/etc/signal-bot/personalities/`
   - the binary → `/opt/signal-bot/signal-bot`
4. **Generates a TLS certificate** with `deploy/gen-cert.sh`. It uses
   `mkcert` if available and otherwise falls back to a self-signed OpenSSL
   certificate for `bot.local`.
5. **Sets the web admin credential.** It prompts for a username and password,
   passes the password via the `SIGNAL_BOT_ADMIN_PASSWORD` environment variable
   (never argv), and runs `--set-admin`, which stores an argon2 hash.
6. **Installs the systemd unit** from `deploy/signal-bot.service`.

> `setup.sh` installs the example config only if `/etc/signal-bot/config.toml`
> doesn't exist yet, so re-running it keeps your edited config. The bundled
> personality files *are* re-copied, so keep custom personas under their own
> file names.

After setup, **edit `/etc/signal-bot/config.toml`** and set `signal_account`
to the bot's real number. The example ships with a fictional placeholder.

Complete Signal registration (next section) *before* starting the service.

## Register the bot's Signal account

See [`deploy/REGISTER.md`](deploy/REGISTER.md) for the full steps:

1. Install `signal-cli` manually (a pinned release with a verified checksum).
2. Register the bot's number (`<BOT_NUMBER>`, the same value as
   `signal_account`).
3. Verify with the SMS or voice code you receive.
4. Smoke test by sending yourself a message.

Then start the service:

```bash
sudo systemctl enable --now signal-bot
```

## Configuration

The main config file is `/etc/signal-bot/config.toml` (or pass another path
with `--config`). [`deploy/config.example.toml`](deploy/config.example.toml)
is the template.

| Key | Description |
|-----|-------------|
| `data_dir` | Where the SQLite database and the signal-cli socket and data live |
| `personalities_dir` | Directory of personality `.toml` files |
| `signal_bin` | Path to the `signal-cli` binary |
| `signal_account` | The bot's own Signal number (E.164, e.g. `+15555550100`) |
| `ollama_url` | Ollama API base URL (normally `http://127.0.0.1:11434`) |
| `bind_addr` | Dashboard listen address (default in the example: `0.0.0.0:8443`) |
| `cert_path`, `key_path` | TLS certificate and private key (PEM) |
| `debug`, `dry_run` | Can also be set with CLI flags |
| `search_api_key` | **Secret.** Tavily API key. Web search is unavailable without it |

The example config also lists optional **seed values** for the runtime
settings (`keep_alive`, `num_predict`, `num_ctx`, `tools_enabled`,
`search_whitelist`, and so on). They are written to the database only when it
is first created. After that, the **Settings** page in the dashboard is the
source of truth.

Never commit a real config. `.gitignore` already excludes `config.local.toml`,
`*.key`, `*.pem`, `*.crt`, `.env`, databases and signal-cli data.

## Web dashboard

Open `https://bot.local:8443` (or the host's LAN address) and trust the local
TLS certificate. For a self-signed cert, import it on your viewing device.

- **Rooms:** pick a room, then set its **personality** and **reply mode**:
  - **addressed:** reply only when the bot is @-mentioned or quoted
  - **always:** reply to every message
  - **proactive:** always reply when addressed. Otherwise a relevance check
    decides whether to chime in, subject to the personality's cooldown and
    hourly cap.
  - Direct messages always get a reply, whatever the mode.
- **Web sources:** extra search domains for the room's current persona. They
  apply to that persona in every room.
- **Settings:** generation parameters, summaries, tools, web-search toggle and
  the global domain whitelist. Changes are validated and saved to the DB.
- **Health:** uptime, reply/error/timeout counts, latency percentiles, RAM,
  loaded Ollama models, and per-room stats. Data comes from `/api/metrics`.

### Network exposure / firewall

`bind_addr = "0.0.0.0:8443"` listens on **all** interfaces. Firewall port
8443 so only your LAN can reach it (for example, a `ufw` rule scoped to your
LAN subnet), or bind to a specific LAN IP. Never port-forward the dashboard
to the internet.

## Tools and web search

When **tools** are enabled in Settings, the model can call:

| Tool | What it does |
|------|--------------|
| `calculator` | Safe arithmetic evaluator (`+ - * /`, parentheses, decimals). No code execution |
| `current_time` | Current UTC date and time |
| `room_search` | Keyword search over the **current room's** stored messages only |
| `web_search` | Tavily search restricted to whitelisted domains (needs `search_api_key` and the web-search toggle) |

Tool calls are capped at `max_tool_rounds` per turn. Tool output goes back to
the model as data, and web results are treated as untrusted. The effective
search whitelist is the global whitelist, plus the persona's
`extra_search_domains` from its TOML, plus any dashboard-configured sources
for that persona.

## Personalities

Personalities are TOML files in the personalities directory:
`/etc/signal-bot/personalities/` in production, `./personalities/` for
development. The file name (without `.toml`) is the personality's ID.

A personality named **`default` must exist**. It is the fallback, and the bot
refuses to start or reload without it. Files that fail to parse are skipped
with a warning.

| Field | Required | Notes |
|-------|----------|-------|
| `label` | yes | Display name; also used as the bot's sender name in history |
| `description` | no | Short description |
| `system_prompt` | yes | Persona instructions. Shared "house rules" are prepended automatically |
| `model` | no | Ollama model (default `qwen3:8b`) |
| `[proactive]` | yes | `relevance_threshold` (0–1), `cooldown_secs`, `max_per_hour` |
| `temperature`, `top_p` | no | Persona sampling values. Used when set; otherwise the global Settings values apply |
| `temperature_override`, `top_p_override` | no | Take precedence over `temperature`/`top_p` if both are set |
| `num_ctx_override`, `num_predict`, `repeat_penalty` | no | Per-persona overrides of the global Settings values |
| `extra_search_domains` | no | Web-search domains only this persona may use |
| `num_ctx` | no | Legacy field, ignored for generation. Use `num_ctx_override` |

Personalities hot-reload when files change (via `notify`) or on `SIGHUP`, so
no restart is needed.

Example (`personalities/default.toml`):

```toml
label = "Default"
description = "Neutral, helpful assistant."
system_prompt = """
You are a helpful assistant in a Signal chat. Keep replies concise and friendly.
"""
model = "qwen3:8b"

[proactive]
relevance_threshold = 0.7
cooldown_secs = 120
max_per_hour = 6
```

## Command-line flags

| Flag | Description |
|------|-------------|
| `--config <PATH>` | Config file (default `/etc/signal-bot/config.toml`) |
| `--debug` | Debug-level logging (also `SIGNAL_BOT_DEBUG`; `RUST_LOG` overrides both) |
| `--dry-run` | Generate replies but don't send them; log what would be sent |
| `--repl` | Interactive REPL without Signal (see below) |
| `--set-admin <USERNAME>` | Set the dashboard admin; password comes from `SIGNAL_BOT_ADMIN_PASSWORD` |

## REPL (testing without Signal)

```bash
cargo run --release -- --repl
```

Input format, one message per line:

```
<room_id>|<is_group>|<is_mention>|<sender_id>|<text>
```

- `room_id`: any string
- `is_group`: `1` for group chats, `0` for DMs
- `is_mention`: `1` if the message mentions the bot
- `sender_id`: the sender's identifier
- `text`: the message body

Example:

```
room-123|1|0|alice|what time is it?
```

Output is `BOT> <reply>`, or `[silent]` when the bot stays quiet.

## On-host smoke test

1. **Model present:** `ollama list | grep qwen3:8b`
2. **REPL:**
   ```bash
   sudo -u signal-bot /opt/signal-bot/signal-bot --config /etc/signal-bot/config.toml --repl --debug
   ```
   Type `+me|0|0|Me|hello, bot` and expect a `BOT>` reply. Press Ctrl-D to exit.
3. **DM:** send a DM to the bot's number. Check that it appears in the
   dashboard and gets a reply (`sudo journalctl -u signal-bot -f`).
4. **Group, addressed mode:** add the bot to a group, set the mode to
   `addressed`, then @-mention it.
5. **Group, proactive mode:** switch to `proactive` and chat normally. If it
   stays quiet, the logs show relevance and rate-limit decisions.

## Development

```bash
cargo test                    # unit + integration tests (tests/)
cargo build                   # debug build
cargo clippy -- -D warnings   # lint
cargo fmt                     # format
```

Integration tests use in-memory SQLite and mock LLM, Signal and search
backends, so they need neither Ollama nor Signal.

## Security notes

- **Secrets live only in `config.toml`** (root-owned, mode `0640`, group
  `signal-bot`): the Tavily key and nothing else. The admin password is stored
  only as an argon2 hash in SQLite. There are no credentials in this
  repository.
- The dashboard uses `Secure`, `HttpOnly`, `SameSite=Strict` session cookies,
  a 12-hour inactivity expiry, and a per-IP login rate limit.
- The systemd unit runs as an unprivileged user with `ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, `NoNewPrivileges`, and no capabilities.
- Ollama and the signal-cli socket are local-only. The dashboard is the only
  network listener.
- Web search goes through Tavily's API. The bot never fetches arbitrary URLs
  itself, which removes the SSRF risk.

## Troubleshooting

- **Ollama not responding:** make sure `ollama serve` is running and the
  model is pulled (`ollama list`).
- **Signal messages not appearing:** check `sudo journalctl -u signal-bot -f`.
  Make sure signal-cli is registered and `signal_account` matches.
- **Dashboard unreachable:** check `sudo systemctl status signal-bot`, the
  firewall, and that your device trusts the certificate.
- **Personality not updating:** the TOML must parse. Parse errors are logged
  and the file is skipped. `default.toml` must exist.

## Author

**enastvo**: <https://github.com/enastvo>

## License

Copyright (C) 2026 enastvo

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.

This program is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with
this program. See [`LICENSE`](LICENSE), or <https://www.gnu.org/licenses/>.

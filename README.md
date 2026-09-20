# signal-bot

A Rust-based Signal chat bot that runs on your local network. It uses [Ollama](https://ollama.ai/) to run a local LLM (`qwen3:8b`), supports per-room personalities with configurable reply modes (addressed, always, or proactive), and provides a LAN-only TLS web dashboard for administration.

## Build

```bash
cargo build --release
```

The binary lands at `target/release/signal-bot`.

## Deploy & Setup

Run the deployment script as the `bot` user (with `sudo`):

```bash
./deploy/setup.sh
```

This script performs the following setup steps:

1. **Install dependencies**
   - Java runtime (default-jre) for signal-cli
   - Ollama
   - Pull the `qwen3:8b` model

2. **Create service user**
   - Creates the unprivileged `signal-bot` system user
   - Creates `/var/lib/signal-bot` with 0700 permissions (config/data dir)

3. **Install config, personalities, and binary**
   - Copies `deploy/config.example.toml` to `/etc/signal-bot/config.toml`
   - Copies personality TOML files to `/etc/signal-bot/personalities/`
   - Installs the binary to `/opt/signal-bot/signal-bot`

4. **Generate TLS certificate**
   - Runs `deploy/gen-cert.sh` to create or import a bot.local TLS cert
   - Uses `mkcert` if available (developer-friendly); falls back to self-signed OpenSSL

5. **Set web admin credential**
   - Prompts for username and password
   - The password is passed via the `SIGNAL_BOT_ADMIN_PASSWORD` environment variable (not argv, to avoid credential exposure in `/proc/cmdline`)
   - Runs `--set-admin <username>` to hash and store the credential

6. **Install systemd unit**
   - Copies `deploy/signal-bot.service` to `/etc/systemd/system/`
   - Reloads systemd daemon

⚠️ **Signal registration** must be completed *before* starting the service. See the next section.

## Register the Bot's Signal Account

Before starting the service, register the bot's Signal account. See `deploy/REGISTER.md` for detailed steps:

1. Install `signal-cli` manually (pinned release, verified checksum)
2. Register the account with the bot's phone number (`+14433996053` by default)
3. Verify with the SMS/voice code you receive
4. Smoke test by sending yourself a message

After registration, start the service:

```bash
sudo systemctl enable --now signal-bot
```

## Web Dashboard

Open `https://bot.local:8443` on the LAN. You will need to trust the local TLS certificate (if self-signed, import the CA on your viewing device).

### Network exposure / firewall

By default `bind_addr` is `0.0.0.0:8443`, which listens on **all** network interfaces, not just the LAN — this is intentional since a home box's LAN IP is often DHCP-assigned and can't always be pinned down in config. This means **you must firewall port 8443 to LAN-only access** (e.g. a `ufw`/`iptables` rule scoped to your LAN subnet), otherwise the dashboard is reachable from the WAN if the host is ever exposed (port-forwarded, on a public interface, etc.). Alternatively, set `bind_addr` in `config.toml` to a specific LAN IP instead of `0.0.0.0` if your box has a static/reserved LAN address.

1. Log in with the admin username and password set during setup
2. Select a room from the dropdown
3. Assign a **personality** (e.g., "Default", "Sage")
4. Assign a **reply mode**:
   - **addressed**: bot replies only when mentioned or addressed directly
   - **always**: bot replies to all messages in the room
   - **proactive**: bot sends unprompted messages based on conversation relevance and rate limits

## Command-Line Flags

When running the binary directly, the following flags are available:

- `--config <PATH>`: Path to the config file (default: `/etc/signal-bot/config.toml`)
- `--debug`: Enable verbose trace-level logging (also settable via `SIGNAL_BOT_DEBUG` env var)
- `--dry-run`: Parse messages and generate replies, but do not send them; log what would be sent instead
- `--repl`: Start an interactive REPL for testing without Signal (see below)
- `--set-admin <USERNAME>`: Bootstrap the web admin credential (password via `SIGNAL_BOT_ADMIN_PASSWORD` env var)

## REPL (Testing without Signal)

Test the reply pipeline without a running Signal account:

```bash
cargo run --release -- --repl
```

The REPL accepts lines with the format:

```
<room_id>|<is_group>|<is_mention>|<sender_id>|<text>
```

Where:
- `room_id`: identifier for the room (any string)
- `is_group`: `1` for group chats, `0` for DMs
- `is_mention`: `1` if the message mentions the bot, `0` otherwise
- `sender_id`: the sender's identifier
- `text`: the message body

Example:

```
room-123|1|0|alice|what time is it?
```

Output will show `BOT> <reply>` for replies, or `[silent]` if the bot chooses not to respond.

## Adding Personalities

Personalities are TOML files stored in the personalities directory (e.g., `/etc/signal-bot/personalities/` in production, or `./personalities/` for development).

Each personality file must have:
- `label`: display name
- `description`: short description
- `system_prompt`: the system prompt (instructions for the LLM)
- `model`: the Ollama model to use (e.g., `qwen3:8b`)
- `temperature`, `top_p`, `num_ctx`: LLM parameters
- `[proactive]` section (optional) with:
  - `relevance_threshold`: score 0.0–1.0 for relevance checks
  - `cooldown_secs`: seconds between proactive messages
  - `max_per_hour`: maximum proactive messages per hour

**Important**: A personality named `default` must always exist (it is loaded as a fallback).

Personalities are hot-reloaded when files change (via `notify` and `SIGHUP`); no bot restart needed.

Example personality (see `personalities/default.toml`):

```toml
label = "Default"
description = "Neutral, helpful assistant."
system_prompt = """
You are a helpful assistant in a Signal chat. Keep replies concise and friendly.
"""
model = "qwen3:8b"
temperature = 0.6
top_p = 0.9
num_ctx = 8192

[proactive]
relevance_threshold = 0.7
cooldown_secs = 120
max_per_hour = 6
```

## On-Host Smoke Test

Once the bot is deployed and the service is running, perform these checks on bot.local:

1. **Verify Ollama has the model**
   ```bash
   ollama list | grep qwen3:8b
   ```
   Confirm `qwen3:8b` is listed and available.

2. **Test the REPL**
   ```bash
   sudo -u signal-bot /opt/signal-bot/signal-bot --config /etc/signal-bot/config.toml --repl --debug
   ```
   Type a test message (e.g., `+me|0|0|Me|hello, bot`). You should see a `BOT>` reply. Press Ctrl-D to exit.
   
   (On a dev machine with source present, use `cargo run --release -- --repl` instead.)

3. **Send a DM to the bot**
   - From your Signal account, send a DM to the bot's number (`+14433996053` by default)
   - Open the web dashboard and verify the message appears and a reply is sent
   - Check the systemd logs: `sudo journalctl -u signal-bot -f` to see activity

4. **Test a group chat (addressed mode)**
   - Add the bot to a Signal group
   - Via the web dashboard, assign a personality and set reply mode to **addressed**
   - Type a message mentioning the bot (e.g., `@bot what is 2+2?`)
   - Verify the bot replies

5. **Test a group chat (proactive mode)**
   - Change the reply mode to **proactive**
   - Have a natural conversation in the group
   - After a minute or two, the bot may contribute unprompted if the conversation is relevant
   - If nothing happens, check the logs (`sudo journalctl -u signal-bot -f`) for rate-limit or relevance decisions

## Development

Run tests:

```bash
cargo test
```

Build without optimizations:

```bash
cargo build
```

Lint and format:

```bash
cargo clippy -- -D warnings
cargo fmt
```

## Configuration

The main configuration file is `/etc/signal-bot/config.toml` (or elsewhere via `--config`). See `deploy/config.example.toml` for a template.

Key fields:
- `data_dir`: where the SQLite database and signal-cli socket live
- `personalities_dir`: where personality TOML files are read from
- `signal_bin`: path to the `signal-cli` binary
- `signal_account`: the bot's Signal phone number
- `ollama_url`: URL to the Ollama API (default: `http://127.0.0.1:11434`)
- `bind_addr`: web server bind address (default: `0.0.0.0:8443`)
- `cert_path`, `key_path`: TLS certificate and key
- `debug`, `dry_run`: can be overridden by CLI flags

## Troubleshooting

- **Ollama not responding**: Confirm ollama is running (`ollama serve`) and the model is available (`ollama list`)
- **Signal messages not appearing**: Check `sudo journalctl -u signal-bot -f` for errors. Ensure signal-cli is registered and the bot has access to its config dir.
- **Web dashboard not accessible**: Verify the service is running (`sudo systemctl status signal-bot`), and check the TLS certificate is trusted on your viewing device.
- **Personality not updating**: Ensure the TOML file is valid and the personalities directory has the correct permissions.

## License

(TBD)

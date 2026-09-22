# HANDOFF — signal-bot conversation-quality / orchestration / observability

**For:** the next agent executing this work. **Date:** 2026-09-20. **Repo:** `~/lab/bot` (branch `master`).

## Your mission

Execute the implementation plan **via the `superpowers:subagent-driven-development` skill** (fresh implementer subagent per task, two-stage review between tasks, whole-branch review at the end). Set up an isolated git worktree first (the SDD skill calls `superpowers:using-git-worktrees`; the native `EnterWorktree` tool failed last time because the repo was pre-init — use the **git worktree fallback**: `git worktree add .worktrees/<name> -b <name>`; `.worktrees/` is already git-ignored).

**Authoritative documents (read both before starting):**
- Spec: `docs/superpowers/specs/2026-09-20-signal-bot-conversation-quality-design.md`
- Plan: `docs/superpowers/plans/2026-09-20-signal-bot-conversation-quality.md` (22 tasks, 3 phases)

## Phase boundary (important)

Execute **Phase 1 (Tasks 1–14) first, then STOP** and hand back for deploy + live group test on "Bit Blasters" before doing Phases 2–3. Phase 1 contains all the behavior fixes; real-group feedback should inform the dashboard/summary work. Do **not** run all three phases straight through without the operator's go-ahead after Phase 1.

## Decisions already made (do not re-litigate — from the brainstorming session)

1. **Burst handling:** coalesce into ONE reply per room (one in-flight generation per room).
2. **Global inference:** at most one qwen3 generation process-wide (global `Semaphore(1)`) — the 7 GB CPU box can't run concurrent generations.
3. **Context strategy:** trimmed window (~15–20 msgs) with the bot's OWN replies capped at the last ~2; explicit `[Name]:` speaker labels; repeat penalty.
4. **Summarization:** IN scope (Phase 3) — per-room, activity-gated, default 6 h, injected as a long-term context layer.
5. **Model:** stay on `qwen3:8b`; get speed from `keep_alive` + single-flight + `num_predict` cap + thinking off. No model swap.
6. **Settings:** DB-backed (`settings` table, single row), web-editable (Phase 2 options screen with plain-language help tooltips); `config.toml` seeds it; per-personality TOML overrides on top.
7. **Personality prompts:** split shared "house rules" (in code) from per-personality "character" (TOML).
8. `handle_burst(Vec<IncomingMessage>, wait_ms: u64)` is the seam between orchestrator and router — keep that signature stable.

## Current code state

- On `master`, **clippy-clean** (`cargo clippy --all-targets -- -D warnings`), **37 tests green**.
- Relevant recent commits: `e598eac` (think:false + proactive-replies-when-addressed), `74849db` (rustls CryptoProvider fix so TLS binds), `bf73452` (self-sender guard, firewall docs, setup.sh preserve-env).
- Modules today: `types, window, store, personalities, personalities_watch, llm, signal, router, repl, config, web/{mod,auth,handlers}`; entry `main.rs`. New modules the plan adds: `orchestrator, context, settings, metrics, summarizer`.
- Key current signatures you'll change: `LlmBackend::generate_reply(&self, ChatRequest) -> Result<String>` (→ `(String, GenStats)`); `router::{handle, decide, build_turns, system_prompt}`; `main.rs` receive loop spawns one task per message (→ Dispatcher).

## Deployment (bare-metal `bot@bot.local`, IP <bot-host-ip>)

- SSH as `bot` works over the operator's key (no password for SSH).
- **You have SCOPED passwordless sudo** on the VM (`/etc/sudoers.d/signal-bot-admin`): run `signal-cli` as the `signal-bot` user, and `systemctl {stop,start,restart,kill -s HUP,--no-pager status,is-active} signal-bot`, and `journalctl -u signal-bot --no-pager`. **You do NOT have** `sudo cp/install/tee` to `/opt` or `/etc`.
- **Build & deploy pattern:** `cargo build --release` locally → `rsync` the binary + changed sources to `bot@bot.local:~/signal-bot/` → **the operator runs** `sudo install -m755 ~/signal-bot/target/release/signal-bot /opt/signal-bot/signal-bot` (needs their sudo — you can't) → **you** `sudo systemctl restart signal-bot`. Migrations run automatically on connect (sqlx `migrate!`) — the new `0002_settings.sql` / `0003_room_summaries.sql` apply on restart.
- Config `/etc/signal-bot/config.toml` (root:signal-bot 0640) — operator edits it if new seed keys are needed; settings otherwise live in the DB.
- Dashboard: `https://<bot-host-ip>:8443` (self-signed, argon2 admin login). Binds `0.0.0.0`.

## Hard stops — hand back to the operator, do NOT do these yourself

- The `/opt` binary install and any `/etc` writes (need the operator's sudo).
- Any Signal account mutation: `register`, `verify`, `unregister`, `deleteLocalAccountData`, changing the number, deleting `/var/lib/signal-bot`. The account `+15555550100` is registered to **signal-cli as PRIMARY** — re-registering elsewhere breaks the bot. Never do destructive signal-cli.
- Merging to `master` / finishing the branch is the operator's decision (SDD ends with `finishing-a-development-branch`, which presents options).

## Gotchas / context you must know

- **The daemon holds an exclusive lock** on `/var/lib/signal-bot`. Running `signal-cli --config /var/lib/signal-bot …` while the service is up fails — `sudo systemctl stop signal-bot` first, then restart after.
- **`qwen3:8b` is a thinking model.** All Ollama calls must keep `think: false` (top-level in the `/api/chat` body). The `/no_think` leak the tester saw was the model emitting the token, it getting stored as an assistant message, and the echo-loop nesting it — the plan fixes this with `sanitize()` **before storing** + capped self-history + no-whole-message-quoting. Do not "fix" it by string-replacing user input.
- **"Rooms not replying while 1:1 works"** is a timeout/orchestration issue (large group prompts exceeding the 120 s client timeout, plus racing), NOT a token limit. Fix = configurable timeout (default 300) + trimmed/faster prompts + single-flight. Do not raise `num_ctx` to "fix" it.
- Metrics are **in-memory** (reset on restart) in v1 — deliberate.
- `Bit Blasters` group id: `HDMGJl7+5XTAuOIK0mVuKtQcygVkOXyJVH7BNbDZARg=`. Signal profile name is `toxic-trash`.

## Model selection for subagents (SDD)

- Pure-logic tasks (sanitize, context trim, settings, EffectiveParams, metrics aggregates, `/proc` parsers): cheapest tier (transcription + tests).
- Integration/judgment tasks — **Task 10 (router refactor)**, **Task 11 (orchestrator)**, **Task 13 (main wiring)**, **Task 17 (health JSON/gauges)**: standard tier.
- Reviewers: mid tier, scaled to diff size; the orchestrator/router diffs deserve careful review (concurrency, lock-across-await, coalescing correctness).
- Enforce **real captured RED** in every implementer report (earlier runs sometimes inferred it). Keep the tree clippy-clean under `-D warnings` every commit.

## Phase-1 live-test acceptance (after deploy)

In Bit Blasters (personality `toxic`): send a burst of several messages → **exactly one** coalesced reply, no duplicates; **no `/no_think`** in output; speakers attributed to the right person; a **factual question gets answered** (persona is flavor, not a substitute); replies vary (no identical template); `journalctl` shows one generation at a time. Then proceed to Phases 2–3 on the operator's go-ahead.

## Memory

Project memory lives at `~/.claude/projects/<project>/memory/` (see `signal-bot-deployment.md`) — deployment facts, the scoped-sudo grant, the ACI/PNI note, and the `/no_think` root cause are recorded there. Update it as state changes.

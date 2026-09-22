# Installation

This guide takes you from nothing to a running bot. It covers:
- the outside accounts the bot needs (a phone number for Signal, and optionally a Tavily API key),
- preparing the host,
- building and installing the software,
- registering the bot with Signal,
- first login.

For architecture, configuration reference and day-to-day operation, see
[README.md](README.md).

---

## Contents

1. [What you need](#1-what-you-need)
2. [Account dependencies](#2-account-dependencies)
   - [A phone number for the bot (Google Voice)](#21-a-phone-number-for-the-bot-google-voice)
   - [Tavily API key (optional, for web search)](#22-tavily-api-key-optional-for-web-search)
3. [Prepare the host](#3-prepare-the-host)
4. [Build the bot](#4-build-the-bot)
5. [Run the setup script](#5-run-the-setup-script)
6. [Install signal-cli](#6-install-signal-cli)
7. [Register the bot's Signal account](#7-register-the-bots-signal-account)
8. [Edit the config](#8-edit-the-config)
9. [Start the service](#9-start-the-service)
10. [First login and room setup](#10-first-login-and-room-setup)
11. [Enable web search (optional)](#11-enable-web-search-optional)
12. [Updating](#12-updating)
13. [Troubleshooting](#13-troubleshooting)

---

## 1. What you need

| Item | Notes |
|------|-------|
| **Linux host** | A Debian/Ubuntu machine on your home network, running 24/7. The bot uses UNIX sockets and systemd, so Windows and macOS aren't supported as hosts. |
| **RAM** | 16 GB recommended. The default model `qwen3:8b` needs about 6 GB when loaded, plus the JVM for signal-cli and the OS. |
| **Disk** | About 10 GB free (model ~5 GB, Rust build cache, signal-cli). |
| **CPU / GPU** | Works on CPU only; replies take several seconds to tens of seconds. An NVIDIA or AMD GPU supported by Ollama makes it much faster. |
| **A phone number for the bot** | Must be able to receive an SMS or voice call once for verification, and must not already be used by a Signal account. See [2.1](#21-a-phone-number-for-the-bot-google-voice). |
| **Your own Signal account** | On your phone, to talk to the bot and add it to groups. |
| **Tavily API key** | *Optional.* Only needed for the web-search tool. See [2.2](#22-tavily-api-key-optional-for-web-search). |

---

## 2. Account dependencies

### 2.1 A phone number for the bot (Google Voice)

The bot is a real Signal account, so it needs its own phone number. Signal
sends a verification code to that number **once**, at registration. After
that the bot runs from `signal-cli` on your server and never needs the number
again, *unless* the account is re-registered (see the warnings below).

A free **Google Voice** number works well in the US.

**Setting up a Google Voice number:**

1. Use a Google account that is **not** your main one; a dedicated account
   for the bot is cleanest.
2. Go to <https://voice.google.com> and choose **Get Google Voice**, then
   **For personal use**.
3. Pick a number. Google Voice requires you to verify an existing **US** mobile
   or landline number that isn't already linked to another Google Voice
   account. This is only for ownership verification; the bot never uses it.
4. In Google Voice **Settings → Messages**, make sure text messages are
   enabled, so the SMS code arrives in the Google Voice web app.

**Keep the number alive.** Google reclaims free Voice numbers that go unused
(roughly three months without any activity). If the number is lost and the
bot's account ever has to re-register, someone else may own the number and
receive your code. To prevent this:
- Send or receive a text in the Google Voice app every month or two. A
  calendar reminder is enough.
- Set a Signal **registration lock PIN** after registering (see
  [step 7](#7-register-the-bots-signal-account)).

**If Signal won't send an SMS to the number.** Signal sometimes refuses or
rate-limits VoIP numbers.
- Retry the registration with the `--voice` option. Google Voice receives the
  call and transcribes it to voicemail, and the code is in the voicemail text.
- Otherwise, use a cheap prepaid SIM, or another number that receives SMS.
  Any number works as long as it can receive the code once and isn't
  registered with Signal elsewhere.

> **Don't use your own personal number.** Registering it with `signal-cli`
> signs your phone's Signal app out of that number.

### 2.2 Tavily API key (optional, for web search)

The bot can search a whitelist of trusted websites, when you enable it,
through [Tavily](https://tavily.com), an LLM-oriented search API. Without a
key everything else works; the `web_search` tool just isn't offered to the
model.

**Getting a key:**

1. Sign up at <https://app.tavily.com>.
2. Copy the API key from the dashboard. It starts with `tvly-`.
3. Keep it secret. It goes **only** in `/etc/signal-bot/config.toml`, never in
   the repo, the database or the dashboard (see [step 11](#11-enable-web-search-optional)).

**Cost:** Tavily's free tier includes a monthly allowance of API credits. The
bot always uses **advanced** search depth, which costs more credits per search
than basic. A busy group with web search on can use up a free allowance, so
check your usage in the Tavily dashboard and the current pricing on their site.

**Rotating the key:** create a new key in the Tavily dashboard, replace it in
`config.toml`, restart the service (`sudo systemctl restart signal-bot`), then
revoke the old key.

---

## 3. Prepare the host

Everything below runs on the Linux host, as a normal user with `sudo`.

**Base packages:**

```bash
sudo apt update
sudo apt install -y git curl build-essential pkg-config cmake
```

**Java 21+** (signal-cli needs it):

```bash
java -version
```

If that shows a version below 21, or no Java at all, install 21:

```bash
sudo apt install -y openjdk-21-jre-headless
```

On Debian 12 the default `default-jre` is Java 17, which is too old. Ubuntu
24.04 ships 21. Alternatively, use signal-cli's **native** Linux build (see
[step 6](#6-install-signal-cli)), which needs no Java at all.

**Rust toolchain** (to build the bot):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

**Hostname `bot.local` (optional).** The dashboard's certificate is issued
for `bot.local`. To reach the dashboard by that name, set the host's name to
`bot` and enable mDNS:

```bash
sudo hostnamectl set-hostname bot
sudo apt install -y avahi-daemon
```

Otherwise, use the host's LAN IP and accept the certificate warning.

**Firewall.** The dashboard listens on port 8443 on all interfaces by
default. Allow it only from your LAN. For example, if your LAN is
`192.168.1.0/24`:

```bash
sudo ufw allow from 192.168.1.0/24 to any port 8443 proto tcp
sudo ufw enable
```

Don't port-forward 8443 on your router.

---

## 4. Build the bot

```bash
git clone git@github.com:enastvo/toxic-bot.git
cd toxic-bot
cargo build --release
```

The first build takes a few minutes. The result is
`target/release/signal-bot`. Optionally, run the test suite, which needs
neither Signal nor Ollama:

```bash
cargo test
```

---

## 5. Run the setup script

From the repo root:

```bash
./deploy/setup.sh
```

The script:
- installs Ollama (if missing) and pulls `qwen3:8b`,
- creates the `signal-bot` system user and `/var/lib/signal-bot`,
- installs the config, personalities and binary under `/etc/signal-bot` and
  `/opt/signal-bot`,
- creates a TLS certificate,
- installs the systemd unit.

It also **prompts for a dashboard username and password**. Choose a strong
password. Only an argon2 hash of it is stored.

> The script installs `default-jre` if no `java` is found. If you already
> installed Java 21 in step 3, it's left alone.

It does **not** install signal-cli or start the bot. That's next.

---

## 6. Install signal-cli

`signal-cli` is the unofficial command-line Signal client that the bot drives.

1. Open <https://github.com/AsamK/signal-cli/releases> and note the latest
   stable version, e.g. `0.13.x`. Use that number for `X.Y.Z` below.
2. Download the release and verify it against the checksum or signature
   published on the release page:

   ```bash
   VER=X.Y.Z
   curl -LO "https://github.com/AsamK/signal-cli/releases/download/v${VER}/signal-cli-${VER}.tar.gz"
   sha256sum "signal-cli-${VER}.tar.gz"   # compare with the release page
   ```

   (Or download `signal-cli-${VER}-Linux-native.tar.gz` for the build that
   needs no Java.)
3. Install it so the binary is at `/usr/local/bin/signal-cli`, which is the
   `signal_bin` path in the config:

   ```bash
   sudo tar -xzf "signal-cli-${VER}.tar.gz" -C /usr/local/
   sudo ln -sf "/usr/local/signal-cli-${VER}/bin/signal-cli" /usr/local/bin/signal-cli
   signal-cli --version
   ```

More detail is in [deploy/REGISTER.md](deploy/REGISTER.md).

---

## 7. Register the bot's Signal account

Run these **as the `signal-bot` user**, so the account data ends up where the
service expects it. Replace `+1XXXXXXXXXX` with the bot's number from step
2.1, in international format.

1. **Request a code:**

   ```bash
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +1XXXXXXXXXX register
   ```

   If it says a **captcha is required**:
   1. Open <https://signalcaptchas.org/registration/generate.html> in a
      browser and solve it.
   2. Right-click the **Open Signal** link and copy it. It starts with
      `signalcaptcha://`.
   3. Run:

      ```bash
      sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +1XXXXXXXXXX register --captcha "signalcaptcha://..."
      ```

   The captcha token expires within minutes, so use it right away. If the SMS
   never arrives, wait a minute and repeat with `--voice` added.
2. **Read the code** in the Google Voice web app (Messages, or the voicemail
   transcript for a voice call).
3. **Verify:**

   ```bash
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +1XXXXXXXXXX verify 123-456
   ```

4. **Set a registration lock PIN.** Strongly recommended: it stops anyone who
   later gets the number from taking over the account.

   ```bash
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +1XXXXXXXXXX setPin <PIN>
   ```

   Store the PIN in your password manager.
5. **Optional: set a profile name,** shown to people who chat with the bot:

   ```bash
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +1XXXXXXXXXX updateProfile --given-name "Bot"
   ```

6. **Smoke test.** Send yourself a message and confirm it arrives on your
   phone:

   ```bash
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +1XXXXXXXXXX send -m "hello" +1YOURNUMBER
   ```

> Never run `register` again for a working account. Re-registering (or
> registering the number somewhere else) signs this install out.

---

## 8. Edit the config

```bash
sudo nano /etc/signal-bot/config.toml
```

At minimum:

- Set **`signal_account`** to the bot's number (the same `+1XXXXXXXXXX`). The
  example file ships with a fictional placeholder.
- Optionally, set **`bind_addr`** to the host's LAN IP instead of
  `0.0.0.0:8443`.
- Optionally, set **`search_api_key`** to your Tavily key (see
  [step 11](#11-enable-web-search-optional)).

The file is owned by `root:signal-bot` with mode `0640`. Keep it that way,
because it may hold the Tavily key.

> **Back this file up before re-running `setup.sh`.** Older versions of the
> script overwrite it with the example config.

---

## 9. Start the service

```bash
sudo systemctl enable --now signal-bot
sudo systemctl status signal-bot
sudo journalctl -u signal-bot -f     # live logs; Ctrl-C to stop watching
```

Within a few seconds the logs should show `connected to signal-cli daemon
socket`.

---

## 10. First login and room setup

1. **Open the dashboard** at `https://bot.local:8443` (or
   `https://<host-LAN-IP>:8443`). Accept or trust the certificate:
   - If `mkcert` was used, install its root CA on your devices
     (`mkcert -CAROOT` shows where it is).
   - Otherwise, trust `/etc/signal-bot/tls/bot.local.crt`.
2. **Log in** with the username and password from step 5.
3. **Start a DM:** from your phone, send the bot a Signal message. It should
   reply, and the conversation appears under **Rooms**.
4. **Groups:** add the bot's number to a Signal group and send a message. The
   group appears under **Rooms**.
5. **Configure the room:** pick a **personality** and a **reply mode**:
   - `addressed`: reply only when @-mentioned or quoted (the default)
   - `always`: reply to everything
   - `proactive`: chime in when relevant

---

## 11. Enable web search (optional)

1. **Add the key:** put your Tavily key in `/etc/signal-bot/config.toml`:

   ```toml
   search_api_key = "tvly-..."
   ```

2. **Restart:** `sudo systemctl restart signal-bot`. The key is read only at
   startup. The log line `web-search provider configured (Tavily)` confirms it
   was picked up.
3. **Turn it on:** in the dashboard **Settings** page, set **Tools** to on and
   **Web search** to on, then save.
4. **Optional: edit the domain whitelist.** The global whitelist is on the
   Settings page. Per-persona extra sources are on each room page.

Web search only ever queries whitelisted domains, and results are passed to
the model as untrusted data.

---

## 12. Updating

```bash
cd toxic-bot
git pull
cargo build --release
sudo install -m 0755 target/release/signal-bot /opt/signal-bot/signal-bot
sudo cp personalities/*.toml /etc/signal-bot/personalities/   # if personas changed
sudo systemctl restart signal-bot
```

Database migrations apply automatically on startup. Personality files
hot-reload without a restart.

Keep **signal-cli** reasonably current as well. Signal periodically retires
old client versions, and an outdated signal-cli eventually stops working.
Repeat step 6 with the new version; the account data in
`/var/lib/signal-bot` is kept.

---

## 13. Troubleshooting

| Symptom | Check |
|---------|-------|
| `register` fails with a captcha error | Get a fresh captcha token and use it immediately (step 7). |
| No SMS arrives | Retry with `--voice`. Check the Google Voice messages and voicemail. Signal may be rate-limiting; wait an hour. |
| `UnsupportedClassVersionError` / Java errors | Java is older than 21. Install `openjdk-21-jre-headless`, or use the native signal-cli build. |
| Bot logs `timed out waiting for signal-cli socket` | Run the step-7 smoke test as `signal-bot`. The account must be registered in `/var/lib/signal-bot`, and `signal_account` must match. |
| Bot never replies in a group | The room's mode is probably `addressed`. @-mention the bot, or change the mode in the dashboard. |
| Replies are very slow | CPU-only inference is slow. Check `ollama ps` and the Health page. A smaller model or a GPU helps. |
| Web search never used | Check that `search_api_key` is set, the service was restarted, and both **Tools** and **Web search** are on. |
| Dashboard unreachable | Check the firewall rule, `bind_addr`, `systemctl status signal-bot`, and that you're using `https://` on port 8443. |

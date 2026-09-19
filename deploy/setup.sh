#!/usr/bin/env bash
set -euo pipefail
# 1. dependencies
command -v java >/dev/null || sudo apt-get install -y default-jre
command -v ollama >/dev/null || curl -fsSL https://ollama.com/install.sh | sh
ollama pull qwen3:8b
# signal-cli: install a pinned release to /usr/local/bin (see REGISTER.md for the exact version/URL)
# 2. service user (system, nologin, no sudo)
id signal-bot &>/dev/null || sudo useradd --system --home /var/lib/signal-bot --shell /usr/sbin/nologin signal-bot
sudo install -d -o signal-bot -g signal-bot -m 0700 /var/lib/signal-bot
sudo install -d -m 0755 /etc/signal-bot /etc/signal-bot/personalities /etc/signal-bot/tls /opt/signal-bot
# 3. config + personalities + binary
sudo install -m 0640 -o root -g signal-bot deploy/config.example.toml /etc/signal-bot/config.toml
sudo cp personalities/*.toml /etc/signal-bot/personalities/
sudo install -m 0755 target/release/signal-bot /opt/signal-bot/signal-bot
# 4. TLS
./deploy/gen-cert.sh
# 5. admin credential (prompts)
read -rp "Web admin username: " U; read -rsp "Web admin password: " P; echo
# Pass the password via env, not argv: argv is world-readable via /proc/<pid>/cmdline,
# while an env var is only readable by the process owner.
sudo -u signal-bot env "SIGNAL_BOT_ADMIN_PASSWORD=$P" /opt/signal-bot/signal-bot --config /etc/signal-bot/config.toml --set-admin "$U"
# 6. enable service (AFTER Signal registration in REGISTER.md)
sudo install -m 0644 deploy/signal-bot.service /etc/systemd/system/signal-bot.service
sudo systemctl daemon-reload
echo "Now complete deploy/REGISTER.md, then: sudo systemctl enable --now signal-bot"

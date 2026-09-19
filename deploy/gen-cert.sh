#!/usr/bin/env bash
set -euo pipefail
TLS=/etc/signal-bot/tls
sudo mkdir -p "$TLS"
if command -v mkcert >/dev/null; then
  mkcert -cert-file "$TLS/bot.local.crt" -key-file "$TLS/bot.local.key" bot.local
  echo "Import this CA on viewing devices: $(mkcert -CAROOT)/rootCA.pem"
else
  sudo openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
    -keyout "$TLS/bot.local.key" -out "$TLS/bot.local.crt" \
    -subj "/CN=bot.local" -addext "subjectAltName=DNS:bot.local"
  echo "Self-signed cert created; trust $TLS/bot.local.crt on viewing devices."
fi
sudo chown root:signal-bot "$TLS"/bot.local.*
sudo chmod 640 "$TLS"/bot.local.*

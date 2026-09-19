#!/usr/bin/env bash
set -euo pipefail
TLS=/etc/signal-bot/tls
sudo mkdir -p "$TLS"
if command -v mkcert >/dev/null; then
  # $TLS is root-owned (created via sudo mkdir above), so generate as the
  # current user into a temp dir first, then install into place with sudo.
  TMPDIR_CERT="$(mktemp -d)"
  trap 'rm -rf "$TMPDIR_CERT"' EXIT
  mkcert -cert-file "$TMPDIR_CERT/bot.local.crt" -key-file "$TMPDIR_CERT/bot.local.key" bot.local
  sudo install -m 640 "$TMPDIR_CERT/bot.local.crt" "$TLS/bot.local.crt"
  sudo install -m 640 "$TMPDIR_CERT/bot.local.key" "$TLS/bot.local.key"
  echo "Import this CA on viewing devices: $(mkcert -CAROOT)/rootCA.pem"
else
  sudo openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
    -keyout "$TLS/bot.local.key" -out "$TLS/bot.local.crt" \
    -subj "/CN=bot.local" -addext "subjectAltName=DNS:bot.local"
  echo "Self-signed cert created; trust $TLS/bot.local.crt on viewing devices."
fi
sudo chown root:signal-bot "$TLS"/bot.local.*
sudo chmod 640 "$TLS"/bot.local.*

## Install signal-cli

`setup.sh` installs a JRE (`default-jre`) but does not install `signal-cli` itself — do that manually first:

1. Pick a release: pin to the latest stable release, e.g. `vX.Y.Z`, from
   `https://github.com/AsamK/signal-cli/releases`. Release assets follow the
   pattern `https://github.com/AsamK/signal-cli/releases/download/vX.Y.Z/signal-cli-X.Y.Z.tar.gz`.
2. Verify the download before installing: check the published SHA256 checksum
   (or GPG signature, where provided) against the downloaded tarball —
   do not skip this step.
3. Untar to `/usr/local`:
   ```
   sudo tar -xzf signal-cli-X.Y.Z.tar.gz -C /usr/local/
   sudo ln -sf /usr/local/signal-cli-X.Y.Z/bin/signal-cli /usr/local/bin/signal-cli
   ```
   The symlink at `/usr/local/bin/signal-cli` must match the `signal_bin`
   value in `config.toml`.
4. Requires a JRE on `PATH` (Java 21+); `setup.sh` installs `default-jre` for this.
5. Confirm it works: `signal-cli --version`.

# Register the bot's Signal account (run once, as the signal-bot user)

signal-cli data lives in /var/lib/signal-bot (config dir passed via --config).

In the commands below, replace `<BOT_NUMBER>` with the bot's phone number in
E.164 form (e.g. `+15555550100`). It must match `signal_account` in
`/etc/signal-bot/config.toml`.

1. Register (may require solving a captcha; follow the printed link):
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a <BOT_NUMBER> register
   # If prompted for captcha:
   # sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a <BOT_NUMBER> register --captcha "<token>"
2. You will receive an SMS/voice code on the bot's number.
3. Verify:
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a <BOT_NUMBER> verify <CODE>
4. Smoke test (send yourself a message):
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a <BOT_NUMBER> send -m "hello" <YOUR_NUMBER>
5. Start the bot: sudo systemctl enable --now signal-bot

Notes:
- The number must NOT already be registered to Signal on another device.
- The daemon uses the same --config data dir the systemd unit passes.

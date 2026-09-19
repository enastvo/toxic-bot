# Register the bot's Signal account (run once, as the signal-bot user)

signal-cli data lives in /var/lib/signal-bot (config dir passed via --config).

1. Register (may require solving a captcha; follow the printed link):
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +14433996053 register
   # If prompted for captcha:
   # sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +14433996053 register --captcha "<token>"
2. You will receive an SMS/voice code on the Google Voice number.
3. Verify:
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +14433996053 verify <CODE>
4. Smoke test (send yourself a message):
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +14433996053 send -m "hello" <YOUR_NUMBER>
5. Start the bot: sudo systemctl enable --now signal-bot

Notes:
- The number must NOT already be registered to Signal on another device.
- The daemon uses the same --config data dir the systemd unit passes.

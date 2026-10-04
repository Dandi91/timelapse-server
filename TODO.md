# To do

## Serve over HTTPS, then keep the screen awake during playback

The PC goes to sleep while you're watching, even in fullscreen. Browsers don't count muted video
without audio as playback when deciding whether to keep the screen on, and our timelapses are
exactly that.

The fix is the Screen Wake Lock API: the page asks to keep the screen on while it plays. But
browsers only offer it on secure pages, so it's unavailable on `http://miniserver.lan:8080`.
Browser workarounds (e.g. Firefox's `dom.securecontext.allowlist`) were considered and rejected in
favour of real HTTPS.

1. **HTTPS on miniserver with a Let's Encrypt certificate for the user's own domain.**
   - A reverse proxy in front of the container is the likely route, e.g. Caddy, which obtains and
     renews certificates itself.
   - miniserver is LAN-only, so the certificate probably needs the DNS-01 challenge, through the
     DNS provider's API, instead of HTTP-01, which needs port 80 reachable from the internet.
   - Point a name, e.g. `timelapse.<domain>`, at miniserver's LAN address.
   - Follow the existing compose layout under `/opt/<service>`.
2. **Once it's served over HTTPS:**
   - Mark the session cookie `Secure`.
   - Consider HSTS.
   - Stop publishing port 8080 on the LAN, if everything goes through the proxy.
3. **Wake lock in the player.**
   - Request `navigator.wakeLock.request('screen')` while playing and release it on pause.
   - Request it again on `visibilitychange`, because the browser drops it whenever the tab is
     hidden.
   - Show a short notice when the API is unavailable, instead of failing silently.

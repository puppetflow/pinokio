<a href="https://puppetflow.com"><img src="https://www.puppetflow.com/img/puppetflow-promo-banner.png" width="100%" alt="Puppetflow" /></a>

# Pinokio

Minimal Chromium gateway written in Rust. It lets remote Puppeteer and Playwright clients connect to isolated Chromium instances over WebSocket, with token authentication, a concurrency limit, a FIFO queue and robust process cleanup.

## Quick start

```bash
docker build -t pinokio .
docker run --rm -p 3000:3000 --shm-size=1g -e TOKEN=secret pinokio
```

Then connect:

```js
const puppeteer = require("puppeteer-core");
const browser = await puppeteer.connect({
  browserWSEndpoint: "ws://localhost:3000?token=secret",
});
```

## Architecture

```text
src/
  main.rs        bootstrap, tracing, SIGTERM/SIGINT handling
  config.rs      env var parsing and validation (refuses to start on invalid values)
  errors.rs      error types mapped to HTTP status codes
  server.rs      Axum routes: / and /chromium (WebSocket), /health, /ready, /status
  auth.rs        token check (?token= or Authorization: Bearer), constant-time compare
  queue.rs       concurrency gate: session slots + FIFO queue
  chromium.rs    process launch, DevToolsActivePort discovery, SIGTERM/SIGKILL teardown
  session.rs     active session lifecycle and guaranteed cleanup
  proxy.rs       transparent bidirectional WebSocket relay (no CDP interpretation)
```

Session flow:

1. The client opens a WebSocket to `/` (or `/chromium`) with its token.
2. Before the upgrade is answered, the server authenticates the client, waits for a free slot (or queues the request), launches an isolated Chromium and connects to its local CDP endpoint. Any failure at this stage returns a proper HTTP status code.
3. The upgrade completes and the server relays frames between the client and Chromium without touching them.
4. When the client disconnects, Chromium exits, a timeout fires or the server shuts down, the session is torn down: Chromium process group killed (SIGTERM then SIGKILL), temp profile dir removed, slot released, next queued request served.

### Concurrency invariants

- Slots are a `tokio::sync::Semaphore` with `MAX_CONCURRENT_SESSIONS` permits. Tokio semaphores wake waiters in FIFO order, which provides the fair queue and makes it impossible to bypass.
- Queue admission uses an atomic counter with a compare-and-swap loop: once `MAX_QUEUE_LENGTH` waiters are registered, the next request is rejected immediately with 429. With 10 slots and a queue of 20, the 31st concurrent request is refused.
- A permit is owned by its session and released exactly once when the session ends, including on panic (drop guard). A client that disconnects while queued is removed from the queue automatically because its request future is dropped.
- No status messages are sent on the main endpoint before proxying: standard CDP clients (Puppeteer, Playwright) would break. Queue visibility is provided by `GET /status` instead.

### Chromium lifecycle

Each session launches its own process with `--headless=new`, `--remote-debugging-port=0` and a unique temp `--user-data-dir`. The CDP port is read from the `DevToolsActivePort` file that Chromium writes in that directory; logs are never parsed. The CDP endpoint listens on 127.0.0.1 only and is never exposed.

Each Chromium runs in its own process group (setsid). Teardown signals only that group: SIGTERM, 3 s grace, then SIGKILL, then the child is reaped. In Docker, tini (PID 1) reaps any re-parented grandchildren. No global `pkill` is ever used.

## Endpoints

| Endpoint | Description |
| --- | --- |
| `GET /` or `GET /chromium` | WebSocket upgrade, creates a session and proxies CDP |
| `GET /health` | Liveness: `{"status":"ok"}` |
| `GET /ready` | Readiness: 200 when accepting requests, 503 when shutting down or saturated |
| `GET /status` | Counters: active/queued sessions vs limits (token-protected when auth is on) |

HTTP errors before the upgrade:

| Code | Meaning |
| --- | --- |
| 401 | Missing or invalid token |
| 429 | Queue full |
| 503 | Server shutting down or Chromium unavailable |
| 504 | Queue timeout or Chromium startup timeout |

After the upgrade, sessions are closed with WebSocket codes 1000 (Chromium closed normally), 1001 (session timeout or server shutdown) or 1011 (Chromium error).

## Configuration

All configuration is done through environment variables, validated at startup. Invalid values prevent the server from starting.

| Variable | Default | Description |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `3000` | Listen port |
| `TOKEN` | empty | Shared secret; empty disables authentication |
| `MAX_CONCURRENT_SESSIONS` | `10` | Maximum simultaneously active Chromium sessions |
| `MAX_QUEUE_LENGTH` | `20` | Maximum requests waiting for a slot; beyond that, 429 |
| `CONNECTION_TIMEOUT_MS` | `600000` | Maximum lifetime of an active session |
| `QUEUE_TIMEOUT_MS` | `600000` | Maximum wait in the queue |
| `CHROME_STARTUP_TIMEOUT_MS` | `15000` | Time Chromium gets to publish its CDP endpoint |
| `SHUTDOWN_GRACE_PERIOD_MS` | `10000` | Time given to active sessions after SIGTERM/SIGINT |
| `CHROME_PATH` | `/usr/bin/chromium` | Chromium or Google Chrome binary |
| `CHROME_HEADLESS` | `true` | Run with `--headless=new` |
| `CHROME_NO_SANDBOX` | `true` | Add `--no-sandbox` (see security notes) |
| `CHROME_DISABLE_DEV_SHM_USAGE` | `true` | Add `--disable-dev-shm-usage` |
| `CHROME_EXTRA_ARGS` | empty | Extra Chromium args, whitespace-separated |
| `LOG_LEVEL` | `info` | trace, debug, info, warn, error |
| `TZ` | system | Timezone inherited by Chromium |
| `LANGUAGE` | system | Locale, also passed as Chromium `--lang` |

Clients cannot modify Chromium launch arguments. Query parameters other than `token` are ignored. Server-wide flags such as `--disable-web-security` or `--window-size` go in `CHROME_EXTRA_ARGS`.

## Client compatibility

### Puppeteer (works as-is)

```js
const puppeteer = require("puppeteer-core");

const browser = await puppeteer.connect({
  browserWSEndpoint: "ws://localhost:3000?token=secret",
});
try {
  const page = await browser.newPage();
  await page.goto("https://example.com");
  console.log(await page.title());
} finally {
  await browser.close();
}
```

### Playwright (CDP over a ws URL)

```js
const { chromium } = require("playwright");

const browser = await chromium.connectOverCDP("ws://localhost:3000?token=secret");
try {
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto("https://example.com");
  console.log(await page.title());
} finally {
  await browser.close();
}
```

Compatibility notes:

- Pinokio exposes a raw CDP WebSocket endpoint, not a Playwright-native (`connect()`) endpoint. Playwright must use `connectOverCDP`, which is Chromium-only and skips some Playwright-managed niceties (it attaches to the existing browser instead of controlling launch).
- The `http://` form of `connectOverCDP` is not supported: it relies on a `GET /json/version` discovery request, which would require creating the session outside the WebSocket handshake. Use the `ws://` form, verified to work.
- `wss://` is handled by your reverse proxy (Traefik, Nginx, Caddy, HAProxy); Pinokio itself does not terminate TLS.

## Docker

The provided `Dockerfile` builds a multi-stage image: Rust builder, then a Debian slim runtime with Chromium, running as a non-root user with tini as PID 1 and a healthcheck on `/health`.

Example compose service:

```yaml
services:
  pinokio:
    build: ./pinokio
    restart: unless-stopped
    ports:
      - "3000:3000"
    shm_size: "1gb"
    environment:
      TOKEN: "${PINOKIO_TOKEN:-}"
      MAX_CONCURRENT_SESSIONS: 10
      MAX_QUEUE_LENGTH: 20
      CONNECTION_TIMEOUT_MS: 600000
      TZ: "Europe/Paris"
      LANGUAGE: "fr-FR"
```

`shm_size` matters: Chromium uses `/dev/shm` for rendering buffers and the Docker default of 64 MB makes tabs crash under load. Give it 1 GB, or keep `CHROME_DISABLE_DEV_SHM_USAGE=true` (then Chromium falls back to `/tmp`, slightly slower but safe).

## Security notes

- The Chromium CDP port listens on 127.0.0.1 inside the container and is never exposed; only the authenticated proxy reaches it.
- Each session gets a unique temp profile directory, removed at teardown.
- Clients cannot inject launch arguments or execute anything server-side; the server never interprets CDP payloads.
- WebSocket frames are capped at 16 MiB (64 MiB per message) in both directions.
- `--no-sandbox` disables Chromium's internal sandbox. It is required in most containers (no user namespaces). Mitigations: the container runs as a non-root user, one Chromium per session, and pages you drive are the main threat, so avoid pointing sessions at untrusted content with sensitive credentials loaded. If your kernel allows it, set `CHROME_NO_SANDBOX=false`.
- Tokens are never logged; neither are CDP payloads or page contents.

## Known limitations

- One WebSocket connection = one Chromium instance. There is no session reuse, prebooting or `browserWSEndpoint` reconnection to a running session.
- Playwright only via `connectOverCDP` with a `ws://` URL (no `/json/version` discovery, no Playwright-native protocol).
- Metrics are limited to the `/status` counters.

## License

Proprietary. Use is permitted only as part of the Puppetflow product; see [LICENSE](LICENSE). Third-party dependencies (Rust crates, Chromium) keep their own licenses.

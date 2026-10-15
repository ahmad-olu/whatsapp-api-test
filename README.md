# whatsapp-gateway

```
 WhatsApp user
      |
      v
 +------------+   POST /webhook    +---------------------------------------------+
 | Meta Cloud |------------------->| gateway-server                              |
 | API        |<-------------------|                                             |
 +------------+  Graph API sends   |  webhook handler (fast path)                |
                                   |    1. HMAC-SHA256 over the RAW body         |
                                   |    2. INSERT raw_events  (durable)          |
                                   |    3. return 200                            |
                                   |                                             |
                                   |  ingest worker                              |
                                   |    normalize -> dedupe(UNIQUE key)          |
                                   |    -> 24h window tracker                    |
                                   |    -> status state machine (forward-only)   |
                                   |                                             |
                                   |  delivery worker (retry + backoff)          |
                                   |    download media -> EventSink              |
                                   +----------------------+----------------------+
                                                          |
                              +---------------------------+---------------------+
                              |                           |                     |
                         SINK_MODE=echo            SINK_MODE=forward     SINK_MODE=log
                         demo bot replies          POST signed JSON      just log it
                         on WhatsApp               to your main app
```

## Workspace layout

| crate | what it is |
|---|---|
| `crates/gateway-core` | Pure logic, no I/O: Meta payload types, signature check, normalization, status state machine, 24h window, outbound message builders/validation, demo bot. |
| `crates/gateway-server` | axum + Postgres service: webhook, queues, Graph transport, sinks, admin API. |
| `crates/gateway-sim` | CLI: sends correctly-signed fake Meta webhooks, calls the admin API, and runs a `smoke` test against any deployment. |

## Quick start (local)

```bash
docker compose up -d                       # Postgres on :5432
cp .env.example .env                       # then edit it (see below)
cargo run -p gateway-server                # listens on :8080

# in another terminal (loads the same .env values)
set -a; source .env; set +a
cargo run -p gateway-sim -- smoke
```

For a first local run with no Meta credentials, put these in `.env`:
`WHATSAPP_APP_SECRET=anything`, `ADMIN_TOKEN=anything`,
`WHATSAPP_VERIFY_TOKEN=anything`, `SINK_MODE=log`. The smoke test passes
without ever contacting WhatsApp.

## Connect your Meta test number

1. **developers.facebook.com → your app → WhatsApp → API Setup.** Copy:
   - **Temporary access token** → `WHATSAPP_ACCESS_TOKEN` (expires in about 24 hours)
   - **Phone number ID** (not the phone number) → `WHATSAPP_PHONE_NUMBER_ID`
2. **App settings → Basic → App secret** → `WHATSAPP_APP_SECRET`.
3. On API Setup, add your own phone to the **"To" recipient list**. A test number
   can only message numbers on that list; replies to anyone else fail.
4. Deploy (below) so you have a public HTTPS URL, then **WhatsApp → Configuration → Webhook**:
   - Callback URL: `https://<your-service>.up.railway.app/webhook`
   - Verify token: the exact `WHATSAPP_VERIFY_TOKEN` you set
   - Click **Verify and save**, then **subscribe to the `messages` field**.
5. Send a WhatsApp message *from your phone to the test number*, e.g.
   `sold 5 bags rice 25000`. You should get Confirm / Cancel buttons back.

If webhooks never arrive, check in this order: the `messages` field is subscribed;
the app is subscribed to the WhatsApp Business Account (Meta documents a
`POST /{waba-id}/subscribed_apps` call for this); the service logs for
`webhook rejected: bad signature` (wrong `WHATSAPP_APP_SECRET`).

## Deploy to Railway

1. Push this repo to GitHub. In Railway: **New Project → Deploy from GitHub repo**.
   `railway.toml` makes it build with the Dockerfile and health-check `/healthz`.
2. **Add → Database → PostgreSQL** in the same project.
3. On the gateway service, **Variables**:
   - `DATABASE_URL` = `${{Postgres.DATABASE_URL}}` (Railway reference variable)
   - `WHATSAPP_VERIFY_TOKEN`, `WHATSAPP_APP_SECRET`, `ADMIN_TOKEN`
   - `WHATSAPP_ACCESS_TOKEN`, `WHATSAPP_PHONE_NUMBER_ID`
   - `SINK_MODE=echo`
   - Railway sets `PORT` itself.
4. **Settings → Networking → Generate Domain** to get the public URL for Meta.
5. Verify from your machine:
   ```bash
   export GATEWAY_URL=https://<your-service>.up.railway.app
   export WHATSAPP_APP_SECRET=... WHATSAPP_VERIFY_TOKEN=... ADMIN_TOKEN=...
   cargo run -p gateway-sim -- smoke
   ```

Migrations run automatically on boot. The temporary Meta token expires after
about a day; when sends start failing with an OAuth error, refresh it (or create
a System User token for something permanent).

## Environment variables

| variable | required | notes |
|---|---|---|
| `DATABASE_URL` | yes | Postgres |
| `WHATSAPP_VERIFY_TOKEN` | yes | webhook handshake secret you invent |
| `WHATSAPP_APP_SECRET` | yes* | verifies `X-Hub-Signature-256`. *Unless `INSECURE_SKIP_SIGNATURE=true` |
| `ADMIN_TOKEN` | yes | Bearer token for `/admin/*` |
| `WHATSAPP_ACCESS_TOKEN` | for sending/media | missing => sends return 503 |
| `WHATSAPP_PHONE_NUMBER_ID` | recommended | default sender; also seeds the `default` workspace |
| `SINK_MODE` | no | `echo` (default), `forward`, `log` |
| `FORWARD_URL`, `FORWARD_SECRET` | if `forward` | where/how to deliver events to your main app |
| `AUTO_MARK_READ` | no | echo mode sends blue ticks (default `true`) |
| `GRAPH_API_VERSION` | no | default `v26.0`. **Meta retires old versions**, so keep this configurable |
| `GRAPH_BASE_URL` | no | override for tests |
| `PORT`, `MAX_BODY_BYTES`, `MEDIA_MAX_BYTES`, `RUST_LOG` | no | |

## Admin API

All need `Authorization: Bearer $ADMIN_TOKEN`.

| route | purpose |
|---|---|
| `POST /admin/send` | `{"phone_number_id"?, "message": {...}}`. Types: `text`, `buttons`, `template`, `document`, `image`, `mark_read`. Free-form is refused with **409** if the 24h window is closed. |
| `GET /admin/events?limit=` | normalized events + delivery state/attempts/error |
| `GET /admin/raw?limit=` | verbatim webhook bodies as received, with processing state |
| `POST /admin/raw/{id}/replay` | re-derive events from a stored raw body (debugging) |
| `GET /admin/outbound?limit=` | messages you sent + their sent/delivered/read/failed status |
| `GET /admin/window?customer=` | is the 24h window open for this customer? |
| `GET /admin/usage` | sends by kind / number, billable counts by pricing category |
| `GET /admin/media/{id}` | a downloaded inbound image / voice note |
| `PUT /admin/workspaces` | map a business `phone_number_id` to a `workspace_id` (multi-tenant routing) |

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" -H 'content-type: application/json' \
  -d '{"message":{"type":"template","to":"2348012345678","name":"hello_world","language":"en_US"}}' \
  $GATEWAY_URL/admin/send
```

## The contract with your main app

Set `SINK_MODE=forward`. For each new event the gateway does:

```
POST $FORWARD_URL
content-type: application/json
x-gateway-signature-256: sha256=<hex HMAC-SHA256 of the raw body, key = FORWARD_SECRET>
idempotency-key: msg:wamid.XXXX          (or st:wamid.XXXX:delivered)

{ "id": 42, "workspace_id": "acme" | null, "event": <Event> }
```

`Event` is one of:

```json
{"kind":"message","id":"wamid...","phone_number_id":"1000000001","from":"2348012345678",
 "profile_name":"Ada","timestamp":1700000000,"reply_to":null,
 "content":{"type":"text","body":"sold 5 bags rice 25000"}}

{"kind":"status","id":"wamid...","phone_number_id":"1000000001","recipient":"2348012345678",
 "status":"delivered","timestamp":1700000005,"billable":true,"pricing_category":"service","error":null}
```

`content.type` is one of `text`, `image`, `audio` (with `voice: true` for voice
notes), `video`, `document`, `sticker`, `location`, `button_reply`, `list_reply`,
`reaction`, or `unsupported`. Unknown future types never fail parsing.

Guarantees your consumer must be built for:

- **At-least-once.** A non-2xx or timeout is retried with backoff (5s, 10s, 20s ... capped at 15 min, 10 attempts). **De-duplicate on `idempotency-key`.** For a ledger, a duplicate means a double-posted transaction.
- **Not strictly ordered across retries.** Don't assume a `delivered` status arrives before `read`.
- Return 2xx quickly; do heavy work asynchronously.

Because your main app is Rust, depend on the core crate and reuse the exact types
and signature check:

```toml
gateway-core = { git = "https://github.com/YOU/whatsapp-gateway", package = "gateway-core" }
```

```rust
use gateway_core::{signature, Event};

#[derive(serde::Deserialize)]
struct Envelope { id: i64, workspace_id: Option<String>, event: Event }

async fn whatsapp_events(headers: HeaderMap, body: Bytes) -> StatusCode {
    let sig = headers.get("x-gateway-signature-256").and_then(|v| v.to_str().ok());
    if signature::verify(&FORWARD_SECRET, &body, sig).is_err() {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(env) = serde_json::from_slice::<Envelope>(&body) else { return StatusCode::BAD_REQUEST };
    // dedupe on env.event.dedupe_key(), enqueue, return 200 fast
    StatusCode::OK
}
```

## Testing

```bash
cargo test -p gateway-core                       # pure logic, no setup
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres \
  cargo test -p gateway-server                   # + 15 end-to-end tests
```

The end-to-end tests boot the real HTTP server against a real Postgres (each test
gets its own throwaway database) and a fake Graph API. They cover: the verify
handshake; signature rejection; duplicate webhook delivery; out-of-order statuses;
the 24h window; media download with auth; forwarding with signature and
idempotency key; failed-forward retry bookkeeping; replay; tenant routing; usage.
Without `TEST_DATABASE_URL` they skip.

Simulate by hand:

```bash
gateway-sim simulate text --text "sold 5 bags rice 25000"
gateway-sim simulate duplicate            # same webhook 3x -> exactly one event
gateway-sim simulate status-sequence      # read, delivered, sent -> settles on read
gateway-sim simulate bad-signature        # must be 401
gateway-sim inspect events
```

Caution: the simulator posts to the gateway as if it were Meta. With `SINK_MODE=echo`
and real credentials the bot will then really try to reply to `--from` (use your own
allow-listed number, or `SINK_MODE=log`).

## Design notes and known limits

- **Persist first, process later.** The webhook handler does exactly one thing (an
  `INSERT`) before answering 200, so a slow or crashing consumer never costs you an
  event and Meta never retries because of your logic. Unsigned requests are rejected
  before anything is stored.
- **Signature over raw bytes.** Re-serializing parsed JSON breaks the HMAC; the
  handler never does.
- **Idempotency is a database constraint** (`events.dedupe_key UNIQUE`), not an
  in-memory set, so it survives restarts and multiple instances. Queues use
  `FOR UPDATE SKIP LOCKED`, so running more than one instance is safe.
- **Status state machine** lives in `gateway-core::status` and is unit-tested:
  `sent -> delivered -> read` only ever advance, `failed` is terminal and can't
  override `delivered`/`read`.
- **Echo bot can double-reply on retry.** If its second reply fails after the first
  succeeded, a retry resends the first. It's a demo; your real consumer should be
  idempotent on the event id.
- **Media is stored in Postgres** (`BYTEA`, 16 MB cap) because Meta's media URLs
  expire in minutes and this survives Railway redeploys without a volume. Fine for a
  test app; use object storage for production volume.
- **`replay` is a debugging tool**: it re-derives events but does not roll back
  conversation/status state.
- **No outbound rate limiting or automatic send retry.** An automatic retry after an
  ambiguous failure could double-send; the caller decides.
- **Cost visibility.** Meta bills per delivered message and has been changing which
  service/utility messages are free. `GET /admin/usage` shows what you actually sent
  and what Meta marked billable. Check Meta's current pricing page for Nigeria before
  modelling costs.
- **Nigerian numbers.** WhatsApp ids are digits with country code (`2348012345678`).
  Outbound recipients are normalized to digits (so `+234 801 234 5678` works), but a
  local `0801...` number is not auto-converted.

## Go-live checklist

- [ ] Replace the temporary token with a **System User** token
- [ ] Business verification complete; real number registered (not the test number)
- [ ] Message **templates** created and approved (needed for anything outside the 24h window)
- [ ] `INSECURE_SKIP_SIGNATURE` unset; `ADMIN_TOKEN` long and random
- [ ] `SINK_MODE=forward`, main app verifies `x-gateway-signature-256` and de-duplicates on `idempotency-key`
- [ ] Alert on events with `forward_attempts >= 10` (dead letters) in `/admin/events`
- [ ] Pin `GRAPH_API_VERSION` deliberately and calendar its retirement date

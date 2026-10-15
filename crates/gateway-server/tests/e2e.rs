//! End-to-end tests: real HTTP server + real Postgres + fake Graph API.
//!
//! Requires `TEST_DATABASE_URL` pointing at a Postgres *server* where the
//! role may CREATE DATABASE, e.g. `postgres://postgres@127.0.0.1:5432/postgres`.
//! Each test creates its own throwaway database. Without the variable the
//! tests print a notice and pass trivially, so `cargo test` works anywhere.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gateway_core::{fixtures, signature};
use gateway_server::config::Config;
use gateway_server::db::Db;
use gateway_server::sender::now_secs;
use gateway_server::transport::GraphTransport;
use gateway_server::Gateway;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const PNID: &str = "1000000001";
const CUSTOMER: &str = "2348012345678";
const APP_SECRET: &str = "test-app-secret";
const ADMIN: &str = "test-admin";
const VERIFY: &str = "test-verify";
const TOKEN: &str = "test-access-token";

static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Fake Graph `/messages`: unique wamid per message, `{"success":true}` for read receipts.
struct GraphSend {
    n: AtomicUsize,
}

impl Respond for GraphSend {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        if v["status"] == "read" {
            return ResponseTemplate::new(200).set_body_json(json!({ "success": true }));
        }
        let n = self.n.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(json!({
            "messaging_product": "whatsapp",
            "contacts": [{ "input": v["to"], "wa_id": v["to"] }],
            "messages": [{ "id": format!("wamid.out.{n}") }]
        }))
    }
}

struct Harness {
    base: String,
    http: reqwest::Client,
    mock: MockServer,
    _gw: Gateway,
}

async fn eventually<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    for _ in 0..150 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

impl Harness {
    /// `None` (test skipped) when no Postgres is configured.
    async fn start(extra: &[(&str, &str)]) -> Option<Self> {
        let Ok(admin_url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("TEST_DATABASE_URL not set: skipping end-to-end test");
            return None;
        };

        let name = format!(
            "gw_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            DB_COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin_pool)
            .await
            .unwrap();
        let db_url = format!("{}/{}", admin_url.rsplit_once('/').unwrap().0, name);

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v26\.0/[^/]+/messages$"))
            .respond_with(GraphSend {
                n: AtomicUsize::new(0),
            })
            .mount(&mock)
            .await;

        let mut vars: std::collections::HashMap<String, String> = [
            ("DATABASE_URL", db_url.as_str()),
            ("WHATSAPP_VERIFY_TOKEN", VERIFY),
            ("WHATSAPP_APP_SECRET", APP_SECRET),
            ("ADMIN_TOKEN", ADMIN),
            ("WHATSAPP_ACCESS_TOKEN", TOKEN),
            ("WHATSAPP_PHONE_NUMBER_ID", PNID),
            ("GRAPH_BASE_URL", mock.uri().as_str()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in extra {
            // `{MOCK}` expands to the fake Graph/consumer server's base URL.
            vars.insert(k.to_string(), v.replace("{MOCK}", &mock.uri()));
        }
        let config = Config::from_lookup(|k| vars.get(k).cloned()).unwrap();

        let db = Db::connect(&db_url).await.unwrap();
        let transport = Arc::new(GraphTransport::new(
            config.graph_base_url.clone(),
            config.graph_version.clone(),
            config.access_token.clone(),
            config.media_max_bytes,
        ));
        let gw = Gateway::start(config, db, transport).await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = gw.router();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        Some(Self {
            base,
            http: reqwest::Client::new(),
            mock,
            _gw: gw,
        })
    }

    async fn post_webhook(&self, payload: &Value) -> reqwest::StatusCode {
        let body = serde_json::to_vec(payload).unwrap();
        self.http
            .post(format!("{}/webhook", self.base))
            .header("content-type", "application/json")
            .header("x-hub-signature-256", signature::sign(APP_SECRET, &body))
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn admin_get(&self, p: &str) -> Value {
        let r = self
            .http
            .get(format!("{}{}", self.base, p))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "GET {p}");
        r.json().await.unwrap()
    }

    async fn admin_post(&self, p: &str, body: &Value) -> (reqwest::StatusCode, Value) {
        let r = self
            .http
            .post(format!("{}{}", self.base, p))
            .bearer_auth(ADMIN)
            .json(body)
            .send()
            .await
            .unwrap();
        let status = r.status();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    /// Bodies of every non-read-receipt message POSTed to the fake Graph API.
    async fn graph_sends(&self) -> Vec<Value> {
        self.mock
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().ends_with("/messages"))
            .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .filter(|v| v["status"] != "read")
            .collect()
    }

    async fn mark_reads(&self) -> Vec<Value> {
        self.mock
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .filter(|v| v["status"] == "read")
            .collect()
    }

    async fn processed_raw(&self) -> usize {
        self.admin_get("/admin/raw?limit=100")
            .await
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["processed"] == true)
            .count()
    }

    async fn wait_raw_processed(&self, n: usize) {
        eventually(&format!("{n} raw events processed"), || async {
            (self.processed_raw().await >= n).then_some(())
        })
        .await;
    }

    async fn events(&self) -> Vec<Value> {
        self.admin_get("/admin/events?limit=100")
            .await
            .as_array()
            .unwrap()
            .clone()
    }

    async fn outbound(&self, wamid: &str) -> Option<Value> {
        self.admin_get("/admin/outbound?limit=100")
            .await
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["wamid"] == wamid)
            .cloned()
    }
}

#[tokio::test]
async fn verification_handshake() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let ok = h
        .http
        .get(format!(
            "{}/webhook?hub.mode=subscribe&hub.verify_token={VERIFY}&hub.challenge=987654",
            h.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.text().await.unwrap(), "987654");

    let bad = h
        .http
        .get(format!(
            "{}/webhook?hub.mode=subscribe&hub.verify_token=wrong&hub.challenge=1",
            h.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 403);

    let wrong_mode = h
        .http
        .get(format!(
            "{}/webhook?hub.mode=unsubscribe&hub.verify_token={VERIFY}&hub.challenge=1",
            h.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_mode.status(), 403);
}

#[tokio::test]
async fn rejects_unsigned_and_wrongly_signed_webhooks() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let payload = fixtures::text(PNID, CUSTOMER, "wamid.X", now_secs(), "hi");
    let body = serde_json::to_vec(&payload).unwrap();

    let unsigned = h
        .http
        .post(format!("{}/webhook", h.base))
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), 401);

    let wrong = h
        .http
        .post(format!("{}/webhook", h.base))
        .header(
            "x-hub-signature-256",
            signature::sign("some-other-secret", &body),
        )
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    // Nothing was stored, nothing was sent.
    assert_eq!(h.admin_get("/admin/raw").await.as_array().unwrap().len(), 0);
    assert!(h.graph_sends().await.is_empty());
}

#[tokio::test]
async fn admin_api_requires_token() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let r = h
        .http
        .get(format!("{}/admin/events", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = h
        .http
        .get(format!("{}/admin/events", h.base))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn text_message_gets_confirm_buttons_and_read_receipt() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let st = h
        .post_webhook(&fixtures::text(
            PNID,
            CUSTOMER,
            "wamid.IN1",
            now_secs(),
            "sold 5 bags rice 25000",
        ))
        .await;
    assert_eq!(st, 200);

    let sends = eventually("interactive reply", || async {
        let s = h.graph_sends().await;
        (!s.is_empty()).then_some(s)
    })
    .await;
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["type"], "interactive");
    assert_eq!(sends[0]["to"], CUSTOMER);
    assert!(sends[0]["interactive"]["body"]["text"]
        .as_str()
        .unwrap()
        .contains("₦25,000"));
    assert_eq!(
        sends[0]["interactive"]["action"]["buttons"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let reads = h.mark_reads().await;
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0]["message_id"], "wamid.IN1");

    // Outbound recorded; 24h window open for the customer.
    let rec = eventually("outbound recorded", || async {
        h.outbound("wamid.out.0").await
    })
    .await;
    assert_eq!(rec["kind"], "buttons");
    let w = h
        .admin_get(&format!("/admin/window?customer={CUSTOMER}"))
        .await;
    assert_eq!(w["window_open"], true);
}

#[tokio::test]
async fn full_confirm_loop_via_button_reply() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    h.post_webhook(&fixtures::text(
        PNID,
        CUSTOMER,
        "wamid.IN1",
        now_secs(),
        "bought diesel 12,500",
    ))
    .await;
    let sends = eventually("buttons", || async {
        let s = h.graph_sends().await;
        (!s.is_empty()).then_some(s)
    })
    .await;
    let button_id = sends[0]["interactive"]["action"]["buttons"][0]["reply"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    h.post_webhook(&fixtures::button_reply(
        PNID,
        CUSTOMER,
        "wamid.IN2",
        now_secs(),
        &button_id,
        "Confirm",
    ))
    .await;
    let sends = eventually("confirmation", || async {
        let s = h.graph_sends().await;
        (s.len() >= 2).then_some(s)
    })
    .await;
    let text = sends[1]["text"]["body"].as_str().unwrap();
    assert!(text.contains("Recorded expense"), "{text}");
    assert!(text.contains("₦12,500"), "{text}");
}

#[tokio::test]
async fn duplicate_webhook_delivery_is_idempotent() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let payload = fixtures::text(PNID, CUSTOMER, "wamid.DUP", now_secs(), "sold bread 1000");
    // Meta retries: the very same payload, three times.
    for _ in 0..3 {
        assert_eq!(h.post_webhook(&payload).await, 200);
    }
    h.wait_raw_processed(3).await;
    eventually("one reply", || async {
        (!h.graph_sends().await.is_empty()).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let msgs: Vec<_> = h
        .events()
        .await
        .into_iter()
        .filter(|e| e["kind"] == "message")
        .collect();
    assert_eq!(msgs.len(), 1, "duplicates must collapse to one event");
    assert_eq!(
        h.graph_sends().await.len(),
        1,
        "and produce exactly one reply"
    );
}

#[tokio::test]
async fn statuses_only_move_forward_even_when_out_of_order() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let ts = now_secs();
    // Arrival order: read, delivered, sent  (the reverse of reality)
    for s in ["read", "delivered", "sent"] {
        assert_eq!(
            h.post_webhook(&fixtures::status(PNID, CUSTOMER, "wamid.OUT1", s, ts, None))
                .await,
            200
        );
    }
    h.wait_raw_processed(3).await;
    let m = h.outbound("wamid.OUT1").await.expect("row");
    assert_eq!(m["status"], "read");
    assert_eq!(m["billable"], true);
    assert_eq!(m["pricing_category"], "service");

    // `failed` cannot revive or regress a read message.
    h.post_webhook(&fixtures::status(
        PNID,
        CUSTOMER,
        "wamid.OUT1",
        "failed",
        ts,
        Some((131000, "x")),
    ))
    .await;
    h.wait_raw_processed(4).await;
    assert_eq!(h.outbound("wamid.OUT1").await.unwrap()["status"], "read");

    // But a message that only reached `sent` can fail, and failure is terminal.
    h.post_webhook(&fixtures::status(
        PNID,
        CUSTOMER,
        "wamid.OUT2",
        "sent",
        ts,
        None,
    ))
    .await;
    h.post_webhook(&fixtures::status(
        PNID,
        CUSTOMER,
        "wamid.OUT2",
        "failed",
        ts,
        Some((131047, "Re-engagement")),
    ))
    .await;
    h.post_webhook(&fixtures::status(
        PNID,
        CUSTOMER,
        "wamid.OUT2",
        "delivered",
        ts,
        None,
    ))
    .await;
    h.wait_raw_processed(7).await;
    let m = h.outbound("wamid.OUT2").await.unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["error_code"], 131047);
}

#[tokio::test]
async fn window_guard_blocks_freeform_but_allows_templates() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let stranger = "2348099999999";

    // No inbound message ever: free-form is refused locally, Meta is never called.
    let (st, body) = h
        .admin_post(
            "/admin/send",
            &json!({ "message": { "type": "text", "to": stranger, "body": "hello" } }),
        )
        .await;
    assert_eq!(st, 409, "{body}");
    assert!(h.graph_sends().await.is_empty());

    // Templates are allowed outside the window.
    let (st, body) = h
        .admin_post(
            "/admin/send",
            &json!({ "message": { "type": "template", "to": stranger, "name": "hello_world", "language": "en_US" } }),
        )
        .await;
    assert_eq!(st, 200, "{body}");
    assert!(body["wamid"].as_str().unwrap().starts_with("wamid.out."));

    // Customer writes in -> window opens -> free-form allowed (even with a '+' formatted number).
    h.post_webhook(&fixtures::text(
        PNID,
        stranger,
        "wamid.S1",
        now_secs(),
        "hi",
    ))
    .await;
    h.wait_raw_processed(1).await;
    eventually("echo reply", || async {
        (h.graph_sends().await.len() >= 2).then_some(())
    })
    .await;
    let (st, body) = h
        .admin_post(
            "/admin/send",
            &json!({ "message": { "type": "text", "to": "+234 809 999 9999", "body": "hello" } }),
        )
        .await;
    assert_eq!(st, 200, "{body}");

    // An inbound from > 24h ago does not open the window.
    let old = "2348077777777";
    h.post_webhook(&fixtures::text(
        PNID,
        old,
        "wamid.OLD",
        now_secs() - 25 * 3600,
        "old",
    ))
    .await;
    h.wait_raw_processed(2).await;
    let (st, _) = h
        .admin_post(
            "/admin/send",
            &json!({ "message": { "type": "text", "to": old, "body": "late" } }),
        )
        .await;
    assert_eq!(st, 409);
}

#[tokio::test]
async fn invalid_outbound_is_rejected_locally() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let (st, _) = h
        .admin_post(
            "/admin/send",
            &json!({ "message": { "type": "buttons", "to": CUSTOMER, "body": "x", "buttons": [] } }),
        )
        .await;
    assert_eq!(st, 422);
    assert!(h.graph_sends().await.is_empty());
}

#[tokio::test]
async fn inbound_media_is_downloaded_with_auth_and_stored() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];

    Mock::given(method("GET"))
        .and(path("/v26.0/MEDIA1"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": format!("{}/dl/MEDIA1", h.mock.uri()),
            "mime_type": "image/jpeg", "sha256": "ignored", "file_size": jpeg.len(), "id": "MEDIA1"
        })))
        .mount(&h.mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/dl/MEDIA1"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg.clone()))
        .mount(&h.mock)
        .await;

    h.post_webhook(&fixtures::image(
        PNID,
        CUSTOMER,
        "wamid.IMG",
        now_secs(),
        "MEDIA1",
        Some("receipt"),
    ))
    .await;

    let bytes = eventually("media stored", || async {
        let r = h
            .http
            .get(format!("{}/admin/media/MEDIA1", h.base))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap();
        if r.status() == 200 {
            assert_eq!(r.headers()["content-type"], "image/jpeg");
            Some(r.bytes().await.unwrap().to_vec())
        } else {
            None
        }
    })
    .await;
    assert_eq!(bytes, jpeg);
    // And the bot still replied about the receipt.
    let sends = eventually("reply", || async {
        let s = h.graph_sends().await;
        (!s.is_empty()).then_some(s)
    })
    .await;
    assert!(sends[0]["text"]["body"]
        .as_str()
        .unwrap()
        .contains("receipt"));
}

#[tokio::test]
async fn forward_sink_posts_signed_events_with_idempotency_key() {
    let Some(h) = Harness::start(&[
        ("SINK_MODE", "forward"),
        ("FORWARD_URL", "{MOCK}/forward"),
        ("FORWARD_SECRET", "fwd-secret"),
    ])
    .await
    else {
        return;
    };
    Mock::given(method("POST"))
        .and(path("/forward"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&h.mock)
        .await;

    h.post_webhook(&fixtures::text(
        PNID,
        CUSTOMER,
        "wamid.F1",
        now_secs(),
        "sold rice 500",
    ))
    .await;

    let req = eventually("forwarded request", || async {
        h.mock
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.url.path() == "/forward")
    })
    .await;

    // Signed exactly like Meta signs ours, so the main app can reuse the same check.
    let sig = req
        .headers
        .get("x-gateway-signature-256")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        signature::verify("fwd-secret", &req.body, Some(&sig)),
        Ok(())
    );
    assert_eq!(
        req.headers
            .get("idempotency-key")
            .unwrap()
            .to_str()
            .unwrap(),
        "msg:wamid.F1"
    );

    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["workspace_id"], "default");
    assert_eq!(body["event"]["kind"], "message");
    assert_eq!(body["event"]["from"], CUSTOMER);
    assert_eq!(body["event"]["content"]["type"], "text");
    assert_eq!(body["event"]["content"]["body"], "sold rice 500");

    // Forward mode never talks to WhatsApp by itself.
    assert!(h.graph_sends().await.is_empty());
    eventually("marked delivered", || async {
        h.events()
            .await
            .iter()
            .any(|e| e["delivered"] == true)
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn failed_forward_is_recorded_and_scheduled_for_retry() {
    let Some(h) = Harness::start(&[
        ("SINK_MODE", "forward"),
        ("FORWARD_URL", "{MOCK}/forward"),
        ("FORWARD_SECRET", "fwd-secret"),
    ])
    .await
    else {
        return;
    };
    Mock::given(method("POST"))
        .and(path("/forward"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.mock)
        .await;

    h.post_webhook(&fixtures::text(
        PNID,
        CUSTOMER,
        "wamid.F2",
        now_secs(),
        "sold rice 500",
    ))
    .await;

    let ev = eventually("failure recorded", || async {
        h.events()
            .await
            .into_iter()
            .find(|e| e["forward_attempts"].as_i64().unwrap_or(0) >= 1)
    })
    .await;
    assert_eq!(ev["delivered"], false);
    assert!(
        ev["forward_error"].as_str().unwrap().contains("500"),
        "{ev}"
    );
    // Event itself is safely stored regardless of the consumer being down.
    assert_eq!(ev["kind"], "message");
}

#[tokio::test]
async fn replay_rederives_events_from_the_raw_payload() {
    let Some(h) = Harness::start(&[("SINK_MODE", "log")]).await else {
        return;
    };
    h.post_webhook(&fixtures::text(
        PNID,
        CUSTOMER,
        "wamid.R1",
        now_secs(),
        "hello",
    ))
    .await;
    h.wait_raw_processed(1).await;
    eventually("delivered", || async {
        h.events()
            .await
            .iter()
            .any(|e| e["delivered"] == true)
            .then_some(())
    })
    .await;

    let raw_id = h.admin_get("/admin/raw").await[0]["id"].as_i64().unwrap();
    let (st, _) = h
        .admin_post(&format!("/admin/raw/{raw_id}/replay"), &json!({}))
        .await;
    assert_eq!(st, 200);

    // Re-derived (not blocked by dedupe) and re-delivered.
    eventually("re-derived", || async {
        let evs = h.events().await;
        (evs.len() == 1 && evs[0]["delivered"] == true).then_some(())
    })
    .await;

    let (st, _) = h.admin_post("/admin/raw/999999/replay", &json!({})).await;
    assert_eq!(st, 404);
}

#[tokio::test]
async fn tenant_routing_maps_phone_number_to_workspace() {
    let Some(h) = Harness::start(&[("SINK_MODE", "log")]).await else {
        return;
    };
    let other = "2000000002";

    // Default number is seeded; an unknown number is still stored, flagged unrouted.
    h.post_webhook(&fixtures::text(PNID, CUSTOMER, "wamid.T1", now_secs(), "a"))
        .await;
    h.post_webhook(&fixtures::text(
        other,
        CUSTOMER,
        "wamid.T2",
        now_secs(),
        "b",
    ))
    .await;
    h.wait_raw_processed(2).await;
    let evs = h.events().await;
    let ws =
        |key: &str| evs.iter().find(|e| e["dedupe_key"] == key).unwrap()["workspace_id"].clone();
    assert_eq!(ws("msg:wamid.T1"), "default");
    assert_eq!(ws("msg:wamid.T2"), Value::Null);

    // Map the second number to a workspace; later events route correctly.
    let r = h
        .http
        .put(format!("{}/admin/workspaces", h.base))
        .bearer_auth(ADMIN)
        .json(&json!({ "phone_number_id": other, "workspace_id": "acme", "display_name": "Acme Ltd" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    h.post_webhook(&fixtures::text(
        other,
        CUSTOMER,
        "wamid.T3",
        now_secs(),
        "c",
    ))
    .await;
    h.wait_raw_processed(3).await;
    let evs = h.events().await;
    assert_eq!(
        evs.iter()
            .find(|e| e["dedupe_key"] == "msg:wamid.T3")
            .unwrap()["workspace_id"],
        "acme"
    );
}

#[tokio::test]
async fn usage_counts_sends_and_billable_statuses() {
    let Some(h) = Harness::start(&[]).await else {
        return;
    };
    h.post_webhook(&fixtures::text(
        PNID,
        CUSTOMER,
        "wamid.U1",
        now_secs(),
        "sold rice 500",
    ))
    .await;
    let rec = eventually("outbound recorded", || async {
        h.outbound("wamid.out.0").await
    })
    .await;
    assert_eq!(rec["kind"], "buttons");

    h.post_webhook(&fixtures::status(
        PNID,
        CUSTOMER,
        "wamid.out.0",
        "sent",
        now_secs(),
        None,
    ))
    .await;
    eventually("status applied", || async {
        (h.outbound("wamid.out.0").await.unwrap()["status"] == "sent").then_some(())
    })
    .await;

    let u = h.admin_get("/admin/usage").await;
    assert_eq!(u["sent_by_kind"]["buttons"], 1);
    assert_eq!(u["sent_by_phone_number_id"][PNID], 1);
    assert_eq!(u["billable_messages"], 1);
    assert_eq!(u["inbound_messages"], 1);
}

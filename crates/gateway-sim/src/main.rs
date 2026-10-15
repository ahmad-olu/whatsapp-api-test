//! gateway-sim: exercise the gateway without a phone.
//!
//! Every config value can come from a flag or the same env vars the server
//! uses, so `source .env && gateway-sim smoke` just works.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use gateway_core::{fixtures, signature};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "gateway-sim",
    about = "Simulate WhatsApp webhooks and drive the gateway admin API"
)]
struct Cli {
    /// Gateway base URL.
    #[arg(
        long,
        env = "GATEWAY_URL",
        default_value = "http://localhost:8080",
        global = true
    )]
    url: String,
    /// Meta app secret, used to sign simulated webhooks.
    #[arg(long, env = "WHATSAPP_APP_SECRET", global = true)]
    app_secret: Option<String>,
    #[arg(long, env = "WHATSAPP_VERIFY_TOKEN", global = true)]
    verify_token: Option<String>,
    #[arg(long, env = "ADMIN_TOKEN", global = true)]
    admin_token: Option<String>,
    /// Business phone number id the simulated messages are "sent to".
    #[arg(
        long,
        env = "WHATSAPP_PHONE_NUMBER_ID",
        default_value = "1000000001",
        global = true
    )]
    phone_number_id: String,
    /// Simulated customer number. With a real WhatsApp-connected gateway use
    /// a number on your Meta test recipient list, or the echo bot's replies
    /// will be rejected by Meta.
    #[arg(long, env = "SIM_FROM", default_value = "2348012345678", global = true)]
    from: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Send a simulated inbound webhook.
    Simulate {
        scenario: Scenario,
        /// Text for the `text` scenario.
        #[arg(long, default_value = "sold 5 bags rice 25000")]
        text: String,
        /// wamid for `status-sequence` (use a real outbound wamid to watch it change).
        #[arg(long)]
        wamid: Option<String>,
        /// Button id for the `button` scenario.
        #[arg(long, default_value = "no")]
        button_id: String,
    },
    /// Send a real outbound WhatsApp message through the gateway admin API.
    Send {
        #[command(subcommand)]
        what: SendCmd,
    },
    /// Read back what the gateway stored.
    Inspect {
        what: InspectWhat,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Re-run a stored raw webhook through processing.
    Replay { raw_id: i64 },
    /// Do Meta's GET verification handshake.
    Verify,
    /// End-to-end check of a running gateway (local or deployed).
    Smoke,
}

#[derive(Subcommand)]
enum SendCmd {
    Text {
        #[arg(long)]
        to: String,
        #[arg(long)]
        body: String,
    },
    Template {
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "hello_world")]
        name: String,
        #[arg(long, default_value = "en_US")]
        language: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Scenario {
    Text,
    Voice,
    Image,
    Button,
    /// The same text webhook delivered 3 times (Meta retries).
    Duplicate,
    /// read, delivered, sent -- in that (wrong) order.
    StatusSequence,
    /// Signed with the wrong secret: must be rejected (401).
    BadSignature,
    /// No signature header at all: must be rejected (401).
    Unsigned,
}

#[derive(Clone, Copy, ValueEnum)]
enum InspectWhat {
    Events,
    Raw,
    Outbound,
    Usage,
    Window,
}

enum Sig {
    Valid,
    Wrong,
    Absent,
}

struct Ctx {
    cli_url: String,
    http: reqwest::Client,
    app_secret: Option<String>,
    verify_token: Option<String>,
    admin_token: Option<String>,
    pnid: String,
    from: String,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn unique(prefix: &str) -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}{n}")
}

impl Ctx {
    fn secret(&self) -> Result<&str> {
        self.app_secret
            .as_deref()
            .context("WHATSAPP_APP_SECRET (or --app-secret) is required to sign webhooks")
    }

    fn admin(&self) -> Result<&str> {
        self.admin_token
            .as_deref()
            .context("ADMIN_TOKEN (or --admin-token) is required")
    }

    async fn webhook(&self, payload: &Value, sig: Sig) -> Result<u16> {
        let body = serde_json::to_vec(payload)?;
        let mut req = self
            .http
            .post(format!("{}/webhook", self.cli_url))
            .header("content-type", "application/json");
        match sig {
            Sig::Valid => {
                req = req.header(
                    "x-hub-signature-256",
                    signature::sign(self.secret()?, &body),
                )
            }
            Sig::Wrong => {
                req = req.header(
                    "x-hub-signature-256",
                    signature::sign("definitely-not-the-secret", &body),
                )
            }
            Sig::Absent => {}
        }
        Ok(req.body(body).send().await?.status().as_u16())
    }

    async fn admin_get(&self, path: &str) -> Result<Value> {
        let r = self
            .http
            .get(format!("{}{}", self.cli_url, path))
            .bearer_auth(self.admin()?)
            .send()
            .await?;
        let status = r.status();
        let text = r.text().await?;
        if !status.is_success() {
            bail!("GET {path} -> {status}: {text}");
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn verify(&self, token: &str) -> Result<(u16, String)> {
        let r = self
            .http
            .get(format!("{}/webhook", self.cli_url))
            .query(&[
                ("hub.mode", "subscribe"),
                ("hub.verify_token", token),
                ("hub.challenge", "sim-challenge-42"),
            ])
            .send()
            .await?;
        Ok((r.status().as_u16(), r.text().await?))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let ctx = Ctx {
        cli_url: cli.url.trim_end_matches('/').to_string(),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
        app_secret: cli.app_secret.clone(),
        verify_token: cli.verify_token.clone(),
        admin_token: cli.admin_token.clone(),
        pnid: cli.phone_number_id.clone(),
        from: cli.from.clone(),
    };

    match cli.cmd {
        Cmd::Simulate {
            scenario,
            text,
            wamid,
            button_id,
        } => simulate(&ctx, scenario, &text, wamid, &button_id).await,
        Cmd::Send { what } => send(&ctx, what).await,
        Cmd::Inspect { what, limit } => {
            let path = match what {
                InspectWhat::Events => format!("/admin/events?limit={limit}"),
                InspectWhat::Raw => format!("/admin/raw?limit={limit}"),
                InspectWhat::Outbound => format!("/admin/outbound?limit={limit}"),
                InspectWhat::Usage => "/admin/usage".to_string(),
                InspectWhat::Window => format!(
                    "/admin/window?customer={}&phone_number_id={}",
                    ctx.from, ctx.pnid
                ),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&ctx.admin_get(&path).await?)?
            );
            Ok(())
        }
        Cmd::Replay { raw_id } => {
            let r = ctx
                .http
                .post(format!("{}/admin/raw/{raw_id}/replay", ctx.cli_url))
                .bearer_auth(ctx.admin()?)
                .send()
                .await?;
            println!("{} {}", r.status(), r.text().await?);
            Ok(())
        }
        Cmd::Verify => {
            let token = ctx
                .verify_token
                .clone()
                .context("WHATSAPP_VERIFY_TOKEN (or --verify-token) required")?;
            let (status, body) = ctx.verify(&token).await?;
            println!("HTTP {status}, body {body:?} (expected 200 and \"sim-challenge-42\")");
            Ok(())
        }
        Cmd::Smoke => smoke(&ctx).await,
    }
}

async fn simulate(
    ctx: &Ctx,
    s: Scenario,
    text: &str,
    wamid: Option<String>,
    button_id: &str,
) -> Result<()> {
    let (p, f) = (ctx.pnid.as_str(), ctx.from.as_str());
    match s {
        Scenario::Text | Scenario::BadSignature | Scenario::Unsigned => {
            let payload = fixtures::text(p, f, &unique("wamid.SIM"), now(), text);
            let sig = match s {
                Scenario::BadSignature => Sig::Wrong,
                Scenario::Unsigned => Sig::Absent,
                _ => Sig::Valid,
            };
            println!("-> HTTP {}", ctx.webhook(&payload, sig).await?);
        }
        Scenario::Voice => {
            let payload =
                fixtures::voice_note(p, f, &unique("wamid.SIM"), now(), &unique("SIMMEDIA"));
            println!(
                "-> HTTP {} (media id is fake, so the media download will fail upstream)",
                ctx.webhook(&payload, Sig::Valid).await?
            );
        }
        Scenario::Image => {
            let payload = fixtures::image(
                p,
                f,
                &unique("wamid.SIM"),
                now(),
                &unique("SIMMEDIA"),
                Some("receipt"),
            );
            println!(
                "-> HTTP {} (media id is fake, so the media download will fail upstream)",
                ctx.webhook(&payload, Sig::Valid).await?
            );
        }
        Scenario::Button => {
            let payload =
                fixtures::button_reply(p, f, &unique("wamid.SIM"), now(), button_id, "Tapped");
            println!("-> HTTP {}", ctx.webhook(&payload, Sig::Valid).await?);
        }
        Scenario::Duplicate => {
            let payload = fixtures::text(p, f, &unique("wamid.SIMDUP"), now(), text);
            for i in 1..=3 {
                println!(
                    "delivery {i} -> HTTP {}",
                    ctx.webhook(&payload, Sig::Valid).await?
                );
            }
            println!("expect exactly ONE event for this wamid: gateway-sim inspect events");
        }
        Scenario::StatusSequence => {
            let id = wamid.unwrap_or_else(|| unique("wamid.SIMOUT"));
            for st in ["read", "delivered", "sent"] {
                println!(
                    "{st} -> HTTP {}",
                    ctx.webhook(&fixtures::status(p, f, &id, st, now(), None), Sig::Valid)
                        .await?
                );
            }
            println!("expect final status `read` for {id}: gateway-sim inspect outbound");
        }
    }
    Ok(())
}

async fn send(ctx: &Ctx, what: SendCmd) -> Result<()> {
    let message = match what {
        SendCmd::Text { to, body } => json!({ "type": "text", "to": to, "body": body }),
        SendCmd::Template { to, name, language } => {
            json!({ "type": "template", "to": to, "name": name, "language": language })
        }
    };
    let r = ctx
        .http
        .post(format!("{}/admin/send", ctx.cli_url))
        .bearer_auth(ctx.admin()?)
        .json(&json!({ "phone_number_id": ctx.pnid, "message": message }))
        .send()
        .await?;
    let status = r.status();
    println!("HTTP {status}\n{}", r.text().await?);
    Ok(())
}

/// Poll `f` until it yields `Some`, for up to `secs` seconds.
async fn wait_for<T, F, Fut>(secs: u64, mut f: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..(secs * 4) {
        if let Some(v) = f().await {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    None
}

async fn smoke(ctx: &Ctx) -> Result<()> {
    let mut failures = 0u32;
    let mut check = |name: &str, ok: bool, detail: String| {
        if ok {
            println!("PASS  {name}");
        } else {
            failures += 1;
            println!("FAIL  {name}  ({detail})");
        }
    };

    let h = ctx
        .http
        .get(format!("{}/healthz", ctx.cli_url))
        .send()
        .await?
        .status()
        .as_u16();
    check("GET /healthz", h == 200, format!("HTTP {h}"));
    let r = ctx
        .http
        .get(format!("{}/readyz", ctx.cli_url))
        .send()
        .await?
        .status()
        .as_u16();
    check(
        "GET /readyz (database reachable)",
        r == 200,
        format!("HTTP {r}"),
    );

    if let Some(t) = &ctx.verify_token {
        let (s, b) = ctx.verify(t).await?;
        check(
            "webhook handshake with correct verify token",
            s == 200 && b == "sim-challenge-42",
            format!("HTTP {s} body {b:?}"),
        );
        let (s, _) = ctx.verify("wrong-token").await?;
        check(
            "webhook handshake rejects wrong token",
            s == 403,
            format!("HTTP {s}"),
        );
    } else {
        println!("SKIP  handshake checks (no WHATSAPP_VERIFY_TOKEN)");
    }

    if ctx.app_secret.is_none() {
        println!("SKIP  signed webhook checks (no WHATSAPP_APP_SECRET)");
        return finish(failures);
    }

    let probe = fixtures::text(&ctx.pnid, &ctx.from, "wamid.SMOKE_UNSIGNED", now(), "x");
    let s = ctx.webhook(&probe, Sig::Absent).await?;
    check(
        "unsigned webhook rejected",
        s == 401,
        format!("HTTP {s} (is INSECURE_SKIP_SIGNATURE on?)"),
    );
    let s = ctx.webhook(&probe, Sig::Wrong).await?;
    check(
        "wrongly-signed webhook rejected",
        s == 401,
        format!("HTTP {s}"),
    );

    let wamid = unique("wamid.SMOKE");
    let payload = fixtures::text(&ctx.pnid, &ctx.from, &wamid, now(), "sold smoke test 100");
    let s = ctx.webhook(&payload, Sig::Valid).await?;
    check("signed webhook accepted", s == 200, format!("HTTP {s}"));

    if ctx.admin_token.is_none() {
        println!("SKIP  stored-event checks (no ADMIN_TOKEN)");
        return finish(failures);
    }

    let key = format!("msg:{wamid}");
    let found = wait_for(15, || async {
        ctx.admin_get("/admin/events?limit=200")
            .await
            .ok()
            .and_then(|v| {
                v.as_array()?
                    .iter()
                    .find(|e| e["dedupe_key"] == key.as_str())
                    .cloned()
            })
    })
    .await;
    check(
        "event stored after webhook",
        found.is_some(),
        "not found within 15s".into(),
    );

    ctx.webhook(&payload, Sig::Valid).await?;
    ctx.webhook(&payload, Sig::Valid).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let count = ctx
        .admin_get("/admin/events?limit=200")
        .await?
        .as_array()
        .map(|a| a.iter().filter(|e| e["dedupe_key"] == key.as_str()).count())
        .unwrap_or(0);
    check(
        "duplicate deliveries collapse to one event",
        count == 1,
        format!("{count} events"),
    );

    let out = unique("wamid.SMOKEOUT");
    for st in ["read", "delivered", "sent"] {
        ctx.webhook(
            &fixtures::status(&ctx.pnid, &ctx.from, &out, st, now(), None),
            Sig::Valid,
        )
        .await?;
    }
    let status = wait_for(15, || async {
        let v = ctx.admin_get("/admin/outbound?limit=200").await.ok()?;
        let row = v
            .as_array()?
            .iter()
            .find(|m| m["wamid"] == out.as_str())?
            .clone();
        (row["status"] == "read").then_some(())
    })
    .await;
    check(
        "out-of-order statuses settle on `read`",
        status.is_some(),
        "status never reached read".into(),
    );

    println!(
        "\nNote: with SINK_MODE=echo the bot also tried to reply to {} via the real Graph API.\n\
         Check `gateway-sim inspect events` -- `forward_error` shows why a reply failed\n\
         (e.g. Meta rejects numbers not on your test recipient list).",
        ctx.from
    );
    finish(failures)
}

fn finish(failures: u32) -> Result<()> {
    if failures == 0 {
        println!("\nAll checks passed.");
        Ok(())
    } else {
        bail!("{failures} check(s) failed")
    }
}

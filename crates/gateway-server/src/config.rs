//! Environment-driven configuration. Secrets are never logged.

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkMode {
    /// Demo bookkeeping bot replies via WhatsApp.
    Echo,
    /// POST every event (HMAC-signed) to `FORWARD_URL` (your main app).
    Forward,
    /// Just log. Useful for pure webhook-delivery testing.
    Log,
}

pub struct Config {
    pub database_url: String,
    pub port: u16,
    pub verify_token: String,
    pub app_secret: Option<String>,
    pub insecure_skip_signature: bool,
    pub access_token: Option<String>,
    pub default_phone_number_id: Option<String>,
    pub graph_base_url: String,
    pub graph_version: String,
    pub admin_token: String,
    pub sink: SinkMode,
    pub forward_url: Option<String>,
    pub forward_secret: Option<String>,
    pub auto_mark_read: bool,
    pub max_body_bytes: usize,
    pub media_max_bytes: usize,
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let req = |k: &str| get(k).with_context(|| format!("missing required env var {k}"));

        let insecure_skip_signature = get("INSECURE_SKIP_SIGNATURE")
            .map(|v| truthy(&v))
            .unwrap_or(false);
        let app_secret = get("WHATSAPP_APP_SECRET");
        if app_secret.is_none() && !insecure_skip_signature {
            bail!(
                "missing required env var WHATSAPP_APP_SECRET \
                 (set INSECURE_SKIP_SIGNATURE=true only for throwaway local testing)"
            );
        }

        let sink = match get("SINK_MODE")
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("echo") => SinkMode::Echo,
            Some("forward") => SinkMode::Forward,
            Some("log") => SinkMode::Log,
            Some(other) => bail!("SINK_MODE must be echo|forward|log, got {other:?}"),
        };
        let forward_url = get("FORWARD_URL");
        let forward_secret = get("FORWARD_SECRET");
        if sink == SinkMode::Forward && (forward_url.is_none() || forward_secret.is_none()) {
            bail!("SINK_MODE=forward requires FORWARD_URL and FORWARD_SECRET");
        }

        Ok(Self {
            database_url: req("DATABASE_URL")?,
            port: get("PORT")
                .map(|p| p.parse().context("PORT must be a number"))
                .transpose()?
                .unwrap_or(8080),
            verify_token: req("WHATSAPP_VERIFY_TOKEN")?,
            app_secret,
            insecure_skip_signature,
            access_token: get("WHATSAPP_ACCESS_TOKEN"),
            default_phone_number_id: get("WHATSAPP_PHONE_NUMBER_ID"),
            graph_base_url: get("GRAPH_BASE_URL")
                .unwrap_or_else(|| "https://graph.facebook.com".into())
                .trim_end_matches('/')
                .to_string(),
            graph_version: get("GRAPH_API_VERSION").unwrap_or_else(|| "v26.0".into()),
            admin_token: req("ADMIN_TOKEN")?,
            sink,
            forward_url,
            forward_secret,
            auto_mark_read: get("AUTO_MARK_READ").map(|v| truthy(&v)).unwrap_or(true),
            max_body_bytes: get("MAX_BODY_BYTES")
                .map(|v| v.parse().context("MAX_BODY_BYTES must be a number"))
                .transpose()?
                .unwrap_or(1024 * 1024),
            media_max_bytes: get("MEDIA_MAX_BYTES")
                .map(|v| v.parse().context("MEDIA_MAX_BYTES must be a number"))
                .transpose()?
                .unwrap_or(16 * 1024 * 1024),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| m.get(k).cloned())
    }

    const BASE: &[(&str, &str)] = &[
        ("DATABASE_URL", "postgres://x"),
        ("WHATSAPP_VERIFY_TOKEN", "vt"),
        ("WHATSAPP_APP_SECRET", "s"),
        ("ADMIN_TOKEN", "a"),
    ];

    #[test]
    fn defaults() {
        let c = cfg(BASE).unwrap();
        assert_eq!(c.port, 8080);
        assert_eq!(c.sink, SinkMode::Echo);
        assert_eq!(c.graph_version, "v26.0");
        assert!(c.auto_mark_read);
    }

    #[test]
    fn requires_app_secret_unless_insecure() {
        let no_secret: Vec<_> = BASE
            .iter()
            .filter(|(k, _)| *k != "WHATSAPP_APP_SECRET")
            .cloned()
            .collect();
        assert!(cfg(&no_secret).is_err());
        let mut ok = no_secret.clone();
        ok.push(("INSECURE_SKIP_SIGNATURE", "true"));
        assert!(cfg(&ok).is_ok());
    }

    #[test]
    fn forward_mode_needs_url_and_secret() {
        let mut p = BASE.to_vec();
        p.push(("SINK_MODE", "forward"));
        assert!(cfg(&p).is_err());
        p.push(("FORWARD_URL", "https://app/hook"));
        p.push(("FORWARD_SECRET", "x"));
        assert_eq!(cfg(&p).unwrap().sink, SinkMode::Forward);
    }

    #[test]
    fn bad_sink_mode() {
        let mut p = BASE.to_vec();
        p.push(("SINK_MODE", "nope"));
        assert!(cfg(&p).is_err());
    }
}

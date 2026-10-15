//! Outbound transport to the WhatsApp Graph API.
//!
//! Deliberately a thin, hand-written client (a handful of endpoints) behind a
//! trait, so tests use a fake, and a provider SDK or BSP could be swapped in.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("graph api error (http {status}, code {code:?}): {message}")]
    Api {
        status: u16,
        code: Option<i64>,
        message: String,
    },
    #[error("network error: {0}")]
    Network(String),
    #[error("not configured: {0}")]
    NotConfigured(&'static str),
    #[error("media larger than limit ({0} bytes)")]
    TooLarge(usize),
}

impl TransportError {
    pub fn is_retryable(&self) -> bool {
        match self {
            TransportError::Network(_) => true,
            TransportError::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SendResponse {
    /// wamid of the created message (absent for mark-as-read).
    pub wamid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MediaMeta {
    pub url: String,
    pub mime_type: Option<String>,
    pub sha256: Option<String>,
    pub file_size: Option<i64>,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(
        &self,
        phone_number_id: &str,
        body: &Value,
    ) -> Result<SendResponse, TransportError>;
    async fn media_meta(&self, media_id: &str) -> Result<MediaMeta, TransportError>;
    async fn download(&self, url: &str) -> Result<Vec<u8>, TransportError>;
}

pub struct GraphTransport {
    client: reqwest::Client,
    base: String,
    version: String,
    token: Option<String>,
    media_max_bytes: usize,
}

impl GraphTransport {
    pub fn new(
        base: impl Into<String>,
        version: impl Into<String>,
        token: Option<String>,
        media_max_bytes: usize,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("whatsapp-gateway/0.1")
            .build()
            .expect("reqwest client");
        Self {
            client,
            base: base.into().trim_end_matches('/').to_string(),
            version: version.into(),
            token,
            media_max_bytes,
        }
    }

    fn token(&self) -> Result<&str, TransportError> {
        self.token
            .as_deref()
            .ok_or(TransportError::NotConfigured("WHATSAPP_ACCESS_TOKEN"))
    }
}

fn net(e: reqwest::Error) -> TransportError {
    TransportError::Network(e.without_url().to_string())
}

fn api_error(status: u16, text: &str) -> TransportError {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let err = &v["error"];
    TransportError::Api {
        status,
        code: err["code"].as_i64(),
        message: err["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| text.chars().take(300).collect()),
    }
}

#[async_trait]
impl Transport for GraphTransport {
    async fn send(
        &self,
        phone_number_id: &str,
        body: &Value,
    ) -> Result<SendResponse, TransportError> {
        let token = self.token()?;
        let url = format!(
            "{}/{}/{}/messages",
            self.base, self.version, phone_number_id
        );
        let resp = self
            .client
            .post(url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(net)?;
        let status = resp.status();
        let text = resp.text().await.map_err(net)?;
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text));
        }
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(SendResponse {
            wamid: v["messages"][0]["id"].as_str().map(str::to_string),
        })
    }

    async fn media_meta(&self, media_id: &str) -> Result<MediaMeta, TransportError> {
        let token = self.token()?;
        let url = format!("{}/{}/{}", self.base, self.version, media_id);
        let resp = self
            .client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(net)?;
        let status = resp.status();
        let text = resp.text().await.map_err(net)?;
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &text));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| TransportError::Network(format!("bad media metadata: {e}")))?;
        let url = v["url"]
            .as_str()
            .ok_or_else(|| TransportError::Network("media metadata missing url".into()))?
            .to_string();
        Ok(MediaMeta {
            url,
            mime_type: v["mime_type"].as_str().map(str::to_string),
            sha256: v["sha256"].as_str().map(str::to_string),
            file_size: v["file_size"].as_i64(),
        })
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, TransportError> {
        let token = self.token()?;
        let resp = self
            .client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(net)?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(api_error(status.as_u16(), &text));
        }
        if let Some(len) = resp.content_length() {
            if len as usize > self.media_max_bytes {
                return Err(TransportError::TooLarge(len as usize));
            }
        }
        let bytes = resp.bytes().await.map_err(net)?;
        if bytes.len() > self.media_max_bytes {
            return Err(TransportError::TooLarge(bytes.len()));
        }
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_graph_error_body() {
        let e = api_error(
            400,
            r#"{"error":{"message":"(#131047) Re-engagement message","type":"OAuthException","code":131047}}"#,
        );
        match e {
            TransportError::Api {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 400);
                assert_eq!(code, Some(131047));
                assert!(message.contains("Re-engagement"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn retryability() {
        assert!(TransportError::Network("x".into()).is_retryable());
        assert!(api_error(503, "").is_retryable());
        assert!(api_error(429, "").is_retryable());
        assert!(!api_error(400, "").is_retryable());
        assert!(!TransportError::NotConfigured("x").is_retryable());
    }
}

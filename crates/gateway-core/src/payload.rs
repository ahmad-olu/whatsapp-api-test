//! Raw webhook payload types, mirroring what Meta sends.
//!
//! Everything is `#[serde(default)]` on purpose: Meta adds fields and message
//! types over time, and a webhook that fails to parse is a webhook that gets
//! retried. Unknown content ends up as `Content::Unsupported`, never a parse
//! error.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Meta sends timestamps as strings ("1700000000"); accept numbers too.
fn de_ts<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::String(s) => s.parse().unwrap_or(0),
        Value::Number(n) => n.as_i64().unwrap_or(0),
        _ => 0,
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WebhookPayload {
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub entry: Vec<Entry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Entry {
    /// WhatsApp Business Account (WABA) id.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Change {
    #[serde(default)]
    pub field: String,
    #[serde(default)]
    pub value: ChangeValue,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChangeValue {
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub contacts: Vec<Contact>,
    #[serde(default)]
    pub messages: Vec<RawMessage>,
    #[serde(default)]
    pub statuses: Vec<RawStatus>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub display_phone_number: String,
    #[serde(default)]
    pub phone_number_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Contact {
    #[serde(default)]
    pub wa_id: String,
    #[serde(default)]
    pub profile: Option<Profile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawMessage {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub id: String,
    #[serde(default, deserialize_with = "de_ts")]
    pub timestamp: i64,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<TextBody>,
    #[serde(default)]
    pub image: Option<MediaBody>,
    #[serde(default)]
    pub audio: Option<MediaBody>,
    #[serde(default)]
    pub video: Option<MediaBody>,
    #[serde(default)]
    pub document: Option<MediaBody>,
    #[serde(default)]
    pub sticker: Option<MediaBody>,
    #[serde(default)]
    pub location: Option<LocationBody>,
    #[serde(default)]
    pub interactive: Option<Interactive>,
    /// Quick-reply button tapped on a *template* message.
    #[serde(default)]
    pub button: Option<TemplateButton>,
    #[serde(default)]
    pub reaction: Option<Reaction>,
    /// Present when the user replied to (quoted) a specific message.
    #[serde(default)]
    pub context: Option<MessageContext>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TextBody {
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaBody {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    /// `true` for voice notes (push-to-talk), absent/false for audio files.
    #[serde(default)]
    pub voice: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LocationBody {
    #[serde(default)]
    pub latitude: f64,
    #[serde(default)]
    pub longitude: f64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Interactive {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub button_reply: Option<Reply>,
    #[serde(default)]
    pub list_reply: Option<Reply>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reply {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TemplateButton {
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reaction {
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub emoji: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageContext {
    /// wamid of the message being replied to.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawStatus {
    /// wamid of the outbound message this status is about.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, deserialize_with = "de_ts")]
    pub timestamp: i64,
    #[serde(default)]
    pub recipient_id: String,
    #[serde(default)]
    pub pricing: Option<Pricing>,
    #[serde(default)]
    pub errors: Vec<RawError>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pricing {
    #[serde(default)]
    pub billable: Option<bool>,
    #[serde(default)]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

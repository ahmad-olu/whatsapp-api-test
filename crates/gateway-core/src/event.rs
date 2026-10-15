//! The normalized event model: the *only* shape the rest of the system (and
//! eventually your main app) sees. Provider payload quirks stop at
//! [`crate::normalize`].

use serde::{Deserialize, Serialize};

use crate::status::DeliveryStatus;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Message(InboundMessage),
    Status(StatusUpdate),
}

impl Event {
    /// Idempotency key. Meta delivers at-least-once, so the same key must
    /// always be treated as the same event.
    pub fn dedupe_key(&self) -> String {
        match self {
            Event::Message(m) => format!("msg:{}", m.id),
            Event::Status(s) => format!("st:{}:{}", s.id, s.status.as_str()),
        }
    }

    pub fn phone_number_id(&self) -> &str {
        match self {
            Event::Message(m) => &m.phone_number_id,
            Event::Status(s) => &s.phone_number_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// wamid
    pub id: String,
    /// Business phone number id that received it (used for tenant routing).
    pub phone_number_id: String,
    /// Customer's WhatsApp id / phone number, digits only.
    pub from: String,
    pub profile_name: Option<String>,
    /// Unix seconds, as stamped by WhatsApp.
    pub timestamp: i64,
    /// wamid of the message this one quotes, if any.
    pub reply_to: Option<String>,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub media_id: String,
    pub mime_type: Option<String>,
    pub sha256: Option<String>,
    pub caption: Option<String>,
    pub filename: Option<String>,
    /// True for push-to-talk voice notes.
    pub voice: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        body: String,
    },
    Image(MediaInfo),
    Audio(MediaInfo),
    Video(MediaInfo),
    Document(MediaInfo),
    Sticker(MediaInfo),
    Location {
        latitude: f64,
        longitude: f64,
        name: Option<String>,
        address: Option<String>,
    },
    ButtonReply {
        id: String,
        title: String,
    },
    ListReply {
        id: String,
        title: String,
        description: Option<String>,
    },
    Reaction {
        message_id: String,
        emoji: Option<String>,
    },
    /// A type we do not model (yet). Kept so nothing is silently lost.
    Unsupported {
        raw_type: String,
    },
}

impl Content {
    /// Media id, if this content carries downloadable media.
    pub fn media(&self) -> Option<&MediaInfo> {
        match self {
            Content::Image(m)
            | Content::Audio(m)
            | Content::Video(m)
            | Content::Document(m)
            | Content::Sticker(m) => Some(m),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusUpdate {
    /// wamid of the outbound message.
    pub id: String,
    pub phone_number_id: String,
    pub recipient: String,
    pub status: DeliveryStatus,
    pub timestamp: i64,
    pub billable: Option<bool>,
    pub pricing_category: Option<String>,
    pub error: Option<StatusError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusError {
    pub code: i64,
    pub title: Option<String>,
    pub message: Option<String>,
}

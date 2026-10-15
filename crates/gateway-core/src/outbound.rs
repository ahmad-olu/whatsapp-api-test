//! Outbound message builders: typed message -> Graph API `/messages` body.
//!
//! Validation mirrors WhatsApp's documented limits so mistakes fail locally
//! with a clear error instead of as an opaque 400 from Meta.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_TEXT: usize = 4096;
pub const MAX_INTERACTIVE_BODY: usize = 1024;
pub const MAX_BUTTONS: usize = 3;
pub const MAX_BUTTON_TITLE: usize = 20;
pub const MAX_BUTTON_ID: usize = 256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OutboundError {
    #[error("recipient is empty")]
    EmptyRecipient,
    #[error("body is empty")]
    EmptyBody,
    #[error("body too long ({len} > {max})")]
    BodyTooLong { len: usize, max: usize },
    #[error("buttons: need 1..={MAX_BUTTONS}, got {0}")]
    ButtonCount(usize),
    #[error("button title must be 1..={MAX_BUTTON_TITLE} chars: {0:?}")]
    ButtonTitle(String),
    #[error("button id must be 1..={MAX_BUTTON_ID} chars")]
    ButtonId,
    #[error("template name/language is empty")]
    TemplateName,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Button {
    pub id: String,
    pub title: String,
}

/// Reference to media: an uploaded media id, or a public https link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaRef {
    Id(String),
    Link(String),
}

impl MediaRef {
    fn to_json(&self) -> (&'static str, &str) {
        match self {
            MediaRef::Id(s) => ("id", s),
            MediaRef::Link(s) => ("link", s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outbound {
    Text {
        to: String,
        body: String,
        #[serde(default)]
        preview_url: bool,
    },
    Buttons {
        to: String,
        body: String,
        buttons: Vec<Button>,
    },
    Template {
        to: String,
        name: String,
        language: String,
        #[serde(default)]
        body_params: Vec<String>,
    },
    Document {
        to: String,
        media: MediaRef,
        filename: Option<String>,
        caption: Option<String>,
    },
    Image {
        to: String,
        media: MediaRef,
        caption: Option<String>,
    },
    /// Mark an inbound message as read (blue ticks).
    MarkRead { message_id: String },
}

impl Outbound {
    /// Does Meta require an open 24h window to send this? Templates and
    /// read-receipts do not.
    pub fn requires_open_window(&self) -> bool {
        !matches!(self, Outbound::Template { .. } | Outbound::MarkRead { .. })
    }

    pub fn recipient(&self) -> Option<&str> {
        match self {
            Outbound::Text { to, .. }
            | Outbound::Buttons { to, .. }
            | Outbound::Template { to, .. }
            | Outbound::Document { to, .. }
            | Outbound::Image { to, .. } => Some(to),
            Outbound::MarkRead { .. } => None,
        }
    }

    /// Short label for usage metering / logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Outbound::Text { .. } => "text",
            Outbound::Buttons { .. } => "buttons",
            Outbound::Template { .. } => "template",
            Outbound::Document { .. } => "document",
            Outbound::Image { .. } => "image",
            Outbound::MarkRead { .. } => "mark_read",
        }
    }

    pub fn validate(&self) -> Result<(), OutboundError> {
        if let Some(to) = self.recipient() {
            if to.trim().is_empty() {
                return Err(OutboundError::EmptyRecipient);
            }
        }
        match self {
            Outbound::Text { body, .. } => check_body(body, MAX_TEXT),
            Outbound::Buttons { body, buttons, .. } => {
                check_body(body, MAX_INTERACTIVE_BODY)?;
                if buttons.is_empty() || buttons.len() > MAX_BUTTONS {
                    return Err(OutboundError::ButtonCount(buttons.len()));
                }
                for b in buttons {
                    let t = b.title.chars().count();
                    if t == 0 || t > MAX_BUTTON_TITLE {
                        return Err(OutboundError::ButtonTitle(b.title.clone()));
                    }
                    let i = b.id.chars().count();
                    if i == 0 || i > MAX_BUTTON_ID {
                        return Err(OutboundError::ButtonId);
                    }
                }
                Ok(())
            }
            Outbound::Template { name, language, .. } => {
                if name.trim().is_empty() || language.trim().is_empty() {
                    Err(OutboundError::TemplateName)
                } else {
                    Ok(())
                }
            }
            Outbound::Document { .. } | Outbound::Image { .. } | Outbound::MarkRead { .. } => {
                Ok(())
            }
        }
    }

    /// Build the JSON body for `POST /{phone_number_id}/messages`.
    pub fn to_json(&self) -> Result<Value, OutboundError> {
        self.validate()?;
        Ok(match self {
            Outbound::Text {
                to,
                body,
                preview_url,
            } => json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "text",
                "text": { "body": body, "preview_url": preview_url }
            }),
            Outbound::Buttons { to, body, buttons } => json!({
                "messaging_product": "whatsapp",
                "recipient_type": "individual",
                "to": to,
                "type": "interactive",
                "interactive": {
                    "type": "button",
                    "body": { "text": body },
                    "action": {
                        "buttons": buttons.iter().map(|b| json!({
                            "type": "reply",
                            "reply": { "id": b.id, "title": b.title }
                        })).collect::<Vec<_>>()
                    }
                }
            }),
            Outbound::Template {
                to,
                name,
                language,
                body_params,
            } => {
                let mut template = json!({ "name": name, "language": { "code": language } });
                if !body_params.is_empty() {
                    template["components"] = json!([{
                        "type": "body",
                        "parameters": body_params.iter()
                            .map(|p| json!({ "type": "text", "text": p }))
                            .collect::<Vec<_>>()
                    }]);
                }
                json!({
                    "messaging_product": "whatsapp",
                    "recipient_type": "individual",
                    "to": to,
                    "type": "template",
                    "template": template
                })
            }
            Outbound::Document {
                to,
                media,
                filename,
                caption,
            } => {
                let (k, v) = media.to_json();
                let mut doc = json!({ k: v });
                if let Some(f) = filename {
                    doc["filename"] = json!(f);
                }
                if let Some(c) = caption {
                    doc["caption"] = json!(c);
                }
                json!({
                    "messaging_product": "whatsapp",
                    "recipient_type": "individual",
                    "to": to,
                    "type": "document",
                    "document": doc
                })
            }
            Outbound::Image { to, media, caption } => {
                let (k, v) = media.to_json();
                let mut img = json!({ k: v });
                if let Some(c) = caption {
                    img["caption"] = json!(c);
                }
                json!({
                    "messaging_product": "whatsapp",
                    "recipient_type": "individual",
                    "to": to,
                    "type": "image",
                    "image": img
                })
            }
            Outbound::MarkRead { message_id } => json!({
                "messaging_product": "whatsapp",
                "status": "read",
                "message_id": message_id
            }),
        })
    }
}

fn check_body(body: &str, max: usize) -> Result<(), OutboundError> {
    let len = body.chars().count();
    if body.trim().is_empty() {
        Err(OutboundError::EmptyBody)
    } else if len > max {
        Err(OutboundError::BodyTooLong { len, max })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(body: &str) -> Outbound {
        Outbound::Text {
            to: "2348012345678".into(),
            body: body.into(),
            preview_url: false,
        }
    }

    #[test]
    fn text_json_shape() {
        let v = text("hello").to_json().unwrap();
        assert_eq!(v["messaging_product"], "whatsapp");
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"]["body"], "hello");
        assert_eq!(v["to"], "2348012345678");
    }

    #[test]
    fn text_validation() {
        assert_eq!(text("  ").validate(), Err(OutboundError::EmptyBody));
        let long = "a".repeat(MAX_TEXT + 1);
        assert!(matches!(
            text(&long).validate(),
            Err(OutboundError::BodyTooLong { .. })
        ));
        let ok = "a".repeat(MAX_TEXT);
        assert!(text(&ok).validate().is_ok());
        let no_to = Outbound::Text {
            to: " ".into(),
            body: "x".into(),
            preview_url: false,
        };
        assert_eq!(no_to.validate(), Err(OutboundError::EmptyRecipient));
    }

    fn buttons(n: usize, title: &str) -> Outbound {
        Outbound::Buttons {
            to: "234".into(),
            body: "Confirm?".into(),
            buttons: (0..n)
                .map(|i| Button {
                    id: format!("b{i}"),
                    title: title.into(),
                })
                .collect(),
        }
    }

    #[test]
    fn button_limits() {
        assert!(buttons(1, "Yes").validate().is_ok());
        assert!(buttons(3, "Yes").validate().is_ok());
        assert_eq!(
            buttons(0, "Yes").validate(),
            Err(OutboundError::ButtonCount(0))
        );
        assert_eq!(
            buttons(4, "Yes").validate(),
            Err(OutboundError::ButtonCount(4))
        );
        assert!(matches!(
            buttons(1, "this title is way too long").validate(),
            Err(OutboundError::ButtonTitle(_))
        ));
        assert!(matches!(
            buttons(1, "").validate(),
            Err(OutboundError::ButtonTitle(_))
        ));
    }

    #[test]
    fn buttons_json_shape() {
        let v = buttons(2, "Yes").to_json().unwrap();
        assert_eq!(v["type"], "interactive");
        assert_eq!(v["interactive"]["type"], "button");
        let arr = v["interactive"]["action"]["buttons"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["reply"]["id"], "b0");
        assert_eq!(arr[0]["type"], "reply");
    }

    #[test]
    fn template_json_with_and_without_params() {
        let t = Outbound::Template {
            to: "234".into(),
            name: "hello_world".into(),
            language: "en_US".into(),
            body_params: vec![],
        };
        let v = t.to_json().unwrap();
        assert_eq!(v["template"]["name"], "hello_world");
        assert_eq!(v["template"]["language"]["code"], "en_US");
        assert!(v["template"].get("components").is_none());

        let t = Outbound::Template {
            to: "234".into(),
            name: "receipt".into(),
            language: "en".into(),
            body_params: vec!["Ada".into(), "₦5,000".into()],
        };
        let v = t.to_json().unwrap();
        assert_eq!(
            v["template"]["components"][0]["parameters"][1]["text"],
            "₦5,000"
        );
    }

    #[test]
    fn document_and_mark_read() {
        let d = Outbound::Document {
            to: "234".into(),
            media: MediaRef::Link("https://example.com/inv.pdf".into()),
            filename: Some("inv.pdf".into()),
            caption: None,
        };
        let v = d.to_json().unwrap();
        assert_eq!(v["document"]["link"], "https://example.com/inv.pdf");
        assert_eq!(v["document"]["filename"], "inv.pdf");

        let r = Outbound::MarkRead {
            message_id: "wamid.X".into(),
        };
        let v = r.to_json().unwrap();
        assert_eq!(v["status"], "read");
        assert_eq!(v["message_id"], "wamid.X");
    }

    #[test]
    fn window_requirements() {
        assert!(text("x").requires_open_window());
        assert!(buttons(1, "Y").requires_open_window());
        let t = Outbound::Template {
            to: "1".into(),
            name: "n".into(),
            language: "en".into(),
            body_params: vec![],
        };
        assert!(!t.requires_open_window());
        assert!(!Outbound::MarkRead {
            message_id: "x".into()
        }
        .requires_open_window());
    }
}

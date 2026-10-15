//! Meta payload -> normalized events.

use crate::event::{Content, Event, InboundMessage, MediaInfo, StatusError, StatusUpdate};
use crate::payload::{MediaBody, RawMessage, WebhookPayload};
use crate::status::DeliveryStatus;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Normalized {
    pub events: Vec<Event>,
    /// Entries that could not be turned into events (missing ids, unknown
    /// status strings). Counted, never fatal.
    pub skipped: usize,
}

/// Parse raw bytes and normalize. Parse failure is the only error.
pub fn normalize_bytes(body: &[u8]) -> Result<Normalized, serde_json::Error> {
    let payload: WebhookPayload = serde_json::from_slice(body)?;
    Ok(normalize(&payload))
}

pub fn normalize(payload: &WebhookPayload) -> Normalized {
    let mut out = Normalized::default();

    for entry in &payload.entry {
        for change in &entry.changes {
            let v = &change.value;
            let pnid = &v.metadata.phone_number_id;

            for m in &v.messages {
                if m.id.is_empty() || m.from.is_empty() {
                    out.skipped += 1;
                    continue;
                }
                let profile_name = v
                    .contacts
                    .iter()
                    .find(|c| c.wa_id == m.from)
                    .and_then(|c| c.profile.as_ref())
                    .and_then(|p| p.name.clone());

                out.events.push(Event::Message(InboundMessage {
                    id: m.id.clone(),
                    phone_number_id: pnid.clone(),
                    from: m.from.clone(),
                    profile_name,
                    timestamp: m.timestamp,
                    reply_to: m.context.as_ref().and_then(|c| c.id.clone()),
                    content: content_of(m),
                }));
            }

            for s in &v.statuses {
                match DeliveryStatus::parse(&s.status) {
                    Some(status) if !s.id.is_empty() => {
                        out.events.push(Event::Status(StatusUpdate {
                            id: s.id.clone(),
                            phone_number_id: pnid.clone(),
                            recipient: s.recipient_id.clone(),
                            status,
                            timestamp: s.timestamp,
                            billable: s.pricing.as_ref().and_then(|p| p.billable),
                            pricing_category: s.pricing.as_ref().and_then(|p| p.category.clone()),
                            error: s.errors.first().map(|e| StatusError {
                                code: e.code,
                                title: e.title.clone(),
                                message: e.message.clone(),
                            }),
                        }));
                    }
                    _ => out.skipped += 1,
                }
            }
        }
    }
    out
}

fn media(b: &MediaBody) -> MediaInfo {
    MediaInfo {
        media_id: b.id.clone(),
        mime_type: b.mime_type.clone(),
        sha256: b.sha256.clone(),
        caption: b.caption.clone(),
        filename: b.filename.clone(),
        voice: b.voice.unwrap_or(false),
    }
}

fn content_of(m: &RawMessage) -> Content {
    let unsupported = || Content::Unsupported {
        raw_type: m.kind.clone(),
    };
    match m.kind.as_str() {
        "text" => m
            .text
            .as_ref()
            .map(|t| Content::Text {
                body: t.body.clone(),
            })
            .unwrap_or_else(unsupported),
        "image" => m
            .image
            .as_ref()
            .map(|b| Content::Image(media(b)))
            .unwrap_or_else(unsupported),
        "audio" => m
            .audio
            .as_ref()
            .map(|b| Content::Audio(media(b)))
            .unwrap_or_else(unsupported),
        "video" => m
            .video
            .as_ref()
            .map(|b| Content::Video(media(b)))
            .unwrap_or_else(unsupported),
        "document" => m
            .document
            .as_ref()
            .map(|b| Content::Document(media(b)))
            .unwrap_or_else(unsupported),
        "sticker" => m
            .sticker
            .as_ref()
            .map(|b| Content::Sticker(media(b)))
            .unwrap_or_else(unsupported),
        "location" => m
            .location
            .as_ref()
            .map(|l| Content::Location {
                latitude: l.latitude,
                longitude: l.longitude,
                name: l.name.clone(),
                address: l.address.clone(),
            })
            .unwrap_or_else(unsupported),
        "interactive" => match m.interactive.as_ref() {
            Some(i) => {
                if let Some(r) = &i.button_reply {
                    Content::ButtonReply {
                        id: r.id.clone(),
                        title: r.title.clone(),
                    }
                } else if let Some(r) = &i.list_reply {
                    Content::ListReply {
                        id: r.id.clone(),
                        title: r.title.clone(),
                        description: r.description.clone(),
                    }
                } else {
                    unsupported()
                }
            }
            None => unsupported(),
        },
        // Quick-reply tapped on a template message.
        "button" => m
            .button
            .as_ref()
            .map(|b| Content::ButtonReply {
                id: b.payload.clone().unwrap_or_else(|| b.text.clone()),
                title: b.text.clone(),
            })
            .unwrap_or_else(unsupported),
        "reaction" => m
            .reaction
            .as_ref()
            .map(|r| Content::Reaction {
                message_id: r.message_id.clone(),
                emoji: r.emoji.clone(),
            })
            .unwrap_or_else(unsupported),
        _ => unsupported(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    const PNID: &str = "1000000001";
    const FROM: &str = "2348012345678";

    fn norm(v: serde_json::Value) -> Normalized {
        normalize_bytes(&serde_json::to_vec(&v).unwrap()).unwrap()
    }

    #[test]
    fn text_message() {
        let n = norm(fixtures::text(
            PNID,
            FROM,
            "wamid.A",
            1_700_000_000,
            "sold rice 25000",
        ));
        assert_eq!(n.events.len(), 1);
        let Event::Message(m) = &n.events[0] else {
            panic!("expected message")
        };
        assert_eq!(m.id, "wamid.A");
        assert_eq!(m.from, FROM);
        assert_eq!(m.phone_number_id, PNID);
        assert_eq!(m.timestamp, 1_700_000_000);
        assert_eq!(m.profile_name.as_deref(), Some("Test User"));
        assert_eq!(
            m.content,
            Content::Text {
                body: "sold rice 25000".into()
            }
        );
    }

    #[test]
    fn voice_note_and_image() {
        let n = norm(fixtures::voice_note(PNID, FROM, "wamid.V", 1, "media-1"));
        let Event::Message(m) = &n.events[0] else {
            panic!()
        };
        let info = m.content.media().expect("media");
        assert_eq!(info.media_id, "media-1");
        assert!(info.voice);
        assert!(matches!(m.content, Content::Audio(_)));

        let n = norm(fixtures::image(
            PNID,
            FROM,
            "wamid.I",
            1,
            "media-2",
            Some("receipt"),
        ));
        let Event::Message(m) = &n.events[0] else {
            panic!()
        };
        assert!(matches!(m.content, Content::Image(_)));
        assert_eq!(
            m.content.media().unwrap().caption.as_deref(),
            Some("receipt")
        );
    }

    #[test]
    fn button_reply() {
        let n = norm(fixtures::button_reply(
            PNID,
            FROM,
            "wamid.B",
            1,
            "ok|sale|100|x",
            "Confirm",
        ));
        let Event::Message(m) = &n.events[0] else {
            panic!()
        };
        assert_eq!(
            m.content,
            Content::ButtonReply {
                id: "ok|sale|100|x".into(),
                title: "Confirm".into()
            }
        );
    }

    #[test]
    fn status_with_error() {
        let n = norm(fixtures::status(
            PNID,
            FROM,
            "wamid.S",
            "failed",
            5,
            Some((131047, "Re-engagement message")),
        ));
        let Event::Status(s) = &n.events[0] else {
            panic!()
        };
        assert_eq!(s.status, DeliveryStatus::Failed);
        assert_eq!(s.error.as_ref().unwrap().code, 131047);
        assert_eq!(s.pricing_category.as_deref(), Some("service"));
    }

    #[test]
    fn unknown_types_and_statuses_never_fail() {
        let v = fixtures::inbound(
            PNID,
            FROM,
            "X",
            serde_json::json!({"from": FROM, "id": "wamid.U", "timestamp": "1", "type": "order"}),
        );
        let n = norm(v);
        let Event::Message(m) = &n.events[0] else {
            panic!()
        };
        assert_eq!(
            m.content,
            Content::Unsupported {
                raw_type: "order".into()
            }
        );

        let n = norm(fixtures::status(PNID, FROM, "wamid.S", "deleted", 1, None));
        assert!(n.events.is_empty());
        assert_eq!(n.skipped, 1);
    }

    #[test]
    fn garbage_payloads() {
        assert!(normalize_bytes(b"not json").is_err());
        let n = normalize_bytes(b"{}").unwrap();
        assert!(n.events.is_empty());
    }

    #[test]
    fn dedupe_keys() {
        let a = norm(fixtures::text(PNID, FROM, "wamid.A", 1, "hi"));
        let b = norm(fixtures::text(PNID, FROM, "wamid.A", 2, "hi"));
        assert_eq!(a.events[0].dedupe_key(), b.events[0].dedupe_key());
        let s = norm(fixtures::status(PNID, FROM, "wamid.A", "sent", 1, None));
        assert_eq!(s.events[0].dedupe_key(), "st:wamid.A:sent");
    }
}

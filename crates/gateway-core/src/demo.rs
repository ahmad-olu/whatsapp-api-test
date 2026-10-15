//! Demo "bookkeeping" bot logic. Intentionally tiny: it only exists to prove
//! the full loop works (text in -> buttons out -> button reply in -> text out)
//! without pulling in any real accounting code.

use crate::event::{Content, InboundMessage};
use crate::outbound::{Button, Outbound};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Sale,
    Expense,
}

impl EntryKind {
    fn label(self) -> &'static str {
        match self {
            EntryKind::Sale => "sale",
            EntryKind::Expense => "expense",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoEntry {
    pub kind: EntryKind,
    pub description: String,
    pub amount_naira: u64,
}

/// Parse "sold 5 bags rice 25000", "bought diesel 12,500", "spent n3k transport".
pub fn parse_entry(text: &str) -> Option<DemoEntry> {
    let lower = text.trim().to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let (first, rest) = tokens.split_first()?;
    let kind = match *first {
        "sold" | "sale" | "sell" => EntryKind::Sale,
        "bought" | "buy" | "spent" | "expense" => EntryKind::Expense,
        _ => return None,
    };
    // Amount is the last token; description is everything between.
    let (last, desc) = rest.split_last()?;
    if desc.is_empty() {
        return None;
    }
    let amount = parse_amount(last)?;
    Some(DemoEntry {
        kind,
        description: desc.join(" "),
        amount_naira: amount,
    })
}

fn parse_amount(tok: &str) -> Option<u64> {
    let t = tok.trim_start_matches(['₦', 'n']).replace(',', "");
    let (num, mult) = match t.strip_suffix('k') {
        Some(n) => (n.to_string(), 1000.0),
        None => (t.clone(), 1.0),
    };
    let f: f64 = num.parse().ok()?;
    if !f.is_finite() || f <= 0.0 {
        return None;
    }
    Some((f * mult).round() as u64)
}

pub fn fmt_naira(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("₦{}", out.chars().rev().collect::<String>())
}

const HELP: &str = "Try: \"sold 5 bags rice 25000\" or \"bought diesel 12,500\". \
I'll ask you to confirm before recording.";

/// Decide how the demo bot replies to one inbound message.
pub fn respond(msg: &InboundMessage) -> Vec<Outbound> {
    let to = msg.from.clone();
    let text = |body: String| Outbound::Text {
        to: to.clone(),
        body,
        preview_url: false,
    };

    match &msg.content {
        Content::Text { body } => match parse_entry(body) {
            Some(e) => {
                let desc: String = e
                    .description
                    .chars()
                    .take(120)
                    .filter(|c| *c != '|')
                    .collect();
                let kind = match e.kind {
                    EntryKind::Sale => "sale",
                    EntryKind::Expense => "expense",
                };
                vec![Outbound::Buttons {
                    to: to.clone(),
                    body: format!(
                        "Record {} of {} for \"{}\"?",
                        e.kind.label(),
                        fmt_naira(e.amount_naira),
                        desc
                    ),
                    buttons: vec![
                        Button {
                            id: format!("ok|{}|{}|{}", kind, e.amount_naira, desc),
                            title: "Confirm".into(),
                        },
                        Button {
                            id: "no".into(),
                            title: "Cancel".into(),
                        },
                    ],
                }]
            }
            None => vec![text(HELP.to_string())],
        },
        Content::ButtonReply { id, .. } => {
            if id == "no" {
                return vec![text("Cancelled. Nothing was recorded.".into())];
            }
            let mut parts = id.splitn(4, '|');
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some("ok"), Some(kind), Some(amount), Some(desc)) => {
                    let amount: u64 = amount.parse().unwrap_or(0);
                    vec![text(format!(
                        "✅ Recorded {kind}: {} for \"{desc}\".",
                        fmt_naira(amount)
                    ))]
                }
                _ => vec![text(HELP.to_string())],
            }
        }
        Content::Image(_) => vec![text(
            "📎 Got your image. The real app would read this as a receipt.".into(),
        )],
        Content::Audio(m) if m.voice => vec![text(
            "🎙️ Got your voice note. The real app would transcribe it.".into(),
        )],
        Content::Audio(_) | Content::Document(_) | Content::Video(_) => {
            vec![text("📎 Got your file.".into())]
        }
        _ => vec![text(HELP.to_string())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MediaInfo;

    #[test]
    fn parses_entries() {
        let e = parse_entry("Sold 5 bags rice 25000").unwrap();
        assert_eq!(e.kind, EntryKind::Sale);
        assert_eq!(e.description, "5 bags rice");
        assert_eq!(e.amount_naira, 25_000);

        let e = parse_entry("bought diesel 12,500").unwrap();
        assert_eq!(e.kind, EntryKind::Expense);
        assert_eq!(e.amount_naira, 12_500);

        assert_eq!(
            parse_entry("spent transport ₦3k").unwrap().amount_naira,
            3_000
        );
        assert_eq!(parse_entry("sold bread n1.5k").unwrap().amount_naira, 1_500);
    }

    #[test]
    fn rejects_non_entries() {
        assert!(parse_entry("hello").is_none());
        assert!(parse_entry("sold 5000").is_none()); // no description
        assert!(parse_entry("sold rice lots").is_none()); // no amount
        assert!(parse_entry("sold rice 0").is_none());
        assert!(parse_entry("").is_none());
    }

    #[test]
    fn formats_naira() {
        assert_eq!(fmt_naira(0), "₦0");
        assert_eq!(fmt_naira(999), "₦999");
        assert_eq!(fmt_naira(25_000), "₦25,000");
        assert_eq!(fmt_naira(1_234_567), "₦1,234,567");
    }

    fn msg(content: Content) -> InboundMessage {
        InboundMessage {
            id: "wamid.T".into(),
            phone_number_id: "1".into(),
            from: "2348012345678".into(),
            profile_name: None,
            timestamp: 1,
            reply_to: None,
            content,
        }
    }

    #[test]
    fn full_confirm_loop() {
        let out = respond(&msg(Content::Text {
            body: "sold 5 bags rice 25000".into(),
        }));
        let Outbound::Buttons { buttons, body, .. } = &out[0] else {
            panic!("expected buttons")
        };
        assert!(body.contains("₦25,000"));
        assert!(out[0].validate().is_ok());

        let confirm_id = buttons[0].id.clone();
        let out = respond(&msg(Content::ButtonReply {
            id: confirm_id,
            title: "Confirm".into(),
        }));
        let Outbound::Text { body, .. } = &out[0] else {
            panic!("expected text")
        };
        assert!(body.contains("Recorded sale"));
        assert!(body.contains("5 bags rice"));

        let out = respond(&msg(Content::ButtonReply {
            id: "no".into(),
            title: "Cancel".into(),
        }));
        let Outbound::Text { body, .. } = &out[0] else {
            panic!()
        };
        assert!(body.contains("Cancelled"));
    }

    #[test]
    fn media_replies() {
        let info = MediaInfo {
            media_id: "m".into(),
            mime_type: None,
            sha256: None,
            caption: None,
            filename: None,
            voice: true,
        };
        let out = respond(&msg(Content::Audio(info.clone())));
        assert!(matches!(&out[0], Outbound::Text { body, .. } if body.contains("voice note")));
        let out = respond(&msg(Content::Image(info)));
        assert!(matches!(&out[0], Outbound::Text { body, .. } if body.contains("receipt")));
        let out = respond(&msg(Content::Unsupported {
            raw_type: "order".into(),
        }));
        assert!(matches!(&out[0], Outbound::Text { .. }));
    }
}

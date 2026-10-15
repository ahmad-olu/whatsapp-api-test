//! Builders for Meta-shaped webhook payloads. Used by unit tests and by the
//! `gateway-sim` CLI so both exercise identical payloads.

use serde_json::{json, Value};

fn wrap(phone_number_id: &str, extra: Value) -> Value {
    let mut value = json!({
        "messaging_product": "whatsapp",
        "metadata": {
            "display_phone_number": "15550001111",
            "phone_number_id": phone_number_id
        }
    });
    if let (Some(v), Some(e)) = (value.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            v.insert(k.clone(), x.clone());
        }
    }
    json!({
        "object": "whatsapp_business_account",
        "entry": [{
            "id": "WABA_TEST",
            "changes": [{ "field": "messages", "value": value }]
        }]
    })
}

/// Wrap an arbitrary raw message object in a full webhook payload.
pub fn inbound(phone_number_id: &str, from: &str, profile_name: &str, message: Value) -> Value {
    wrap(
        phone_number_id,
        json!({
            "contacts": [{ "profile": { "name": profile_name }, "wa_id": from }],
            "messages": [message]
        }),
    )
}

pub fn text(phone_number_id: &str, from: &str, wamid: &str, ts: i64, body: &str) -> Value {
    inbound(
        phone_number_id,
        from,
        "Test User",
        json!({
            "from": from, "id": wamid, "timestamp": ts.to_string(),
            "type": "text", "text": { "body": body }
        }),
    )
}

pub fn image(
    phone_number_id: &str,
    from: &str,
    wamid: &str,
    ts: i64,
    media_id: &str,
    caption: Option<&str>,
) -> Value {
    let mut img = json!({ "id": media_id, "mime_type": "image/jpeg", "sha256": "deadbeef" });
    if let Some(c) = caption {
        img["caption"] = json!(c);
    }
    inbound(
        phone_number_id,
        from,
        "Test User",
        json!({
            "from": from, "id": wamid, "timestamp": ts.to_string(),
            "type": "image", "image": img
        }),
    )
}

pub fn voice_note(
    phone_number_id: &str,
    from: &str,
    wamid: &str,
    ts: i64,
    media_id: &str,
) -> Value {
    inbound(
        phone_number_id,
        from,
        "Test User",
        json!({
            "from": from, "id": wamid, "timestamp": ts.to_string(),
            "type": "audio",
            "audio": { "id": media_id, "mime_type": "audio/ogg; codecs=opus", "voice": true }
        }),
    )
}

pub fn button_reply(
    phone_number_id: &str,
    from: &str,
    wamid: &str,
    ts: i64,
    button_id: &str,
    title: &str,
) -> Value {
    inbound(
        phone_number_id,
        from,
        "Test User",
        json!({
            "from": from, "id": wamid, "timestamp": ts.to_string(),
            "type": "interactive",
            "interactive": {
                "type": "button_reply",
                "button_reply": { "id": button_id, "title": title }
            }
        }),
    )
}

/// A delivery status webhook for an outbound message.
pub fn status(
    phone_number_id: &str,
    recipient: &str,
    wamid: &str,
    status: &str,
    ts: i64,
    error: Option<(i64, &str)>,
) -> Value {
    let mut s = json!({
        "id": wamid, "status": status, "timestamp": ts.to_string(),
        "recipient_id": recipient,
        "pricing": { "billable": true, "pricing_model": "PMP", "category": "service" }
    });
    if let Some((code, title)) = error {
        s["errors"] = json!([{ "code": code, "title": title, "message": title }]);
    }
    wrap(phone_number_id, json!({ "statuses": [s] }))
}

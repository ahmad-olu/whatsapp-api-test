//! Delivery status state machine.
//!
//! Webhooks are at-least-once and unordered, so a `read` can arrive before
//! `delivered`, and a `sent` can arrive after both. Statuses therefore only
//! ever move *forward*:
//!
//! ```text
//!   (none) -> sent -> delivered -> read
//!               \
//!                +--> failed   (terminal)
//! ```
//!
//! `failed` is accepted only while the message is still `sent` (or unknown).
//! Once delivered/read a message cannot fail, and once failed it never revives.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Sent,
    Delivered,
    Read,
    Failed,
}

impl DeliveryStatus {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sent" => Some(Self::Sent),
            "delivered" => Some(Self::Delivered),
            "read" => Some(Self::Read),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Read => "read",
            Self::Failed => "failed",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Sent => 1,
            Self::Delivered => 2,
            Self::Read => 3,
            Self::Failed => 0, // handled separately
        }
    }
}

/// Returns the new status if `incoming` should replace `current`, else `None`.
pub fn advance(
    current: Option<DeliveryStatus>,
    incoming: DeliveryStatus,
) -> Option<DeliveryStatus> {
    use DeliveryStatus::*;
    match (current, incoming) {
        (None, s) => Some(s),
        (Some(Failed), _) => None,
        (Some(Sent), Failed) => Some(Failed),
        (Some(_), Failed) => None,
        (Some(cur), inc) if inc.rank() > cur.rank() => Some(inc),
        _ => None,
    }
}

/// Fold a sequence of statuses (in arrival order) into the resulting status.
pub fn fold<I: IntoIterator<Item = DeliveryStatus>>(statuses: I) -> Option<DeliveryStatus> {
    statuses
        .into_iter()
        .fold(None, |cur, inc| advance(cur, inc).or(cur))
}

#[cfg(test)]
mod tests {
    use super::DeliveryStatus::*;
    use super::*;

    #[test]
    fn first_status_always_applies() {
        for s in [Sent, Delivered, Read, Failed] {
            assert_eq!(advance(None, s), Some(s));
        }
    }

    #[test]
    fn moves_forward_only() {
        assert_eq!(advance(Some(Sent), Delivered), Some(Delivered));
        assert_eq!(advance(Some(Delivered), Read), Some(Read));
        assert_eq!(advance(Some(Sent), Read), Some(Read));
        // regressions ignored
        assert_eq!(advance(Some(Read), Delivered), None);
        assert_eq!(advance(Some(Read), Sent), None);
        assert_eq!(advance(Some(Delivered), Sent), None);
        // duplicates ignored
        assert_eq!(advance(Some(Sent), Sent), None);
    }

    #[test]
    fn failed_rules() {
        assert_eq!(advance(Some(Sent), Failed), Some(Failed));
        assert_eq!(advance(Some(Delivered), Failed), None);
        assert_eq!(advance(Some(Read), Failed), None);
        // terminal
        assert_eq!(advance(Some(Failed), Sent), None);
        assert_eq!(advance(Some(Failed), Delivered), None);
        assert_eq!(advance(Some(Failed), Read), None);
    }

    #[test]
    fn fold_handles_out_of_order_arrival() {
        assert_eq!(fold([Read, Delivered, Sent]), Some(Read));
        assert_eq!(fold([Delivered, Sent, Read]), Some(Read));
        assert_eq!(fold([Sent, Failed]), Some(Failed));
        assert_eq!(fold([Failed, Sent]), Some(Failed));
        assert_eq!(fold(std::iter::empty()), None);
    }

    #[test]
    fn parse_roundtrip() {
        for s in [Sent, Delivered, Read, Failed] {
            assert_eq!(DeliveryStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(DeliveryStatus::parse("deleted"), None);
    }
}

//! The 24-hour customer service window.
//!
//! A business may send free-form (non-template) messages only while a window
//! opened by the customer's last inbound message is still open. Outside it,
//! only approved templates can be sent.

pub const WINDOW_SECS: i64 = 24 * 60 * 60;

/// Is a free-form message allowed right now?
pub fn is_open(last_inbound_at: Option<i64>, now: i64) -> bool {
    matches!(last_inbound_at, Some(t) if now - t < WINDOW_SECS)
}

/// Unix time at which the window closes, if one was ever opened.
pub fn closes_at(last_inbound_at: Option<i64>) -> Option<i64> {
    last_inbound_at.map(|t| t + WINDOW_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_inbound_means_closed() {
        assert!(!is_open(None, 1_000_000));
    }

    #[test]
    fn open_inside_24h_closed_at_boundary() {
        let t = 1_700_000_000;
        assert!(is_open(Some(t), t));
        assert!(is_open(Some(t), t + WINDOW_SECS - 1));
        assert!(!is_open(Some(t), t + WINDOW_SECS));
        assert!(!is_open(Some(t), t + WINDOW_SECS + 3600));
    }

    #[test]
    fn closes_at_is_last_inbound_plus_window() {
        assert_eq!(closes_at(Some(100)), Some(100 + WINDOW_SECS));
        assert_eq!(closes_at(None), None);
    }
}

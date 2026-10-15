//! Pure WhatsApp Cloud API logic. No I/O, no async, no database.

pub mod demo;
pub mod event;
pub mod fixtures;
pub mod normalize;
pub mod outbound;
pub mod payload;
pub mod signature;
pub mod status;
pub mod window;

pub use event::{Content, Event, InboundMessage, MediaInfo, StatusError, StatusUpdate};
pub use normalize::{normalize, normalize_bytes, Normalized};
pub use outbound::{Button, MediaRef, Outbound, OutboundError};
pub use payload::WebhookPayload;
pub use status::DeliveryStatus;

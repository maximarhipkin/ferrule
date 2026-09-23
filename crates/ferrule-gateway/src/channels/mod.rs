//! Concrete `Channel` implementations. Kept in their own module so new
//! adapters (WhatsApp, Slack, …) can be added later without touching the
//! trait definition in `crate::channel`.

pub mod local;
pub mod telegram;

pub use local::LocalChannel;
pub use telegram::TelegramChannel;

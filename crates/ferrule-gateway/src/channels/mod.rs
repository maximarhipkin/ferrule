//! Concrete `Channel` implementations. Kept in their own module so new
//! adapters (WhatsApp, Slack, …) can be added later without touching the
//! trait definition in `crate::channel`.

pub mod access;
pub mod discord;
pub mod local;
pub mod slack;
pub mod telegram;
pub mod ws;

pub use discord::DiscordChannel;
pub use local::LocalChannel;
pub use slack::SlackChannel;
pub use telegram::TelegramChannel;

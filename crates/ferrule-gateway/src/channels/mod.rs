//! Concrete `Channel` implementations. Kept in their own module so new
//! adapters (WhatsApp, Slack, …) can be added later without touching the
//! trait definition in `crate::channel`.

pub mod access;
pub mod discord;
pub mod email;
pub mod files;
pub mod hmac;
pub mod local;
pub mod matrix;
pub mod mattermost;
pub mod signal;
pub mod slack;
pub mod telegram;
pub mod whatsapp;
pub mod ws;

pub use discord::DiscordChannel;
pub use email::EmailChannel;
pub use local::LocalChannel;
pub use matrix::MatrixChannel;
pub use mattermost::MattermostChannel;
pub use signal::SignalChannel;
pub use slack::SlackChannel;
pub use telegram::TelegramChannel;
pub use whatsapp::WhatsAppChannel;

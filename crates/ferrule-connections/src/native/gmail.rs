//! Gmail with an app password: search and read over IMAP, send over SMTP.
//! Since M63.2 it is the general mailbox (`mail.rs`) with Gmail's habits:
//! Gmail's own search syntax (`X-GM-RAW`) over All Mail, Google's servers,
//! and Google's words when the password is refused. Its tools keep their
//! names (`gmail_search`, `gmail_read`, `gmail_send`).

use super::mail::{self, Flavor, Mail};
use super::Endpoints;
use std::collections::BTreeMap;

/// A Gmail connection `id` from its fields (`email`, `app_password`).
pub fn new(
    id: &str,
    fields: &BTreeMap<String, String>,
    write: bool,
    ep: &Endpoints,
) -> Result<Mail, String> {
    mail::new(id, Flavor::Gmail, fields, write, ep, None)
}

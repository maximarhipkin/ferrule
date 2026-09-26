//! Discord's REST rate limits, as its docs describe them. A route is its
//! method and path with every id but the major one (the channel, guild or
//! webhook) blanked; each response names the route's bucket and how much
//! of it is left (`X-RateLimit-Bucket`, `-Remaining`, `-Reset-After`). A
//! request whose bucket is empty waits for its reset. A 429 says how long
//! (`retry_after`, fractional seconds); a global one stops every request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The longest a request waits for a bucket before it gives up.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Limits {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Route → the bucket Discord said it's in.
    buckets: HashMap<String, String>,
    /// Bucket (or route, before Discord named one) → when it's usable again.
    blocked: HashMap<String, Instant>,
    global: Option<Instant>,
}

/// The route key: `PATCH /channels/1/messages/:id` for any message in
/// channel 1. Webhook routes keep their token, which is part of the major
/// parameter, but it only lives in memory.
pub fn route(method: &str, path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    let mut out = String::from(method);
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        out.push('/');
        let major = i == 2
            && matches!(
                parts[1],
                "channels" | "guilds" | "webhooks" | "interactions"
            );
        let id_like = part.chars().all(|c| c.is_ascii_digit());
        if id_like && !major {
            out.push_str(":id");
        } else if part.contains('%') {
            // A reaction's emoji: every one shares the route.
            out.push_str(":emoji");
        } else {
            out.push_str(part);
        }
    }
    out
}

impl Inner {
    /// A bucket is shared by routes, but counted per major parameter: the
    /// same bucket in two channels is two limits.
    fn key(&self, route: &str) -> String {
        match self.buckets.get(route) {
            Some(bucket) => {
                let mut parts = route.split('/');
                let major = match (parts.nth(1), parts.next()) {
                    (Some("channels" | "guilds" | "webhooks" | "interactions"), Some(id)) => id,
                    _ => "",
                };
                format!("{bucket}/{major}")
            }
            None => route.to_string(),
        }
    }
}

impl Limits {
    /// How long `route` must wait now; zero when it may go.
    pub fn wait_for(&self, route: &str) -> Duration {
        let now = Instant::now();
        let inner = self.inner.lock().unwrap();
        let global = inner
            .global
            .map_or(Duration::ZERO, |g| g.saturating_duration_since(now));
        let key = inner.key(route);
        let own = inner
            .blocked
            .get(&key)
            .map_or(Duration::ZERO, |b| b.saturating_duration_since(now));
        global.max(own)
    }

    /// A response's headers: the bucket, and whether it's empty now.
    pub fn update(&self, route: &str, headers: &reqwest::header::HeaderMap) {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let mut inner = self.inner.lock().unwrap();
        if let Some(bucket) = get("x-ratelimit-bucket") {
            inner.buckets.insert(route.to_string(), bucket);
        }
        let key = inner.key(route);
        let remaining = get("x-ratelimit-remaining").and_then(|v| v.parse::<u64>().ok());
        let reset = get("x-ratelimit-reset-after")
            .and_then(|v| v.parse::<f64>().ok())
            .map(secs);
        match (remaining, reset) {
            (Some(0), Some(reset)) => {
                inner.blocked.insert(key, Instant::now() + reset);
            }
            (Some(_), _) => {
                inner.blocked.remove(&key);
            }
            _ => {}
        }
    }

    /// A 429 on `route`: it (or, `global`, everything) waits `retry_after`.
    pub fn limited(&self, route: &str, retry_after: Duration, global: bool) {
        let until = Instant::now() + retry_after;
        let mut inner = self.inner.lock().unwrap();
        if global {
            inner.global = Some(inner.global.map_or(until, |g| g.max(until)));
        } else {
            let key = inner.key(route);
            inner.blocked.insert(key, until);
        }
    }
}

/// Fractional seconds as a duration, never negative, at most a day.
pub fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s.clamp(0.0, 86_400.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn routes_keep_only_the_major_id() {
        assert_eq!(
            route("PATCH", "/channels/111/messages/222"),
            "PATCH/channels/111/messages/:id"
        );
        assert_eq!(
            route(
                "PUT",
                "/channels/111/messages/222/reactions/%F0%9F%91%80/@me"
            ),
            "PUT/channels/111/messages/:id/reactions/:emoji/@me"
        );
        assert_eq!(
            route("POST", "/users/@me/channels"),
            "POST/users/@me/channels"
        );
        assert_ne!(
            route("POST", "/channels/1/messages"),
            route("POST", "/channels/2/messages")
        );
    }

    #[test]
    fn an_empty_bucket_waits_for_its_reset_and_a_global_429_stops_all() {
        let l = Limits::default();
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-bucket", HeaderValue::from_static("abc"));
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        h.insert("x-ratelimit-reset-after", HeaderValue::from_static("2.5"));
        l.update("POST/channels/1/messages", &h);
        let w = l.wait_for("POST/channels/1/messages");
        assert!(w > Duration::from_secs(2) && w <= Duration::from_millis(2500));
        assert_eq!(l.wait_for("POST/channels/2/messages"), Duration::ZERO);
        // The same bucket in another channel is its own limit.
        let mut ok = h.clone();
        ok.insert("x-ratelimit-remaining", HeaderValue::from_static("4"));
        l.update("POST/channels/2/messages", &ok);
        assert_eq!(l.wait_for("POST/channels/2/messages"), Duration::ZERO);
        assert!(l.wait_for("POST/channels/1/messages") > Duration::from_secs(2));
        l.limited("GET/users/@me", Duration::from_secs(3), true);
        assert!(l.wait_for("POST/channels/2/messages") > Duration::from_secs(2));
    }
}

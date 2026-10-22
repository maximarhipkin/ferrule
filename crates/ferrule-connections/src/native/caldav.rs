//! M63.3: calendars without Google's Cloud console. Two native connections:
//!
//! - **CalDAV** (RFC 4791) with a user name and an app password: iCloud,
//!   Fastmail, Nextcloud or any server. It finds the calendars the way RFC
//!   6764 says (`current-user-principal`, then `calendar-home-set`, trying
//!   `/.well-known/caldav` when the address given isn't the service), lists
//!   events in a range with the server expanding repeats, and creates,
//!   changes and deletes events. The three changes are writes: listed only
//!   on a write connection, and gated like any connected write.
//! - **A calendar's secret address** (an `.ics` URL, read-only): Google's
//!   "Secret address in iCal format", a published Outlook or iCloud
//!   calendar. The address is the credential, so it's a secret field,
//!   sealed and scrubbed like a password, and never put in a message.
//!
//! Event text is someone else's words: a connected service's read marks the
//! run as having read outside content (M54.5). The credential goes only to
//! the hosts the connection was saved with; a redirect elsewhere is refused,
//! never followed.

use super::ical::{self, Event, When};
use super::xml::{self, Node};
use super::{arg_str, cap, limit, opt_str, Endpoints};
use async_trait::async_trait;
use ferrule_mcp::local::LocalServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The most of an answer ferrule reads (a secret calendar's whole history
/// can be large).
const MAX_BODY: usize = 20 * 1024 * 1024;
/// How long the calendars found stay known before they're looked up again.
const KNOWN_FOR: Duration = Duration::from_secs(600);
/// The most events one list returns.
const MAX_EVENTS: u64 = 100;

/// A host pattern a credential may go to: `a.example.com` or
/// `*.example.com` (any name under it).
fn allowed(patterns: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    patterns.iter().any(|p| match p.strip_prefix("*.") {
        Some(suffix) => host.ends_with(&format!(".{suffix}")),
        None => host == *p,
    })
}

/// The connection's address checked: https (a test's `http://127.0.0.1`
/// with `ep.loopback`), `webcal://` read as https.
pub fn address(raw: &str, ep: &Endpoints) -> Result<url::Url, String> {
    let raw = raw.trim();
    let raw = match raw.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => raw.to_string(),
    };
    let url = url::Url::parse(&raw).map_err(|_| "that isn't a web address".to_string())?;
    let loopback = ep.loopback && url.host_str() == Some("127.0.0.1");
    if url.scheme() != "https" && !loopback {
        return Err("the address must start with https://".into());
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err("the address needs a host, and no user name or password in it".into());
    }
    Ok(url)
}

/// The hosts a CalDAV connection's password may go to: its address's, plus
/// a preset's `extra_hosts` (iCloud answers from `pNN-caldav.icloud.com`).
pub fn caldav_hosts(fields: &BTreeMap<String, String>) -> Vec<String> {
    let mut hosts: Vec<String> = fields
        .get("url")
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .into_iter()
        .collect();
    if let Some(extra) = fields.get("extra_hosts") {
        hosts.extend(
            extra
                .split(',')
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty()),
        );
    }
    hosts.dedup();
    hosts
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(super::net::TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("ferrule/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("an HTTP client")
}

/// An answer: where it came from after redirects, its status, `ETag`,
/// and body (at most [`MAX_BODY`]).
struct Answer {
    url: url::Url,
    status: u16,
    etag: Option<String>,
    body: String,
}

/// Sends `method` to `url` with the credential, following up to five
/// redirects that stay on `hosts`.
#[allow(clippy::too_many_arguments)]
async fn send(
    http: &reqwest::Client,
    hosts: &[String],
    auth: Option<(&str, &str)>,
    method: Method,
    mut url: url::Url,
    headers: &[(&str, String)],
    body: Option<String>,
    what: &str,
) -> Result<Answer, String> {
    for _ in 0..6 {
        let host = url.host_str().unwrap_or_default().to_string();
        if !allowed(hosts, &host) {
            return Err(format!(
                "{what} sent ferrule on to {host}, which this connection isn't bound to, so \
                 nothing was sent there. Connect again with that address if it's the right one."
            ));
        }
        let mut r = http.request(method.clone(), url.clone());
        if let Some((user, password)) = auth {
            r = r.basic_auth(user, Some(password));
        }
        for (k, v) in headers {
            r = r.header(*k, v);
        }
        if let Some(b) = &body {
            r = r.body(b.clone());
        }
        let resp = r
            .send()
            .await
            .map_err(|_| format!("couldn't reach {what} at {host}"))?;
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let next = resp
                .headers()
                .get("location")
                .and_then(|l| l.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .ok_or_else(|| format!("{what} redirected without saying where"))?;
            url = next;
            continue;
        }
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut bytes = Vec::new();
        let mut resp = resp;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|_| format!("the answer from {what} broke off"))?
        {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_BODY {
                return Err(format!(
                    "{what}'s answer is over {} MB, more than ferrule reads",
                    MAX_BODY / 1024 / 1024
                ));
            }
        }
        return Ok(Answer {
            url,
            status,
            etag,
            body: String::from_utf8_lossy(&bytes).to_string(),
        });
    }
    Err(format!("{what} redirected too many times"))
}

/// One calendar on a CalDAV server: its collection's URL (ending in `/`).
#[derive(Debug, Clone)]
struct Calendar {
    url: url::Url,
    name: String,
}

pub struct CalDav {
    id: String,
    base: url::Url,
    user: String,
    password: String,
    hosts: Vec<String>,
    write: bool,
    http: reqwest::Client,
    known: Mutex<Option<(Instant, Vec<Calendar>)>>,
}

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;

const PROPFIND_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;

const PROPFIND_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:resourcetype/><d:displayname/><c:supported-calendar-component-set/></d:prop></d:propfind>"#;

impl CalDav {
    pub fn new(
        id: &str,
        fields: &BTreeMap<String, String>,
        write: bool,
        ep: &Endpoints,
    ) -> Result<Self, String> {
        let get = |k: &str| {
            fields
                .get(k)
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| format!("the {} is missing", k.replace('_', " ")))
        };
        let base = address(&get("url")?, ep)?;
        Ok(Self {
            id: format!("native:caldav:{id}"),
            base,
            user: get("username")?,
            password: get("app_password")?,
            hosts: caldav_hosts(fields),
            write,
            http: client(),
            known: Mutex::new(None),
        })
    }

    async fn dav(
        &self,
        method: &str,
        url: url::Url,
        depth: Option<&str>,
        body: Option<&str>,
        extra: &[(&str, String)],
    ) -> Result<Answer, String> {
        let method = Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
        let mut headers: Vec<(&str, String)> = extra.to_vec();
        if let Some(d) = depth {
            headers.push(("Depth", d.to_string()));
        }
        if body.is_some() && !extra.iter().any(|(k, _)| *k == "Content-Type") {
            headers.push(("Content-Type", "application/xml; charset=utf-8".into()));
        }
        let a = send(
            &self.http,
            &self.hosts,
            Some((&self.user, &self.password)),
            method,
            url,
            &headers,
            body.map(str::to_string),
            "the calendar server",
        )
        .await?;
        match a.status {
            401 => Err(
                "the calendar server refused the user name and app password. Check them, \
                 and make a new app password if needed: the account's normal password usually \
                 won't work here."
                    .into(),
            ),
            403 => Err("the calendar server says this account may not do that".into()),
            _ => Ok(a),
        }
    }

    /// The `href` inside the first `name` property of a multistatus.
    fn href_of(doc: &Node, name: &str) -> Option<String> {
        doc.find(name)
            .and_then(|n| n.find("href"))
            .map(|h| h.trimmed().to_string())
            .filter(|h| !h.is_empty())
    }

    /// The calendars, found again when the last look is older than
    /// [`KNOWN_FOR`].
    async fn calendars(&self) -> Result<Vec<Calendar>, String> {
        if let Some((at, cals)) = self.known.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            if at.elapsed() < KNOWN_FOR {
                return Ok(cals);
            }
        }
        let cals = self.discover().await?;
        *self.known.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), cals.clone()));
        Ok(cals)
    }

    async fn discover(&self) -> Result<Vec<Calendar>, String> {
        let unreadable = || "the calendar server's answer couldn't be read".to_string();
        let mut principal = None;
        let well_known = self
            .base
            .join("/.well-known/caldav")
            .map_err(|e| e.to_string())?;
        for start in [self.base.clone(), well_known] {
            let a = self
                .dav(
                    "PROPFIND",
                    start.clone(),
                    Some("0"),
                    Some(PROPFIND_PRINCIPAL),
                    &[],
                )
                .await?;
            if a.status != 207 {
                continue;
            }
            let doc = xml::parse(&a.body)?;
            if let Some(h) = Self::href_of(&doc, "current-user-principal") {
                principal = Some(a.url.join(&h).map_err(|_| unreadable())?);
                break;
            }
        }
        let principal = principal.ok_or(
            "the address doesn't lead to a CalDAV account: use the server's CalDAV address \
             (its help pages give it)",
        )?;
        let a = self
            .dav(
                "PROPFIND",
                principal.clone(),
                Some("0"),
                Some(PROPFIND_HOME),
                &[],
            )
            .await?;
        let home = (a.status == 207)
            .then(|| xml::parse(&a.body).ok())
            .flatten()
            .and_then(|doc| Self::href_of(&doc, "calendar-home-set"))
            .ok_or("the calendar server didn't say where this account's calendars are")?;
        let home = a.url.join(&home).map_err(|_| unreadable())?;
        let a = self
            .dav(
                "PROPFIND",
                home.clone(),
                Some("1"),
                Some(PROPFIND_CALENDARS),
                &[],
            )
            .await?;
        if a.status != 207 {
            return Err(format!(
                "the calendar server didn't list the calendars ({})",
                a.status
            ));
        }
        let doc = xml::parse(&a.body)?;
        let mut cals = Vec::new();
        for r in doc.children_named("response") {
            let Some(href) = r.child("href").map(|h| h.trimmed().to_string()) else {
                continue;
            };
            let Ok(mut url) = a.url.join(&href) else {
                continue;
            };
            if !url.path().ends_with('/') {
                let path = format!("{}/", url.path());
                url.set_path(&path);
            }
            let calendar = r
                .find("resourcetype")
                .is_some_and(|t| t.child("calendar").is_some());
            // A calendar that names its components but not events is a
            // task list.
            let events = r
                .find("supported-calendar-component-set")
                .map(|set| {
                    set.children_named("comp")
                        .any(|c| c.attr("name") == Some("VEVENT"))
                })
                .unwrap_or(true);
            if !calendar || !events {
                continue;
            }
            let name = r
                .find("displayname")
                .map(|d| d.trimmed().to_string())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| {
                    href.trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .to_string()
                });
            cals.push(Calendar { url, name });
        }
        if cals.is_empty() {
            return Err("the account has no calendars for events".into());
        }
        Ok(cals)
    }

    /// The calendar `which` names (its name, any case, or its id), or the
    /// first one.
    async fn calendar(&self, which: Option<&str>) -> Result<Calendar, String> {
        let cals = self.calendars().await?;
        match which {
            None => Ok(cals[0].clone()),
            Some(w) => cals
                .iter()
                .find(|c| {
                    let w = w.trim();
                    c.name.eq_ignore_ascii_case(w) || c.url.path() == w || c.url.as_str() == w
                })
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "there's no calendar `{w}`; there are: {}",
                        cals.iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }),
        }
    }

    pub async fn check(&self) -> Result<String, String> {
        let cals = self.discover().await?;
        Ok(format!(
            "{} calendar(s): {}",
            cals.len(),
            cals.iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    async fn list_calendars(&self) -> Result<String, String> {
        let cals = self.calendars().await?;
        let mut out = String::from("Calendars (name · id):\n");
        for c in cals {
            out.push_str(&format!("{} · {}\n", c.name, c.url.path()));
        }
        Ok(out)
    }

    async fn events(&self, args: &Value) -> Result<String, String> {
        let (from, to) = range(args)?;
        let cal = self.calendar(opt_str(args, "calendar")).await?;
        let (start, end) = (ical::utc_stamp(from), ical::utc_stamp(to));
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
<d:prop><d:getetag/><c:calendar-data><c:expand start="{start}" end="{end}"/></c:calendar-data></d:prop>
<c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="{start}" end="{end}"/></c:comp-filter></c:comp-filter></c:filter>
</c:calendar-query>"#
        );
        let a = self
            .dav("REPORT", cal.url.clone(), Some("1"), Some(&body), &[])
            .await?;
        if a.status != 207 {
            return Err(format!(
                "the calendar server didn't list the events ({})",
                a.status
            ));
        }
        let doc = xml::parse(&a.body)?;
        let mut found: Vec<Found> = Vec::new();
        for r in doc.children_named("response") {
            let id = r
                .child("href")
                .and_then(|h| a.url.join(h.trimmed()).ok())
                .map(|u| u.path().to_string());
            let Some(data) = r.find("calendar-data").map(|d| d.text.clone()) else {
                continue;
            };
            found.extend(events_in(&data, from, to, id.as_deref()));
        }
        Ok(show(&mut found, &cal.name, args))
    }

    /// An event's URL from its id (its path), checked: inside one of this
    /// account's calendars, on that calendar's host.
    async fn event_url(&self, id: &str) -> Result<url::Url, String> {
        let cals = self.calendars().await?;
        cals.iter()
            .filter_map(|c| {
                let url = c.url.join(id.trim()).ok()?;
                (url.host_str() == c.url.host_str()
                    && url.path().starts_with(c.url.path())
                    && url.path().len() > c.url.path().len())
                .then_some(url)
            })
            .next()
            .ok_or_else(|| {
                format!(
                    "`{id}` isn't an event in this account's calendars (use an id from events_list)"
                )
            })
    }

    async fn create(&self, args: &Value) -> Result<String, String> {
        let cal = self.calendar(opt_str(args, "calendar")).await?;
        let start = ical::arg_time(arg_str(args, "start")?)?;
        let end = match opt_str(args, "end") {
            Some(e) => ical::arg_time(e)?,
            None => default_end(&start),
        };
        if end.key() <= start.key() {
            return Err("`end` must be after `start`".into());
        }
        let uid = new_uid()?;
        let ics = build(&uid, arg_str(args, "summary")?, &start, &end, args);
        let url = cal
            .url
            .join(&format!("{uid}.ics"))
            .map_err(|e| e.to_string())?;
        let a = self
            .dav(
                "PUT",
                url.clone(),
                None,
                Some(&ics),
                &[
                    ("Content-Type", "text/calendar; charset=utf-8".into()),
                    ("If-None-Match", "*".into()),
                ],
            )
            .await?;
        match a.status {
            200 | 201 | 204 => Ok(format!(
                "created in {}: {} (id {})",
                cal.name,
                start.show(),
                url.path()
            )),
            412 => Err("an event with that id exists already; nothing was changed".into()),
            s => Err(format!("the calendar server didn't save the event ({s})")),
        }
    }

    async fn update(&self, args: &Value) -> Result<String, String> {
        let url = self.event_url(arg_str(args, "id")?).await?;
        let a = self.dav("GET", url.clone(), None, None, &[]).await?;
        if a.status == 404 {
            return Err("there's no such event any more".into());
        }
        if a.status != 200 {
            return Err(format!(
                "the calendar server didn't give the event ({})",
                a.status
            ));
        }
        let mut start = None;
        let mut end = None;
        if let Some(s) = opt_str(args, "start") {
            start = Some(ical::arg_time(s)?);
        }
        if let Some(e) = opt_str(args, "end") {
            end = Some(ical::arg_time(e)?);
        }
        let ics = edit(&a.body, args, start.as_ref(), end.as_ref())?;
        let mut headers = vec![("Content-Type", "text/calendar; charset=utf-8".to_string())];
        if let Some(tag) = &a.etag {
            headers.push(("If-Match", tag.clone()));
        }
        let put = self.dav("PUT", url, None, Some(&ics), &headers).await?;
        match put.status {
            200 | 201 | 204 => Ok("changed".into()),
            412 => Err("the event changed on the server meanwhile; list it again first".into()),
            s => Err(format!("the calendar server didn't save the change ({s})")),
        }
    }

    async fn delete(&self, args: &Value) -> Result<String, String> {
        let url = self.event_url(arg_str(args, "id")?).await?;
        let a = self.dav("DELETE", url, None, None, &[]).await?;
        match a.status {
            200 | 202 | 204 => Ok("deleted".into()),
            404 => Err("there's no such event any more".into()),
            s => Err(format!("the calendar server didn't delete it ({s})")),
        }
    }
}

/// A random event id.
fn new_uid() -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| "no randomness for an event id".to_string())?;
    Ok(format!(
        "{}-ferrule",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    ))
}

/// An hour after a timed start; the day after an all-day one.
fn default_end(start: &When) -> When {
    shifted(start, if start.minute.is_some() { 60 } else { 1440 })
}

/// A new event as an iCalendar object.
fn build(uid: &str, summary: &str, start: &When, end: &When, args: &Value) -> String {
    let stamp = ical::utc_stamp((crate::store::now() / 60) as i64);
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//ferrule//M63//EN".to_string(),
        "BEGIN:VEVENT".to_string(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{stamp}"),
        ical::time_line("DTSTART", start),
        ical::time_line("DTEND", end),
        format!("SUMMARY:{}", ical::escape(summary)),
    ];
    for (arg, prop) in [("location", "LOCATION"), ("description", "DESCRIPTION")] {
        if let Some(v) = opt_str(args, arg) {
            lines.push(format!("{prop}:{}", ical::escape(v)));
        }
    }
    lines.push("END:VEVENT".into());
    lines.push("END:VCALENDAR".into());
    lines.iter().map(|l| ical::fold(l)).collect()
}

/// `ics` with its event (the series, not an override of one occurrence)
/// given the new summary, times, place and description; every other line,
/// an alarm's own included, as it was.
fn edit(
    ics: &str,
    args: &Value,
    start: Option<&When>,
    end: Option<&When>,
) -> Result<String, String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let l = raw.strip_suffix('\r').unwrap_or(raw);
        match (l.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ if l.is_empty() => {}
            _ => lines.push(l.to_string()),
        }
    }
    let prop = |l: &str| {
        l.split([':', ';'])
            .next()
            .unwrap_or("")
            .to_ascii_uppercase()
    };
    let mut set: BTreeMap<String, String> = BTreeMap::new();
    if let Some(s) = opt_str(args, "summary") {
        set.insert("SUMMARY".into(), format!("SUMMARY:{}", ical::escape(s)));
    }
    for (arg, name) in [("location", "LOCATION"), ("description", "DESCRIPTION")] {
        if let Some(v) = opt_str(args, arg) {
            set.insert(name.into(), format!("{name}:{}", ical::escape(v)));
        }
    }
    if set.is_empty() && start.is_none() && end.is_none() {
        return Err("nothing to change: give summary, start, end, location or description".into());
    }
    // The series' VEVENT: its lines, and which are its own (not an alarm's).
    let mut found = None;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].eq_ignore_ascii_case("BEGIN:VEVENT") {
            let (mut depth, mut j, mut own, mut rid) = (0, i + 1, Vec::new(), false);
            while j < lines.len() {
                let up = lines[j].to_ascii_uppercase();
                if up.starts_with("BEGIN:") {
                    depth += 1;
                } else if up.starts_with("END:") {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                } else if depth == 0 {
                    rid |= prop(&lines[j]) == "RECURRENCE-ID";
                    own.push(j);
                }
                j += 1;
            }
            if !rid {
                found = Some((j, own));
                break;
            }
            i = j;
        }
        i += 1;
    }
    let (close, own) = found.ok_or("the event's data has no event in it ferrule can change")?;
    let old = |name: &str| {
        own.iter()
            .find(|&&k| prop(&lines[k]) == name)
            .and_then(|&k| parse_prop_when(&lines[k]))
    };
    if let Some(s) = start {
        set.insert("DTSTART".into(), ical::time_line("DTSTART", s));
        // A start moved with no end given keeps its length.
        if end.is_none() && !own.iter().any(|&k| prop(&lines[k]) == "DURATION") {
            if let (Some(os), Some(oe)) = (old("DTSTART"), old("DTEND")) {
                let ne = shifted(s, (oe.key() - os.key()).max(0));
                set.insert("DTEND".into(), ical::time_line("DTEND", &ne));
            }
        }
    }
    if let Some(e) = end {
        set.insert("DTEND".into(), ical::time_line("DTEND", e));
    }
    let mut written: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for (k, line) in lines.iter().enumerate() {
        if k == close {
            for (name, value) in &set {
                if !written.contains(name) {
                    out.push(value.clone());
                }
            }
        }
        if own.contains(&k) {
            let name = prop(line);
            if let Some(value) = set.get(&name) {
                if !written.contains(&name) {
                    out.push(value.clone());
                    written.push(name);
                }
                continue;
            }
        }
        out.push(line.clone());
    }
    Ok(out.iter().map(|l| ical::fold(l)).collect())
}

/// `w` moved on by `minutes`.
fn shifted(w: &When, minutes: i64) -> When {
    let key = w.key() + minutes;
    let (y, mo, d) = super::mime::civil(key.div_euclid(1440));
    When {
        y,
        mo,
        d,
        minute: w.minute.map(|_| key.rem_euclid(1440) as u32),
        zone: w.zone.clone(),
    }
}

/// A `DTSTART`-style line's time.
fn parse_prop_when(line: &str) -> Option<When> {
    let comps = ical::parse(&format!("BEGIN:X\r\n{line}\r\nEND:X\r\n"));
    comps.first()?.props.first().and_then(ical::when)
}

/// The tools' `from`/`to` as keys: today (UTC) and a week on by default.
fn range(args: &Value) -> Result<(i64, i64), String> {
    let today = (crate::store::now() as i64).div_euclid(86_400) * 1440;
    let from = match opt_str(args, "from") {
        Some(f) => ical::arg_time(f)?.key(),
        None => today,
    };
    let to = match opt_str(args, "to") {
        Some(t) => {
            let w = ical::arg_time(t)?;
            // A date `to` takes in that whole day.
            w.key() + if w.minute.is_none() { 1440 } else { 0 }
        }
        None => from + 7 * 1440,
    };
    if to <= from {
        return Err("`to` must be after `from`".into());
    }
    if to - from > 400 * 1440 {
        return Err("ask for at most about a year at a time".into());
    }
    Ok((from, to))
}

/// One occurrence to show.
struct Found {
    start: When,
    length: i64,
    event: Event,
    id: Option<String>,
    note: Option<String>,
}

/// The occurrences of the events in `data` within `[from, to)`. A series
/// with an override of one occurrence shows the override in its place.
fn events_in(data: &str, from: i64, to: i64, id: Option<&str>) -> Vec<Found> {
    let mut comps = Vec::new();
    let cals = ical::parse(data);
    for c in &cals {
        if c.name == "VEVENT" {
            comps.push(c);
        }
        c.all("VEVENT", &mut comps);
    }
    let events: Vec<Event> = comps.iter().filter_map(|c| Event::from(c)).collect();
    let mut out = Vec::new();
    for e in &events {
        if e.status.eq_ignore_ascii_case("CANCELLED") {
            continue;
        }
        let overrides: Vec<i64> = events
            .iter()
            .filter(|o| o.uid == e.uid && o.recurrence_id.is_some() && e.recurrence_id.is_none())
            .filter_map(|o| o.recurrence_id)
            .collect();
        match e.occurrences(from, to, MAX_EVENTS as usize) {
            Ok(list) => {
                for start in list {
                    if e.rrule.is_some() && overrides.contains(&start.key()) {
                        continue;
                    }
                    out.push(Found {
                        start,
                        length: e.length,
                        event: e.clone(),
                        id: id.map(str::to_string),
                        note: None,
                    });
                }
            }
            // A rule ferrule doesn't work out: shown once, when it began
            // before the range ends, with the rule.
            Err(rule) if e.start.key() < to => out.push(Found {
                start: e.start.clone(),
                length: e.length,
                event: e.clone(),
                id: id.map(str::to_string),
                note: Some(format!(
                    "repeats ({rule}); the dates in this range weren't worked out"
                )),
            }),
            Err(_) => {}
        }
    }
    out
}

/// The list the tools answer with.
fn show(found: &mut [Found], calendar: &str, args: &Value) -> String {
    let text = opt_str(args, "text").map(|t| t.to_lowercase());
    let n = limit(args, 25, MAX_EVENTS) as usize;
    found.sort_by_key(|f| f.start.key());
    let shown: Vec<&Found> = found
        .iter()
        .filter(|f| {
            text.as_ref().is_none_or(|t| {
                f.event.summary.to_lowercase().contains(t)
                    || f.event.location.to_lowercase().contains(t)
                    || f.event.description.to_lowercase().contains(t)
            })
        })
        .collect();
    if shown.is_empty() {
        return format!("no events in {calendar} in that range");
    }
    let mut out = format!(
        "{} event(s) in {calendar}{}:\n",
        shown.len(),
        if shown.len() > n {
            format!(", the first {n}")
        } else {
            String::new()
        }
    );
    for f in shown.iter().take(n) {
        let end = if f.start.minute.is_none() {
            if f.length > 1440 {
                format!(" to {}", day_before(&f.start, f.length))
            } else {
                " (all day)".to_string()
            }
        } else {
            let m = (i64::from(f.start.minute.unwrap_or(0)) + f.length).rem_euclid(1440);
            format!("–{:02}:{:02}", m / 60, m % 60)
        };
        out.push_str(&format!(
            "{}{end} · {}{}",
            f.start.show(),
            if f.event.summary.is_empty() {
                "(no title)"
            } else {
                &f.event.summary
            },
            if f.event.location.is_empty() {
                String::new()
            } else {
                format!(" @ {}", f.event.location)
            }
        ));
        if let Some(id) = &f.id {
            out.push_str(&format!(" · id {id}"));
        }
        if let Some(note) = &f.note {
            out.push_str(&format!(" · {note}"));
        }
        out.push('\n');
        let d = f.event.description.trim();
        if !d.is_empty() {
            let short: String = d.chars().take(300).collect();
            out.push_str(&format!("    {}\n", short.replace('\n', " ")));
        }
    }
    out.push_str(
        "(times are each event's own; an event's text is its organiser's, not the owner's)\n",
    );
    cap(out)
}

/// The last day an all-day event of `length` minutes covers.
fn day_before(start: &When, length: i64) -> String {
    let key = start.key() + length - 1440;
    let (y, mo, d) = super::mime::civil(key.div_euclid(1440));
    format!("{y:04}-{mo:02}-{d:02}")
}

#[async_trait]
impl LocalServer for CalDav {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn tools(&self) -> Vec<Value> {
        let s = |d: &str| json!({ "type": "string", "description": d });
        let when = "2026-10-22 (all day), 2026-10-22T09:00 (that time wherever you are) or 2026-10-22T09:00+03:00";
        let mut t = vec![
            super::tool(
                "calendars_list",
                "List this account's calendars (name and id).",
                json!({}),
                &[],
                true,
            ),
            super::tool(
                "events_list",
                "List events in a calendar between two days (default: the next 7 days), repeats worked out. Each has an id for event_update and event_delete.",
                json!({ "calendar": s("A name or id from calendars_list; default the first"), "from": s("Like 2026-10-22"), "to": s("Like 2026-10-29 (that day included)"), "text": s("Only events whose title, place or notes contain this"), "limit": { "type": "integer", "maximum": MAX_EVENTS } }),
                &[],
                true,
            ),
        ];
        if self.write {
            t.push(super::tool(
                "event_create",
                "Create an event. `end` defaults to an hour after `start` (the next day for an all-day event).",
                json!({ "calendar": s("A name or id; default the first"), "summary": s("The title"), "start": s(when), "end": s(when), "location": s(""), "description": s("") }),
                &["summary", "start"],
                false,
            ));
            t.push(super::tool(
                "event_update",
                "Change an event by its id from events_list: only the fields given change. On a repeating event the whole series changes.",
                json!({ "id": s("From events_list"), "summary": s(""), "start": s(when), "end": s(when), "location": s(""), "description": s("") }),
                &["id"],
                false,
            ));
            let mut del = super::tool(
                "event_delete",
                "Delete an event by its id from events_list (a repeating one: the whole series).",
                json!({ "id": s("From events_list") }),
                &["id"],
                false,
            );
            del["annotations"]["destructiveHint"] = json!(true);
            t.push(del);
        }
        t
    }

    async fn call(&self, name: &str, args: Value) -> Result<String, String> {
        match name {
            "calendars_list" => self.list_calendars().await,
            "events_list" => self.events(&args).await,
            "event_create" | "event_update" | "event_delete" if !self.write => {
                Err("this connection is read-only".into())
            }
            "event_create" => self.create(&args).await,
            "event_update" => self.update(&args).await,
            "event_delete" => self.delete(&args).await,
            other => Err(format!("no tool `{other}`")),
        }
    }
}

/// A calendar's secret `.ics` address, read-only.
pub struct Ics {
    id: String,
    url: url::Url,
    hosts: Vec<String>,
    http: reqwest::Client,
}

impl Ics {
    pub fn new(
        id: &str,
        fields: &BTreeMap<String, String>,
        ep: &Endpoints,
    ) -> Result<Self, String> {
        let raw = fields
            .get("secret_url")
            .filter(|v| !v.is_empty())
            .ok_or("the calendar's secret address is missing")?;
        let url = address(raw, ep)?;
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        Ok(Self {
            id: format!("native:ics:{id}"),
            url,
            hosts: vec![host],
            http: client(),
        })
    }

    async fn fetch(&self) -> Result<String, String> {
        let a = send(
            &self.http,
            &self.hosts,
            None,
            Method::GET,
            self.url.clone(),
            &[("Accept", "text/calendar".into())],
            None,
            "the calendar's address",
        )
        .await?;
        match a.status {
            200 => {}
            401 | 403 | 404 => {
                return Err(format!(
                    "the calendar's address answered {}: it was probably reset or unpublished. \
                     Copy the current secret address and connect again.",
                    a.status
                ))
            }
            s => return Err(format!("the calendar's address answered {s}")),
        }
        if !a.body.contains("BEGIN:VCALENDAR") {
            return Err("the address answered with something that isn't a calendar".into());
        }
        Ok(a.body)
    }

    pub async fn check(&self) -> Result<String, String> {
        let body = self.fetch().await?;
        let cals = ical::parse(&body);
        let mut events = Vec::new();
        for c in &cals {
            c.all("VEVENT", &mut events);
        }
        let name = cals
            .first()
            .and_then(|c| c.prop("X-WR-CALNAME"))
            .map(|p| ical::unescape(&p.value))
            .unwrap_or_else(|| "the calendar".into());
        Ok(format!("{name} reads, with {} event(s)", events.len()))
    }
}

#[async_trait]
impl LocalServer for Ics {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn tools(&self) -> Vec<Value> {
        let s = |d: &str| json!({ "type": "string", "description": d });
        vec![super::tool(
            "events_list",
            "List this calendar's events between two days (default: the next 7 days). Read-only: this calendar can't be changed from here.",
            json!({ "from": s("Like 2026-10-22"), "to": s("Like 2026-10-29 (that day included)"), "text": s("Only events whose title, place or notes contain this"), "limit": { "type": "integer", "maximum": MAX_EVENTS } }),
            &[],
            true,
        )]
    }

    async fn call(&self, name: &str, args: Value) -> Result<String, String> {
        match name {
            "events_list" => {
                let (from, to) = range(&args)?;
                let body = self.fetch().await?;
                let label = ical::parse(&body)
                    .first()
                    .and_then(|c| c.prop("X-WR-CALNAME"))
                    .map(|p| ical::unescape(&p.value))
                    .unwrap_or_else(|| "the calendar".into());
                let mut found = events_in(&body, from, to, None);
                Ok(show(&mut found, &label, &args))
            }
            other => Err(format!("no tool `{other}`")),
        }
    }
}

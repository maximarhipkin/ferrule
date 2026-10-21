//! M63.3: iCalendar (RFC 5545), as much as a calendar's events need:
//! reading `VEVENT`s (unfolded lines, parameters, escaped text, dates,
//! times and their zone, durations, `RRULE`, `EXDATE`, `RECURRENCE-ID`),
//! working out the common repeat rules over a range, and writing one event.
//!
//! Times are compared on each event's own clock: a `TZID` or floating time
//! is taken as written, a `Z` time as UTC. No time zone database is
//! consulted, so an event near the edge of a range can be off by the zone's
//! offset; the times shown are the event's own, with its zone named.

use super::mime::civil;

/// One property line: `NAME;PARAM=V:value`.
#[derive(Debug, Clone)]
pub struct Prop {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

impl Prop {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// `BEGIN:X` … `END:X`.
#[derive(Debug, Clone, Default)]
pub struct Component {
    pub name: String,
    pub props: Vec<Prop>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| p.name == name)
    }

    /// Every component named `name` at any depth.
    pub fn all<'a>(&'a self, name: &str, out: &mut Vec<&'a Component>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            }
            c.all(name, out);
        }
    }
}

/// Lines with their continuations joined (RFC 5545 §3.1).
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match (line.strip_prefix([' ', '\t']), out.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ if line.is_empty() => {}
            _ => out.push(line.to_string()),
        }
    }
    out
}

fn parse_line(line: &str) -> Option<Prop> {
    // The value starts at the first `:` outside a quoted parameter.
    let mut quoted = false;
    let colon = line.char_indices().find_map(|(i, c)| match c {
        '"' => {
            quoted = !quoted;
            None
        }
        ':' if !quoted => Some(i),
        _ => None,
    })?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    let params = parts
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_ascii_uppercase(),
                v.trim_matches('"').to_string(),
            )
        })
        .collect();
    Some(Prop {
        name,
        params,
        value: value.to_string(),
    })
}

/// The top-level components (`VCALENDAR`s) of `text`.
pub fn parse(text: &str) -> Vec<Component> {
    let mut stack: Vec<Component> = vec![Component::default()];
    for line in unfold(text) {
        let Some(p) = parse_line(&line) else {
            continue;
        };
        match p.name.as_str() {
            "BEGIN" => stack.push(Component {
                name: p.value.trim().to_ascii_uppercase(),
                ..Default::default()
            }),
            "END" if stack.len() > 1 => {
                let done = stack.pop().unwrap_or_default();
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(done);
                }
            }
            _ => {
                if let Some(top) = stack.last_mut() {
                    top.props.push(p);
                }
            }
        }
    }
    // An unclosed component still counts.
    while stack.len() > 1 {
        let done = stack.pop().unwrap_or_default();
        if let Some(parent) = stack.last_mut() {
            parent.children.push(done);
        }
    }
    stack.pop().map(|root| root.children).unwrap_or_default()
}

/// TEXT's escapes undone (`\n`, `\,`, `\;`, `\\`).
pub fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

/// TEXT escaped for a property value.
pub fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
        .replace('\r', "")
}

/// Which clock a time is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Zone {
    Utc,
    Tz(String),
    Floating,
}

/// A `DATE` or `DATE-TIME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct When {
    pub y: i64,
    pub mo: u32,
    pub d: u32,
    /// Minutes into the day; `None` for an all-day date.
    pub minute: Option<u32>,
    pub zone: Zone,
}

impl When {
    /// Minutes since 1970 on the event's own clock (see the module docs).
    pub fn key(&self) -> i64 {
        days_from_civil(self.y, self.mo, self.d) * 1440 + i64::from(self.minute.unwrap_or(0))
    }

    fn with_key(&self, key: i64) -> When {
        let (y, mo, d) = civil(key.div_euclid(1440));
        When {
            y,
            mo,
            d,
            minute: self.minute.map(|_| key.rem_euclid(1440) as u32),
            zone: self.zone.clone(),
        }
    }

    pub fn show(&self) -> String {
        let date = format!("{:04}-{:02}-{:02}", self.y, self.mo, self.d);
        match (self.minute, &self.zone) {
            (None, _) => date,
            (Some(m), zone) => {
                let t = format!("{date} {:02}:{:02}", m / 60, m % 60);
                match zone {
                    Zone::Utc => format!("{t} UTC"),
                    Zone::Tz(tz) => format!("{t} ({tz})"),
                    Zone::Floating => t,
                }
            }
        }
    }
}

/// `p`'s date or date-time.
pub fn when(p: &Prop) -> Option<When> {
    let v = p.value.trim();
    let date = v.get(..8)?;
    let (y, mo, d) = (
        date.get(..4)?.parse().ok()?,
        date.get(4..6)?.parse().ok()?,
        date.get(6..8)?.parse().ok()?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let Some(time) = v.get(8..).filter(|t| t.starts_with('T')) else {
        return Some(When {
            y,
            mo,
            d,
            minute: None,
            zone: Zone::Floating,
        });
    };
    let h: u32 = time.get(1..3)?.parse().ok()?;
    let mi: u32 = time.get(3..5)?.parse().ok()?;
    let zone = if time.ends_with('Z') {
        Zone::Utc
    } else if let Some(tz) = p.param("TZID") {
        Zone::Tz(tz.to_string())
    } else {
        Zone::Floating
    };
    Some(When {
        y,
        mo,
        d,
        minute: Some((h * 60 + mi).min(1439)),
        zone,
    })
}

/// A duration (`PT1H30M`, `P1D`, `P2W`) in minutes.
pub fn duration(v: &str) -> Option<i64> {
    let v = v.trim();
    let (neg, v) = match v.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let v = v.strip_prefix('P')?;
    let mut total = 0i64;
    let mut num = String::new();
    let mut in_time = false;
    for c in v.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            _ => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match (c, in_time) {
                    ('W', _) => 7 * 1440,
                    ('D', _) => 1440,
                    ('H', true) => 60,
                    ('M', true) => 1,
                    ('S', true) => 0,
                    _ => return None,
                };
            }
        }
    }
    Some(if neg { -total } else { total })
}

/// What a calendar event says, with its text unescaped.
#[derive(Debug, Clone)]
pub struct Event {
    pub uid: String,
    pub summary: String,
    pub location: String,
    pub description: String,
    pub status: String,
    pub start: When,
    /// Minutes from start to end.
    pub length: i64,
    pub rrule: Option<String>,
    pub exdates: Vec<i64>,
    pub recurrence_id: Option<i64>,
}

impl Event {
    pub fn from(c: &Component) -> Option<Event> {
        let text = |n: &str| c.prop(n).map(|p| unescape(&p.value)).unwrap_or_default();
        let start = when(c.prop("DTSTART")?)?;
        let length = match (c.prop("DTEND").and_then(when), c.prop("DURATION")) {
            (Some(end), _) => (end.key() - start.key()).max(0),
            (None, Some(d)) => duration(&d.value).unwrap_or(0).max(0),
            (None, None) if start.minute.is_none() => 1440,
            (None, None) => 0,
        };
        let mut exdates = Vec::new();
        for p in c.props.iter().filter(|p| p.name == "EXDATE") {
            for v in p.value.split(',') {
                let one = Prop {
                    value: v.to_string(),
                    ..p.clone()
                };
                if let Some(w) = when(&one) {
                    exdates.push(w.key());
                }
            }
        }
        Some(Event {
            uid: text("UID"),
            summary: text("SUMMARY"),
            location: text("LOCATION"),
            description: text("DESCRIPTION"),
            status: text("STATUS"),
            rrule: c.prop("RRULE").map(|p| p.value.trim().to_string()),
            recurrence_id: c.prop("RECURRENCE-ID").and_then(when).map(|w| w.key()),
            start,
            length,
            exdates,
        })
    }

    /// The occurrences that overlap `[from, to)` (keys), at most `max`;
    /// `Err` with the rule when ferrule can't work out its repeats.
    pub fn occurrences(&self, from: i64, to: i64, max: usize) -> Result<Vec<When>, String> {
        let overlaps = |s: i64| s < to && s + self.length.max(1) > from;
        let Some(rule) = &self.rrule else {
            return Ok(if overlaps(self.start.key()) {
                vec![self.start.clone()]
            } else {
                Vec::new()
            });
        };
        let r = Rule::parse(rule, &self.start).ok_or_else(|| rule.clone())?;
        let mut out = Vec::new();
        for (i, start) in r.starts(&self.start).enumerate() {
            if start >= to || r.until.is_some_and(|u| start > u) {
                break;
            }
            let seen = i as u32 + 1;
            if !self.exdates.contains(&start) && overlaps(start) {
                out.push(self.start.with_key(start));
                if out.len() >= max {
                    break;
                }
            }
            if r.count.is_some_and(|c| seen >= c) {
                break;
            }
        }
        Ok(out)
    }
}

/// The repeat rules ferrule works out itself: DAILY, WEEKLY (with plain
/// `BYDAY`), MONTHLY (on the start's day, or one `BYDAY` like `3TU` or
/// `-1FR`) and YEARLY (on the start's date), with INTERVAL, COUNT and
/// UNTIL. Anything else is left for the reader, said as such.
struct Rule {
    freq: Freq,
    interval: i64,
    count: Option<u32>,
    until: Option<i64>,
    /// WEEKLY: weekdays (0 = Sunday).
    days: Vec<u32>,
    /// MONTHLY: the nth (or, negative, nth from last) weekday.
    nth: Option<(i32, u32)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

const WEEKDAYS: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];

impl Rule {
    fn parse(rule: &str, start: &When) -> Option<Rule> {
        let mut r = Rule {
            freq: Freq::Daily,
            interval: 1,
            count: None,
            until: None,
            days: Vec::new(),
            nth: None,
        };
        let mut freq = None;
        for part in rule.split(';') {
            let (k, v) = part.split_once('=')?;
            match k.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    freq = Some(match v {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        _ => return None,
                    })
                }
                "INTERVAL" => r.interval = v.parse().ok().filter(|n: &i64| *n > 0)?,
                "COUNT" => r.count = Some(v.parse().ok()?),
                "UNTIL" => {
                    let p = Prop {
                        name: "UNTIL".into(),
                        params: Vec::new(),
                        value: v.to_string(),
                    };
                    let u = when(&p)?;
                    // A date UNTIL takes in that whole day.
                    r.until = Some(u.key() + if u.minute.is_none() { 1439 } else { 0 });
                }
                "BYDAY" => {
                    for d in v.split(',') {
                        let d = d.trim();
                        let wd = WEEKDAYS.iter().position(|w| d.ends_with(w))? as u32;
                        let ord = &d[..d.len() - 2];
                        if ord.is_empty() {
                            r.days.push(wd);
                        } else {
                            if r.nth.is_some() {
                                return None;
                            }
                            r.nth = Some((ord.parse().ok().filter(|n: &i32| *n != 0)?, wd));
                        }
                    }
                }
                "BYMONTHDAY" if v.trim().parse::<u32>().ok() == Some(start.d) => {}
                "BYMONTH" if v.trim().parse::<u32>().ok() == Some(start.mo) => {}
                "WKST" => {}
                _ => return None,
            }
        }
        r.freq = freq?;
        let fits = match r.freq {
            Freq::Weekly => r.nth.is_none(),
            Freq::Monthly => r.days.is_empty(),
            Freq::Daily | Freq::Yearly => r.days.is_empty() && r.nth.is_none(),
        };
        fits.then_some(r)
    }

    /// Candidate starts from `start`, in order (bounded).
    fn starts(&self, start: &When) -> impl Iterator<Item = i64> {
        let mut out = Vec::new();
        let base_day = days_from_civil(start.y, start.mo, start.d);
        let minute = i64::from(start.minute.unwrap_or(0));
        let limit = 20_000;
        match self.freq {
            Freq::Daily => {
                for i in 0..limit {
                    out.push((base_day + i * self.interval) * 1440 + minute);
                }
            }
            Freq::Weekly => {
                let days = if self.days.is_empty() {
                    vec![weekday(base_day)]
                } else {
                    let mut d = self.days.clone();
                    d.sort_unstable();
                    d
                };
                // The week (from Sunday) the start is in.
                let week0 = base_day - i64::from(weekday(base_day));
                'weeks: for w in 0..limit / 7 {
                    for &wd in &days {
                        let day = week0 + w * 7 * self.interval + i64::from(wd);
                        if day < base_day {
                            continue;
                        }
                        out.push(day * 1440 + minute);
                        if out.len() >= limit as usize {
                            break 'weeks;
                        }
                    }
                }
            }
            Freq::Monthly | Freq::Yearly => {
                let step = if self.freq == Freq::Monthly {
                    self.interval
                } else {
                    12 * self.interval
                };
                for i in 0..limit / 4 {
                    let months = i64::from(start.mo) - 1 + i * step;
                    let (y, mo) = (
                        start.y + months.div_euclid(12),
                        months.rem_euclid(12) as u32 + 1,
                    );
                    let day = match self.nth {
                        Some((n, wd)) => nth_weekday(y, mo, n, wd),
                        None => (start.d <= days_in_month(y, mo)).then_some(start.d),
                    };
                    if let Some(d) = day {
                        let k = days_from_civil(y, mo, d);
                        if k >= base_day {
                            out.push(k * 1440 + minute);
                        }
                    }
                }
            }
        }
        out.into_iter()
    }
}

/// 0 = Sunday.
fn weekday(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

fn days_in_month(y: i64, mo: u32) -> u32 {
    match mo {
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

/// The `n`th `wd` of the month (negative: from the end).
fn nth_weekday(y: i64, mo: u32, n: i32, wd: u32) -> Option<u32> {
    let len = days_in_month(y, mo);
    let first = weekday(days_from_civil(y, mo, 1));
    let firsts: Vec<u32> = (1..=len).filter(|d| (first + d - 1) % 7 == wd).collect();
    let i = if n > 0 {
        n as usize - 1
    } else {
        firsts.len().checked_sub(n.unsigned_abs() as usize)?
    };
    firsts.get(i).copied()
}

/// Days since 1970-01-01 (the inverse of `mime::civil`).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A key as iCalendar's UTC form (`20261021T090000Z`).
pub fn utc_stamp(key: i64) -> String {
    let (y, mo, d) = civil(key.div_euclid(1440));
    let m = key.rem_euclid(1440);
    format!("{y:04}{mo:02}{d:02}T{:02}{:02}00Z", m / 60, m % 60)
}

/// What a tool's `start`/`end`/`from`/`to` argument means: `2026-10-22`
/// (a date), `2026-10-22T09:00` (floating: that time wherever the owner
/// is), or with `Z` or `+03:00` (a fixed instant, kept as UTC).
pub fn arg_time(v: &str) -> Result<When, String> {
    let bad = || {
        format!("`{v}` isn't a date or time like 2026-10-22, 2026-10-22T09:00 or 2026-10-22T09:00+03:00")
    };
    let v = v.trim();
    let (date, time) = match v.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (v, None),
    };
    let nums: Vec<&str> = date.split('-').collect();
    let [y, mo, d] = nums.as_slice() else {
        return Err(bad());
    };
    let (y, mo, d): (i64, u32, u32) = (
        y.parse().map_err(|_| bad())?,
        mo.parse().map_err(|_| bad())?,
        d.parse().map_err(|_| bad())?,
    );
    if !(1..=12).contains(&mo) || d == 0 || d > days_in_month(y, mo) {
        return Err(bad());
    }
    let Some(time) = time else {
        return Ok(When {
            y,
            mo,
            d,
            minute: None,
            zone: Zone::Floating,
        });
    };
    let (clock, offset) = if let Some(c) = time.strip_suffix(['Z', 'z']) {
        (c, Some(0i64))
    } else if let Some(at) = time.rfind(['+', '-']) {
        let (c, off) = time.split_at(at);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let hm: Vec<&str> = off[1..].split(':').collect();
        let (h, m): (i64, i64) = match hm.as_slice() {
            [h, m] => (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?),
            [hm] if hm.len() == 4 => (
                hm[..2].parse().map_err(|_| bad())?,
                hm[2..].parse().map_err(|_| bad())?,
            ),
            _ => return Err(bad()),
        };
        (c, Some(sign * (h * 60 + m)))
    } else {
        (time, None)
    };
    let hms: Vec<&str> = clock.split(':').collect();
    let (h, mi): (u32, u32) = match hms.as_slice() {
        [h, m] | [h, m, _] => (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?),
        _ => return Err(bad()),
    };
    if h > 23 || mi > 59 {
        return Err(bad());
    }
    let local = When {
        y,
        mo,
        d,
        minute: Some(h * 60 + mi),
        zone: Zone::Floating,
    };
    Ok(match offset {
        None => local,
        Some(off) => {
            let utc = local.key() - off;
            When {
                zone: Zone::Utc,
                ..local.with_key(utc)
            }
        }
    })
}

/// `DTSTART`-style line for `w`.
pub fn time_line(name: &str, w: &When) -> String {
    let date = format!("{:04}{:02}{:02}", w.y, w.mo, w.d);
    match (w.minute, &w.zone) {
        (None, _) => format!("{name};VALUE=DATE:{date}"),
        (Some(m), Zone::Utc) => format!("{name}:{date}T{:02}{:02}00Z", m / 60, m % 60),
        (Some(m), Zone::Tz(tz)) => format!("{name};TZID={tz}:{date}T{:02}{:02}00", m / 60, m % 60),
        (Some(m), Zone::Floating) => format!("{name}:{date}T{:02}{:02}00", m / 60, m % 60),
    }
}

/// A content line folded at 75 octets (RFC 5545 §3.1), CRLF-ended.
pub fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in line.chars() {
        if used + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            used = 1;
        }
        out.push(c);
        used += c.len_utf8();
    }
    out.push_str("\r\n");
    out
}

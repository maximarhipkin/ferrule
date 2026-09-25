//! Today's spend, and each scheduled task's, read back from the ledger.
//!
//! The ledger is the one record every process writes to (`ferrule run`,
//! the gateway, the scheduler), so the day and task caps count what all of
//! them spent. The file is read once from the start of the day and then
//! only its new tail; a new day starts over.

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use ferrule_core::LedgerRecord;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;

/// Scheduler sessions are `scheduler__<task id>`.
pub const TASK_PREFIX: &str = "scheduler__";

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spend {
    pub tokens: u64,
    pub usd: f64,
}

impl Spend {
    pub fn of(r: &LedgerRecord) -> Self {
        Self {
            tokens: r.input_tokens + r.output_tokens,
            usd: r.cost_usd.unwrap_or(0.0),
        }
    }

    pub fn add(&mut self, other: Spend) {
        self.tokens += other.tokens;
        self.usd += other.usd;
    }
}

/// The scheduled task a row belongs to, if any.
pub fn task_of(r: &LedgerRecord) -> Option<&str> {
    r.tree
        .as_deref()
        .unwrap_or(&r.session_id)
        .strip_prefix(TASK_PREFIX)
}

/// Whether a row is the owner's spend. `ferrule eval` rows aren't, unless
/// the suite opted into the owner's caps (its rows then carry a tree).
pub fn counts(r: &LedgerRecord) -> bool {
    r.eval.is_none() || r.tree.is_some()
}

pub struct Meter {
    path: PathBuf,
    tz: Tz,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    day: Option<NaiveDate>,
    offset: u64,
    today: Spend,
    tasks: HashMap<String, Spend>,
}

impl Meter {
    pub fn new(path: PathBuf, tz: Tz) -> Self {
        Self {
            path,
            tz,
            state: Mutex::new(State::default()),
        }
    }

    pub fn day(&self, now: DateTime<Utc>) -> NaiveDate {
        now.with_timezone(&self.tz).date_naive()
    }

    /// Today's spend, and `task`'s today. No ledger yet is nothing spent;
    /// a ledger that can't be read is an error (the caller fails closed).
    pub fn read(&self, now: DateTime<Utc>, task: Option<&str>) -> Result<(Spend, Spend), String> {
        let day = self.day(now);
        let mut st = self.state.lock().unwrap();
        if st.day != Some(day) {
            *st = State {
                day: Some(day),
                ..State::default()
            };
        }
        self.refresh(&mut st, day)?;
        let task = task
            .and_then(|t| st.tasks.get(t))
            .copied()
            .unwrap_or_default();
        Ok((st.today, task))
    }

    fn refresh(&self, st: &mut State, day: NaiveDate) -> Result<(), String> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                *st = State {
                    day: Some(day),
                    ..State::default()
                };
                return Ok(());
            }
            Err(e) => return Err(self.unreadable(e)),
        };
        let len = file.metadata().map_err(|e| self.unreadable(e))?.len();
        if len < st.offset {
            // Rotated or rewritten: count it again from the start.
            *st = State {
                day: Some(day),
                ..State::default()
            };
        }
        if len == st.offset {
            return Ok(());
        }
        file.seek(SeekFrom::Start(st.offset))
            .map_err(|e| self.unreadable(e))?;
        let mut buf = Vec::new();
        file.take(len - st.offset)
            .read_to_end(&mut buf)
            .map_err(|e| self.unreadable(e))?;
        // Only whole lines: a row being written now is read next time.
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
            return Ok(());
        };
        for line in buf[..end].split(|&b| b == b'\n') {
            let Ok(r) = serde_json::from_slice::<LedgerRecord>(line) else {
                continue;
            };
            if !counts(&r) {
                continue;
            }
            let Ok(at) = DateTime::parse_from_rfc3339(&r.timestamp) else {
                continue;
            };
            if at.with_timezone(&self.tz).date_naive() != day {
                continue;
            }
            let spend = Spend::of(&r);
            st.today.add(spend);
            if let Some(task) = task_of(&r) {
                st.tasks.entry(task.to_string()).or_default().add(spend);
            }
        }
        st.offset += end as u64 + 1;
        Ok(())
    }

    fn unreadable(&self, e: std::io::Error) -> String {
        format!("the ledger {} can't be read ({e})", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn row(at: &str, session: &str, tokens: u64, usd: f64) -> LedgerRecord {
        LedgerRecord {
            timestamp: at.into(),
            session_id: session.into(),
            task_shape: "run".into(),
            origin: None,
            provider: "p".into(),
            model: "m".into(),
            iteration: 0,
            call_kind: "turn".into(),
            input_tokens: tokens,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            tool_calls: 0,
            latency_ms: 1,
            outcome: "ok".into(),
            error_kind: None,
            error_message: None,
            cost_usd: Some(usd),
            eval: None,
            tree: Some(session.into()),
        }
    }

    fn append(path: &std::path::Path, r: &LedgerRecord) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn counts_today_in_the_zone_and_follows_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let tz: Tz = "Asia/Jerusalem".parse().unwrap();
        let m = Meter::new(path.clone(), tz);
        let now = at("2026-09-25T10:00:00Z");
        assert_eq!(
            m.read(now, None).unwrap().0,
            Spend::default(),
            "no ledger yet"
        );

        // 21:30 UTC on the 24th is 00:30 on the 25th in Jerusalem.
        append(&path, &row("2026-09-24T21:30:00+00:00", "a", 100, 1.0));
        append(&path, &row("2026-09-24T20:30:00+00:00", "a", 1000, 9.0));
        append(
            &path,
            &row("2026-09-25T09:00:00+00:00", "scheduler__daily", 10, 0.5),
        );
        let (today, task) = m.read(now, Some("daily")).unwrap();
        assert_eq!(today.tokens, 110);
        assert!((today.usd - 1.5).abs() < 1e-9);
        assert_eq!(task.tokens, 10);

        append(
            &path,
            &row("2026-09-25T09:30:00+00:00", "scheduler__daily", 5, 0.25),
        );
        let (today, task) = m.read(now, Some("daily")).unwrap();
        assert_eq!((today.tokens, task.tokens), (115, 15));

        // Midnight in Jerusalem: a new day starts from nothing.
        let (today, _) = m.read(at("2026-09-25T21:00:01Z"), None).unwrap();
        assert_eq!(today.tokens, 0);
    }

    #[test]
    fn eval_rows_count_only_with_a_tree_and_half_lines_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let m = Meter::new(path.clone(), chrono_tz::UTC);
        let mut e = row("2026-09-25T09:00:00+00:00", "eval-1", 500, 2.0);
        e.eval = Some(ferrule_core::ledger::EvalTag {
            run_id: "r".into(),
            suite: "s".into(),
            kind: "capability".into(),
            task: "t".into(),
            variant: "engineered".into(),
            repeat: 0,
            result: None,
        });
        let mut hermetic = e.clone();
        hermetic.tree = None;
        append(&path, &hermetic);
        append(&path, &e);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(f, "{{\"timestamp\":").unwrap();
        let now = at("2026-09-25T10:00:00Z");
        assert_eq!(m.read(now, None).unwrap().0.tokens, 500);
        let rest = serde_json::to_string(&row("2026-09-25T09:00:00+00:00", "x", 7, 0.0)).unwrap();
        writeln!(f, "{}", &rest["{\"timestamp\":".len()..]).unwrap();
        assert_eq!(m.read(now, None).unwrap().0.tokens, 507);
    }

    #[test]
    fn a_shorter_ledger_is_counted_again_and_an_unreadable_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let m = Meter::new(path.clone(), chrono_tz::UTC);
        let now = at("2026-09-25T10:00:00Z");
        append(&path, &row("2026-09-25T09:00:00+00:00", "a", 100, 0.0));
        append(&path, &row("2026-09-25T09:00:00+00:00", "a", 100, 0.0));
        assert_eq!(m.read(now, None).unwrap().0.tokens, 200);
        std::fs::remove_file(&path).unwrap();
        append(&path, &row("2026-09-25T09:00:00+00:00", "a", 3, 0.0));
        assert_eq!(m.read(now, None).unwrap().0.tokens, 3);

        let dir_as_ledger = Meter::new(dir.path().to_path_buf(), chrono_tz::UTC);
        assert!(dir_as_ledger.read(now, None).is_err());
    }
}

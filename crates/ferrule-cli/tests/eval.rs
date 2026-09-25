//! The real `ferrule` binary running `ferrule eval` on the starter suite's
//! smoke tasks, against the suite's mock model (`evals/starter/mock`):
//! the A/B report, the exit codes, the budget cap and the dry run. Needs
//! `python3` (the suite's graders and the mock are Python); without it
//! the tests say so and pass.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

fn suite() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/starter")
}

fn have_python() -> bool {
    let ok = Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("python3 not found: skipping (the starter suite's graders need it)");
    }
    ok
}

/// The mock model on a free port, stopped on drop.
struct Mock {
    child: Child,
    url: String,
}

impl Mock {
    fn start() -> Mock {
        Self::spawn(&suite().join("mock/model.py"), &[])
    }

    /// M25: `evals/routing/weak_mock.py`, weak or not.
    fn routing(weak: bool) -> Mock {
        let script = suite().join("../routing/weak_mock.py");
        Self::spawn(&script, if weak { &["--weak"] } else { &[] })
    }

    fn spawn(script: &Path, extra: &[&str]) -> Mock {
        let mut child = Command::new("python3")
            .arg(script)
            .args(["--port", "0"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("starting the mock model");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .trim()
            .rsplit(' ')
            .next()
            .expect("the mock prints its URL")
            .to_string();
        assert!(url.starts_with("http://127.0.0.1:"), "{line}");
        Mock { child, url }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn home(url: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for d in ["work", "data", "home", "tmp"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    std::fs::write(
        dir.path().join("ferrule.toml"),
        format!(
            r#"default_provider = "mock"

[providers.mock]
base_url = "{url}"
api_key_env = "FERRULE_TEST_KEY"
model = "mock"
price_input_per_mtok = 1.0
price_cached_input_per_mtok = 0.1
price_output_per_mtok = 5.0

[skills]
enabled = false

[sandbox]
mode = "off"
"#
        ),
    )
    .unwrap();
    dir
}

fn ferrule(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args(args)
        .current_dir(home.join("work"))
        .env("FERRULE_CONFIG", home.join("ferrule.toml"))
        .env("FERRULE_DATA_DIR", home.join("data"))
        .env("FERRULE_TEST_KEY", "sk-test")
        .env("TMPDIR", home.join("tmp"));
    for var in [
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "APPDATA",
        "LOCALAPPDATA",
    ] {
        cmd.env(var, home.join("home"));
    }
    for var in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(var);
    }
    cmd.output().unwrap()
}

fn texts(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The row for `task` in the report's first table.
fn row<'a>(report: &'a str, task: &str) -> &'a str {
    report
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{task} ")))
        .unwrap_or_else(|| panic!("no row for {task} in:\n{report}"))
}

#[test]
fn the_smoke_ab_passes_under_ferrule_and_loses_the_context_and_verify_tasks_naive() {
    if !have_python() {
        return;
    }
    let mock = Mock::start();
    let home = home(&mock.url);
    let suite = suite();
    let out = ferrule(
        home.path(),
        &[
            "eval",
            "run",
            suite.to_str().unwrap(),
            "--tag",
            "smoke",
            "--variant",
            "ab",
        ],
    );
    let (stdout, stderr) = texts(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");

    // Engineered passes all four; naive loses the request to truncation on
    // release-notes and never sees slugify's check.
    for task in ["fix-median", "sales-summary"] {
        let r = row(&stdout, task);
        assert_eq!(r.matches("pass").count(), 2, "{r}");
    }
    let r = row(&stdout, "release-notes");
    assert!(r.contains("pass (") && r.contains("compact"), "{r}");
    assert!(r.contains("FAIL (") && r.contains("trunc"), "{r}");
    let r = row(&stdout, "slugify");
    assert!(r.contains("pass (1×check)") && r.contains("FAIL"), "{r}");
    assert!(stdout.contains("4/4 (100%)"), "{stdout}");
    assert!(stdout.contains("2/4 (50%)"), "{stdout}");
    assert!(stdout.contains("+50 pts"), "{stdout}");
    assert!(
        stdout.contains("release-notes (naive): RELEASE_NOTES.md"),
        "{stdout}"
    );
    assert!(stdout.contains("slugify (naive): rules:"), "{stdout}");
    // Progress went to stderr, the report to stdout and to disk.
    assert!(stderr.contains("slugify"), "{stderr}");
    let dir = stdout
        .lines()
        .find_map(|l| l.strip_prefix("transcripts and this report: "))
        .expect("the run's directory");
    let saved = std::fs::read_to_string(Path::new(dir).join("report.txt")).unwrap();
    assert!(saved.contains("+50 pts"));
    assert!(Path::new(dir).join("run.json").is_file());
    // Every workspace was cleaned up.
    let left: Vec<_> = walk(&home.path().join("tmp"))
        .into_iter()
        .filter(|p| p.ends_with("ws"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
            out.push(p);
        }
    }
    out
}

#[test]
fn the_budget_cap_stops_the_binary_with_exit_code_3() {
    if !have_python() {
        return;
    }
    let mock = Mock::start();
    let home = home(&mock.url);
    let suite = suite();
    let out = ferrule(
        home.path(),
        &[
            "eval",
            "run",
            suite.to_str().unwrap(),
            "--tag",
            "smoke",
            "--max-tokens",
            "3000",
        ],
    );
    let (stdout, stderr) = texts(&out);
    assert_eq!(out.status.code(), Some(3), "{stdout}\n{stderr}");
    assert!(stdout.contains("STOPPED by the budget"), "{stdout}");
    assert!(stdout.contains("not started"), "{stdout}");
}

#[test]
fn the_dry_run_prices_the_worst_case_and_calls_nothing() {
    // No model at all: the URL goes nowhere.
    let home = home("http://127.0.0.1:9/v1");
    let suite = suite();
    let out = ferrule(
        home.path(),
        &[
            "eval",
            "run",
            suite.to_str().unwrap(),
            "--tag",
            "smoke",
            "--variant",
            "ab",
            "--dry-run",
        ],
    );
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    for want in [
        "dry run — suite starter",
        "nothing is sent to the model",
        "context window 32.0k",
        "release-notes [smoke,context]",
        "8 task run(s): 4 task(s) × engineered + naive × 1 repeat(s)",
        "worst-case cost: $",
        "budget cap: $5.00, 20.00M tokens",
    ] {
        assert!(stdout.contains(want), "{want:?} not in:\n{stdout}");
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// The run id a report names in its first line.
fn run_id(report: &str) -> String {
    report
        .lines()
        .find_map(|l| {
            l.strip_prefix("ferrule eval — ")
                .and_then(|l| l.rsplit(", run ").next())
        })
        .expect("the report's header")
        .to_string()
}

#[test]
fn a_second_run_reports_its_diff_against_the_first() {
    if !have_python() {
        return;
    }
    let mock = Mock::start();
    let home = home(&mock.url);
    // A copy of the suite, so a task can change between the runs.
    let suite = home.path().join("suite");
    copy_dir(&self::suite(), &suite);
    let args = [
        "eval",
        "run",
        suite.to_str().unwrap(),
        "--task",
        "fix-median",
        "--task",
        "sales-summary",
        "--variant",
        "naive",
    ];

    let out = ferrule(home.path(), &args);
    let (first, stderr) = texts(&out);
    assert_eq!(out.status.code(), Some(0), "{first}\n{stderr}");
    assert!(
        first.contains("naive: no earlier run to compare with"),
        "{first}"
    );
    let first_id = run_id(&first);

    // sales-summary's grader now rejects everything.
    let toml = std::fs::read_to_string(suite.join("suite.toml")).unwrap();
    let broken = toml.replace(
        r#"command = 'python3 "{suite_dir}/graders/sales-summary.py"'"#,
        r#"command = 'python3 -c "import sys; sys.exit(\"the grader changed\")"'"#,
    );
    assert_ne!(toml, broken);
    std::fs::write(suite.join("suite.toml"), broken).unwrap();

    let out = ferrule(home.path(), &args);
    let (second, stderr) = texts(&out);
    assert_eq!(out.status.code(), Some(0), "{second}\n{stderr}");
    let want = format!("naive vs run {first_id}: pass rate 2/2 (100%) → 1/2 (50%), -50 pts");
    assert!(second.contains(&want), "{want:?} not in:\n{second}");
    assert!(
        second.contains("NEWLY FAILING: sales-summary (task changed)"),
        "{second}"
    );
    let second_id = run_id(&second);

    // `eval report` re-prints the latest run with the same diff...
    let out = ferrule(home.path(), &["eval", "report"]);
    let (report, stderr) = texts(&out);
    assert!(out.status.success(), "{report}\n{stderr}");
    assert!(report.contains(&format!("run {second_id}")), "{report}");
    assert!(report.contains(&want), "{report}");
    // ...and an older one by id, which has nothing before it.
    let out = ferrule(
        home.path(),
        &["eval", "report", "starter", "--run", &first_id],
    );
    let (report, _) = texts(&out);
    assert!(out.status.success(), "{report}");
    assert!(report.contains(&format!("run {first_id}")), "{report}");
    assert!(
        report.contains("naive: no earlier run to compare with"),
        "{report}"
    );
    // An unknown suite is an error that says where it looked.
    let out = ferrule(home.path(), &["eval", "report", "nope"]);
    assert!(!out.status.success());
    assert!(texts(&out).1.contains("no saved eval run of suite `nope`"));
}

/// Counts connections to a port that nothing should call.
fn tripwire() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, hits)
}

/// M19b: an eval run is hermetic. With Telegram, a heartbeat, "back up"
/// on start, systemd's watchdog and an unclean-exit marker all set up, it
/// reacts to nothing, pings nothing, sends no notice and writes no marker.
#[test]
fn an_eval_run_sends_no_receipts_pings_heartbeats_or_notices() {
    if !have_python() {
        return;
    }
    let mock = Mock::start();
    let home = home(&mock.url);
    let (tg_port, tg_hits) = tripwire();
    let (beat_port, beat_hits) = tripwire();
    let mut toml = std::fs::read_to_string(home.path().join("ferrule.toml")).unwrap();
    toml.push_str(&format!(
        "\n[gateway]\ntelegram_token_env = \"FERRULE_TEST_TG\"\ntelegram_base_url = \"http://127.0.0.1:{tg_port}\"\ntelegram_allowed_chats = [42]\n\n[health]\nnotify_on_start = true\nheartbeat_url = \"http://127.0.0.1:{beat_port}/ping\"\nheartbeat_secs = 1\n"
    ));
    std::fs::write(home.path().join("ferrule.toml"), toml).unwrap();
    // What a killed gateway leaves: a stale marker with a turn in it.
    let gw = home.path().join("data/gateway");
    std::fs::create_dir_all(&gw).unwrap();
    let marker = r#"{"pid":999999,"version":"0.0.0","started":1,"turns":[{"place":"telegram chat 42","channel":"telegram","chat_id":"42","text":"hi"}]}"#;
    std::fs::write(gw.join("running.json"), marker).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(gw.join("running.json"))
        .unwrap()
        .set_modified(old)
        .unwrap();

    let suite = suite();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrule"));
    cmd.args([
        "eval",
        "run",
        suite.to_str().unwrap(),
        "--task",
        "fix-median",
        "--variant",
        "ab",
    ])
    .current_dir(home.path().join("work"))
    .env("FERRULE_CONFIG", home.path().join("ferrule.toml"))
    .env("FERRULE_DATA_DIR", home.path().join("data"))
    .env("FERRULE_TEST_KEY", "sk-test")
    .env("FERRULE_TEST_TG", "TESTTOKEN")
    .env("TMPDIR", home.path().join("tmp"));
    for var in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_DATA_HOME"] {
        cmd.env(var, home.path().join("home"));
    }
    for var in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "NOTIFY_SOCKET",
        "WATCHDOG_USEC",
        "WATCHDOG_PID",
    ] {
        cmd.env_remove(var);
    }
    #[cfg(unix)]
    let notify = {
        let path = home.path().join("notify");
        let rx = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        rx.set_nonblocking(true).unwrap();
        cmd.env("NOTIFY_SOCKET", &path)
            .env("WATCHDOG_USEC", "300000");
        rx
    };
    let out = cmd.output().unwrap();
    let (stdout, stderr) = texts(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(row(&stdout, "fix-median").contains("pass"), "{stdout}");
    // Give anything stray a moment to land.
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let hits = |h: &std::sync::atomic::AtomicUsize| h.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(hits(&tg_hits), 0, "the eval called Telegram");
    assert_eq!(hits(&beat_hits), 0, "the eval sent a heartbeat");
    #[cfg(unix)]
    {
        let mut buf = [0u8; 64];
        let got = notify.recv(&mut buf);
        assert!(
            got.is_err(),
            "the eval pinged systemd: {:?}",
            got.map(|n| String::from_utf8_lossy(&buf[..n]).into_owned())
        );
    }
    // The marker is left for the next gateway; no status file appeared.
    assert_eq!(
        std::fs::read_to_string(gw.join("running.json")).unwrap(),
        marker
    );
    assert!(!gw.join("status.txt").exists());
}

/// M24's estimate (`src/model_eval/typical.json`) is the mock's use on
/// each task: a harness change that moves it fails here, to be measured
/// again (docs/m24-dashboard-2.md §2). The smoke tasks only, one of each
/// kind, to keep this quick; paths differ by OS, hence the slack.
#[test]
fn the_eval_estimate_still_matches_the_mocks_use() {
    if !have_python() {
        return;
    }
    let typical: serde_json::Value =
        serde_json::from_str(include_str!("../src/model_eval/typical.json")).unwrap();
    let mock = Mock::start();
    let dir = home(&mock.url);
    let out = ferrule(
        dir.path(),
        &[
            "eval",
            "run",
            suite().to_str().unwrap(),
            "--tag",
            "smoke",
            "--variant",
            "engineered",
        ],
    );
    drop(mock);
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let saved = walk(&dir.path().join("data/eval"))
        .into_iter()
        .map(|d| d.join("run.json"))
        .find(|p| p.is_file())
        .expect("the run was saved");
    let run: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(saved).unwrap()).unwrap();
    let results = run["results"].as_array().unwrap();
    assert_eq!(results.len(), 4, "{run:#}");
    let near = |got: u64, want: u64| got.abs_diff(want) <= want / 5 + 200;
    for r in results {
        let task = r["task"].as_str().unwrap();
        let want = &typical["tasks"][task];
        let t = &r["totals"];
        let (i, o) = (
            t["input_tokens"].as_u64().unwrap(),
            t["output_tokens"].as_u64().unwrap(),
        );
        let (wi, wo) = (
            want["input"].as_u64().unwrap(),
            want["output"].as_u64().unwrap(),
        );
        assert!(
            near(i, wi) && near(o, wo),
            "{task}: the mock used {i} in / {o} out, typical.json says {wi} / {wo}: measure it again"
        );
        assert_eq!(t["calls"], want["calls"], "{task}");
    }
}

/// `home` plus a second provider, `weak`, at a tenth of the price.
fn routing_home(strong: &str, weak: &str, tiers: bool) -> tempfile::TempDir {
    let home = home(strong);
    let path = home.path().join("ferrule.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!(
        r#"
[providers.weak]
base_url = "{weak}"
api_key_env = "FERRULE_TEST_KEY"
model = "mock"
price_input_per_mtok = 0.1
price_cached_input_per_mtok = 0.01
price_output_per_mtok = 0.5

[models]
catalog_url = ""
"#
    ));
    if tiers {
        text.push_str("\n[routing]\ntiers = [\"weak/mock\", \"mock/mock\"]\n");
    }
    std::fs::write(&path, text).unwrap();
    home
}

#[test]
fn the_routing_variant_moves_the_weak_models_failed_check_up_and_prices_each_row() {
    if !have_python() {
        return;
    }
    let (weak, strong) = (Mock::routing(true), Mock::routing(false));
    // No [routing] at all: the eval takes the pair from the flags, and the
    // owner's routing (off) doesn't matter.
    let home = routing_home(&strong.url, &weak.url, false);
    let suite = suite();
    let out = ferrule(
        home.path(),
        &[
            "eval",
            "run",
            suite.to_str().unwrap(),
            "--tag",
            "smoke",
            "--variant",
            "routing",
            "--cheap",
            "weak/mock",
            "--strong",
            "mock/mock",
        ],
    );
    let (stdout, stderr) = texts(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("routing: cheap weak/mock, strong mock/mock"),
        "{stdout}"
    );
    // The weak model never fixes slugify's failed check; routed moves it up.
    let r = row(&stdout, "slugify");
    assert!(r.contains("FAIL (") && r.contains("×check)"), "{r}");
    assert!(r.contains("pass (1×check, 1×up)"), "{r}");
    for task in ["fix-median", "sales-summary", "release-notes"] {
        let r = row(&stdout, task);
        assert_eq!(r.matches("pass").count(), 3, "{r}");
        assert!(!r.contains("×up"), "{r}");
    }
    assert!(stdout.contains("3/4 (75%)"), "{stdout}");
    assert_eq!(stdout.matches("4/4 (100%)").count(), 2, "{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("escalations"))
        .expect(&stdout);
    assert!(line.contains("1 (check_failed×1)"), "{line}");

    // The ledger: every call priced at the model that served it.
    let ledger = std::fs::read_to_string(home.path().join("data/ledger.jsonl")).unwrap();
    let rows: Vec<serde_json::Value> = ledger
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|r: &serde_json::Value| r["call_kind"] != "eval_result")
        .collect();
    assert!(!rows.is_empty());
    for r in &rows {
        let (i, o) = (
            r["input_tokens"].as_f64().unwrap(),
            r["output_tokens"].as_f64().unwrap(),
        );
        let want = match r["provider"].as_str().unwrap() {
            "weak" => (i * 0.1 + o * 0.5) / 1e6,
            "mock" => (i * 1.0 + o * 5.0) / 1e6,
            other => panic!("a row from {other}"),
        };
        let cost = r["cost_usd"].as_f64().unwrap();
        assert!((cost - want).abs() < 1e-9, "{r}");
    }
    let up: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| r["route"]["escalated"].is_string())
        .collect();
    assert_eq!(up.len(), 1, "{up:?}");
    assert_eq!(up[0]["route"]["tier"], "mock/mock");
    assert_eq!(up[0]["route"]["escalated"], "check_failed");
    assert_eq!(up[0]["eval"]["task"], "slugify");
    assert_eq!(up[0]["eval"]["variant"], "routed");
}

#[test]
fn the_routing_variant_takes_its_pair_from_the_tiers_and_says_what_it_needs() {
    let tiered = routing_home("http://127.0.0.1:9/v1", "http://127.0.0.1:9/v1", true);
    let suite = suite();
    let run = |extra: &[&str]| {
        let mut args = vec!["eval", "run", suite.to_str().unwrap(), "--tag", "smoke"];
        args.extend_from_slice(extra);
        let out = ferrule(tiered.path(), &args);
        let (stdout, stderr) = texts(&out);
        (out.status.success(), format!("{stdout}{stderr}"))
    };
    let (ok, text) = run(&["--variant", "routing", "--dry-run"]);
    assert!(ok, "{text}");
    for want in [
        "dry run — suite starter",
        "weak/mock → mock/mock via routing",
        "12 task run(s): 4 task(s) × cheap + routed + strong × 1 repeat(s)",
        "worst-case cost: $",
    ] {
        assert!(text.contains(want), "{want:?} not in:\n{text}");
    }
    let (ok, text) = run(&["--variant", "routing", "--cheap", "mock", "--dry-run"]);
    assert!(!ok && text.contains("both mock/mock"), "{text}");
    let (ok, text) = run(&["--variant", "routing", "--strong", "nope/x", "--dry-run"]);
    assert!(!ok && text.contains("--strong:"), "{text}");
    let (ok, text) = run(&["--variant", "routing", "--model", "mock", "--dry-run"]);
    assert!(!ok && text.contains("not --provider or --model"), "{text}");
    let (ok, text) = run(&["--variant", "ab", "--cheap", "weak/mock", "--dry-run"]);
    assert!(!ok && text.contains("go with --variant routing"), "{text}");
    let bare = home("http://127.0.0.1:9/v1");
    let out = ferrule(
        bare.path(),
        &[
            "eval",
            "run",
            suite.to_str().unwrap(),
            "--variant",
            "routing",
            "--dry-run",
        ],
    );
    let (stdout, stderr) = texts(&out);
    assert!(!out.status.success());
    assert!(stderr.contains("needs --cheap"), "{stdout}{stderr}");
}

/// The real comparison on the smoke subset, against the models in your own
/// config's `[routing] tiers` (see `docs/routing.md`, "Measuring it"):
///
/// ```text
/// FERRULE_LIVE_CONFIG=~/.config/ferrule/ferrule.toml \
///   cargo test -p ferrule-cli --test eval -- --ignored live_routing --nocapture
/// ```
///
/// Capped at $1; the report is printed.
#[test]
#[ignore = "live: needs FERRULE_LIVE_CONFIG with [routing] tiers and their keys, costs cents"]
fn live_routing_smoke_compares_cheap_routed_and_strong() {
    let Some(config) = std::env::var_os("FERRULE_LIVE_CONFIG") else {
        eprintln!("FERRULE_LIVE_CONFIG isn't set: skipped");
        return;
    };
    if !have_python() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ferrule"))
        .args(["eval", "run", suite().to_str().unwrap()])
        .args(["--tag", "smoke", "--variant", "routing", "--max-usd", "1"])
        .env("FERRULE_CONFIG", config)
        .env("FERRULE_DATA_DIR", data.path())
        .output()
        .unwrap();
    let (stdout, stderr) = texts(&out);
    println!("{stdout}");
    assert_ne!(
        out.status.code(),
        Some(3),
        "the $1 cap stopped it:\n{stderr}"
    );
    assert!(stdout.contains("routing: cheap "), "{stdout}\n{stderr}");
    assert!(stdout
        .lines()
        .any(|l| l.trim_start().starts_with("escalations")));
}

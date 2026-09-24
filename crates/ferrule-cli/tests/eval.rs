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
        let mut child = Command::new("python3")
            .arg(suite().join("mock/model.py"))
            .args(["--port", "0"])
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

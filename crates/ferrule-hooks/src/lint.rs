//! M29's per-edit lint: a built-in PostToolUse hook on `edit_file` and
//! `write_file` that runs the project's own linter on the file just
//! written and hands its complaints back with the tool result
//! (docs/m29-edit-mechanics.md §3).
//!
//! A linter runs only when it's installed **and** the project has adopted
//! it (its config file is there): one the project doesn't use reports
//! style nobody follows. It runs in the sandbox, with a timeout. A missing
//! linter is silent; `ferrule doctor` says which are found.

use ferrule_core::lifecycle::{
    Hook, HookEvent, HookHandler, HookInput, HookRun, HookSource, Matcher,
};
use ferrule_core::tool::ToolContext;
use ferrule_sandbox::Sandbox;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

/// Lines of linter output kept.
pub const MAX_LINES: usize = 40;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// The linters, for `ferrule doctor`: (program, files it's for).
pub const LINTERS: &[(&str, &str)] = &[
    ("rustfmt", ".rs"),
    ("ruff", ".py"),
    ("gofmt", ".go"),
    ("eslint", ".js/.ts"),
    ("tsc", ".ts"),
];

const ESLINT_CONFIGS: &[&str] = &[
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc.yml",
    ".eslintrc.yaml",
];

pub struct LintHook {
    sandbox: Arc<Sandbox>,
    timeout: Duration,
    /// Where to look for the linters; `None` is `PATH`.
    path: Option<Vec<PathBuf>>,
}

impl LintHook {
    pub fn new(sandbox: Arc<Sandbox>, timeout: Duration) -> LintHook {
        LintHook {
            sandbox,
            timeout,
            path: None,
        }
    }

    /// Look for linters in `dirs` instead of `PATH` (tests).
    pub fn searching(mut self, dirs: Vec<PathBuf>) -> LintHook {
        self.path = Some(dirs);
        self
    }

    /// As the built-in PostToolUse hook on the two file-writing tools.
    pub fn into_hook(self) -> Hook {
        Hook::new(
            HookEvent::PostToolUse,
            Matcher::parse(Some("edit_file|write_file")),
            HookSource::Builtin,
            Arc::new(self),
        )
    }

    fn dirs(&self) -> Vec<PathBuf> {
        match &self.path {
            Some(dirs) => dirs.clone(),
            None => std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
        }
    }

    /// `name` in the workspace's `node_modules/.bin` (for the JS tools),
    /// then on the search path.
    fn find(&self, name: &str, workspace: &Path, node: bool) -> Option<PathBuf> {
        let local = node.then(|| workspace.join("node_modules").join(".bin"));
        local
            .into_iter()
            .chain(self.dirs())
            .find_map(|d| executable_in(&d, name))
    }
}

/// `dir/name`, or on Windows `name.exe`/`.cmd`/`.bat`.
pub fn executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    exts.iter()
        .map(|e| dir.join(format!("{name}{e}")))
        .find(|p| p.is_file())
}

/// Whether `name` is on `PATH`, for `ferrule doctor`.
pub fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| executable_in(&d, name).is_some()))
}

/// One linter run on one file.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    label: &'static str,
    program: &'static str,
    node: bool,
    args: Vec<String>,
    /// Keep only output lines about this file (tsc checks the project).
    only: Option<String>,
}

/// The nearest dir, from `file`'s up to `workspace`, where `found` holds.
fn find_up(file: &Path, workspace: &Path, found: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if found(d) {
            return Some(d.to_path_buf());
        }
        if d == workspace {
            break;
        }
        dir = d.parent();
    }
    None
}

fn has_any(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|n| dir.join(n).is_file())
}

/// The Rust edition from the nearest `Cargo.toml` (rustfmt needs it to
/// parse 2018+ code on its own); 2021 when it doesn't say.
fn edition(manifest_dir: &Path) -> String {
    let text = std::fs::read_to_string(manifest_dir.join("Cargo.toml")).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("edition"))
        .filter_map(|r| r.trim_start().strip_prefix('='))
        .map(|v| v.trim().trim_matches('"').to_string())
        .find(|v| v.len() == 4 && v.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or_else(|| "2021".into())
}

/// What runs for `file` (absolute, inside `workspace`; `rel` is the
/// workspace-relative, `/`-separated path).
fn plans(file: &Path, rel: &str, workspace: &Path) -> Vec<Plan> {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let arg = rel.to_string();
    let mut out = Vec::new();
    match ext.as_str() {
        "rs" => {
            if let Some(dir) = find_up(file, workspace, |d| d.join("Cargo.toml").is_file()) {
                out.push(Plan {
                    label: "rustfmt --check",
                    program: "rustfmt",
                    node: false,
                    args: vec!["--check".into(), "--edition".into(), edition(&dir), arg],
                    only: None,
                });
            }
        }
        "py" | "pyi" => {
            let adopted = find_up(file, workspace, |d| {
                has_any(d, &["ruff.toml", ".ruff.toml"])
                    || std::fs::read_to_string(d.join("pyproject.toml"))
                        .is_ok_and(|t| t.contains("[tool.ruff"))
            });
            if adopted.is_some() {
                out.push(Plan {
                    label: "ruff check",
                    program: "ruff",
                    node: false,
                    args: vec!["check".into(), "--quiet".into(), "--no-cache".into(), arg],
                    only: None,
                });
            }
        }
        "go" => {
            if find_up(file, workspace, |d| d.join("go.mod").is_file()).is_some() {
                out.push(Plan {
                    label: "gofmt -l",
                    program: "gofmt",
                    node: false,
                    args: vec!["-l".into(), "-e".into(), arg],
                    only: None,
                });
            }
        }
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => {
            let eslint = find_up(file, workspace, |d| {
                has_any(d, ESLINT_CONFIGS)
                    || std::fs::read_to_string(d.join("package.json"))
                        .is_ok_and(|t| t.contains("\"eslintConfig\""))
            });
            if eslint.is_some() {
                out.push(Plan {
                    label: "eslint",
                    program: "eslint",
                    node: true,
                    args: vec!["--no-color".into(), arg.clone()],
                    only: None,
                });
            }
            let typed = matches!(ext.as_str(), "ts" | "tsx" | "mts" | "cts");
            if let Some(dir) = typed
                .then(|| find_up(file, workspace, |d| d.join("tsconfig.json").is_file()))
                .flatten()
            {
                let project = dir.strip_prefix(workspace).map_or_else(
                    |_| ".".to_string(),
                    |p| {
                        let p = p.to_string_lossy().replace('\\', "/");
                        if p.is_empty() {
                            ".".into()
                        } else {
                            p
                        }
                    },
                );
                out.push(Plan {
                    label: "tsc --noEmit",
                    program: "tsc",
                    node: true,
                    args: vec![
                        "--noEmit".into(),
                        "--pretty".into(),
                        "false".into(),
                        "-p".into(),
                        project,
                    ],
                    only: Some(arg),
                });
            }
        }
        _ => {}
    }
    out
}

/// The edited file, absolute, if it's a real file inside the workspace.
fn edited_file(input: &HookInput, workspace: &Path) -> Option<(PathBuf, String)> {
    if input.tool_response.as_ref()?["ok"].as_bool() != Some(true) {
        return None;
    }
    let path = input.tool_input.as_ref()?["path"].as_str()?;
    let root = dunce::canonicalize(workspace).ok()?;
    let file = dunce::canonicalize(root.join(path)).ok()?;
    let rel = file.strip_prefix(&root).ok()?;
    if !file.is_file() {
        return None;
    }
    let rel = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Some((file, rel))
}

enum Outcome {
    Clean,
    Problems(String),
    TimedOut,
    Missing,
}

impl LintHook {
    async fn run_one(&self, plan: &Plan, workspace: &Path) -> Outcome {
        let Some(program) = self.find(plan.program, workspace, plan.node) else {
            return Outcome::Missing;
        };
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut std_cmd = match self.sandbox.command(&program, &plan.args, workspace) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("lint: sandbox setup for {} failed: {e}", plan.program);
                return Outcome::Missing;
            }
        };
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut std_cmd, 0);
        let mut cmd = tokio::process::Command::from(std_cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let Ok(child) = cmd.spawn() else {
            return Outcome::Missing;
        };
        let pid = child.id();
        let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(_)) => return Outcome::Missing,
            Err(_) => {
                if let Some(pid) = pid {
                    crate::command::kill_tree(pid);
                }
                return Outcome::TimedOut;
            }
        };
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.trim().is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&err);
        }
        let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        if let Some(only) = &plan.only {
            let windows = only.replace('/', "\\");
            lines.retain(|l| l.starts_with(only.as_str()) || l.starts_with(&windows));
            if lines.is_empty() {
                return Outcome::Clean;
            }
        } else if lines.is_empty() && out.status.success() {
            return Outcome::Clean;
        }
        if lines.is_empty() {
            let code = out
                .status
                .code()
                .map_or("a signal".into(), |c| c.to_string());
            return Outcome::Problems(format!("(exited with {code}, no output)"));
        }
        let more = lines.len().saturating_sub(MAX_LINES);
        let mut shown = lines[..lines.len().min(MAX_LINES)].join("\n");
        if more > 0 {
            shown.push_str(&format!("\n… {more} more lines"));
        }
        Outcome::Problems(shown)
    }
}

#[async_trait::async_trait]
impl HookHandler for LintHook {
    fn command(&self) -> String {
        "ferrule lint (rustfmt/ruff/gofmt/eslint/tsc, the project's own)".into()
    }

    async fn run(&self, input: &HookInput, ctx: &ToolContext) -> HookRun {
        let mut notes = Vec::new();
        if let Some((file, rel)) = edited_file(input, &ctx.workspace) {
            let root = dunce::canonicalize(&ctx.workspace).unwrap_or(ctx.workspace.clone());
            for plan in plans(&file, &rel, &root) {
                match self.run_one(&plan, &root).await {
                    Outcome::Clean | Outcome::Missing => {}
                    Outcome::Problems(text) => notes.push(format!(
                        "lint ({}) on `{rel}` — problems in the file after your edit (some may predate it):\n{text}",
                        plan.label
                    )),
                    Outcome::TimedOut => notes.push(format!(
                        "lint ({}) on `{rel}` timed out after {:?}; nothing was checked",
                        plan.label, self.timeout
                    )),
                }
            }
        }
        let stdout = if notes.is_empty() {
            String::new()
        } else {
            serde_json::json!({ "additionalContext": notes.join("\n\n") }).to_string()
        };
        HookRun {
            exit_code: Some(0),
            stdout,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_for(files: &[(&str, &str)], edited: &str) -> Vec<Plan> {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let root = dunce::canonicalize(dir.path()).unwrap();
        plans(&root.join(edited), edited, &root)
    }

    fn programs(plans: &[Plan]) -> Vec<&str> {
        plans.iter().map(|p| p.program).collect::<Vec<_>>()
    }

    /// `lint = "auto"`: only what the project has adopted.
    #[test]
    fn a_linter_runs_only_where_the_project_adopted_it() {
        assert!(plan_for(&[("a.rs", "")], "a.rs").is_empty());
        let p = plan_for(
            &[
                ("Cargo.toml", "[package]\nedition = \"2018\"\n"),
                ("src/a.rs", ""),
            ],
            "src/a.rs",
        );
        assert_eq!(p[0].args, ["--check", "--edition", "2018", "src/a.rs"]);

        assert!(plan_for(&[("pyproject.toml", "[project]\n"), ("a.py", "")], "a.py").is_empty());
        let ruff = plan_for(
            &[
                ("pyproject.toml", "[tool.ruff]\nline-length = 100\n"),
                ("a.py", ""),
            ],
            "a.py",
        );
        assert_eq!(programs(&ruff), ["ruff"]);
        assert_eq!(
            programs(&plan_for(&[("ruff.toml", ""), ("a.py", "")], "a.py")),
            ["ruff"]
        );

        assert!(plan_for(&[("a.go", "")], "a.go").is_empty());
        assert_eq!(
            programs(&plan_for(&[("go.mod", ""), ("a.go", "")], "a.go")),
            ["gofmt"]
        );

        assert!(plan_for(&[("package.json", "{}"), ("a.ts", "")], "a.ts").is_empty());
        let both = plan_for(
            &[
                ("eslint.config.js", ""),
                ("web/tsconfig.json", "{}"),
                ("web/src/a.ts", ""),
            ],
            "web/src/a.ts",
        );
        assert_eq!(programs(&both), ["eslint", "tsc"]);
        assert_eq!(both[1].args.last().unwrap(), "web");
        assert_eq!(both[1].only.as_deref(), Some("web/src/a.ts"));
        // JavaScript gets no tsc.
        let js = plan_for(&[("tsconfig.json", "{}"), ("a.js", "")], "a.js");
        assert!(js.is_empty());
        assert!(plan_for(&[("README.md", "")], "README.md").is_empty());
    }

    #[cfg(unix)]
    mod running {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        struct Setup {
            ws: tempfile::TempDir,
            bin: tempfile::TempDir,
        }

        fn setup(files: &[(&str, &str)]) -> Setup {
            let ws = tempfile::tempdir().unwrap();
            for (name, text) in files {
                let p = ws.path().join(name);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
            Setup {
                ws,
                bin: tempfile::tempdir().unwrap(),
            }
        }

        fn fake(dir: &Path, name: &str, script: &str) {
            let p = dir.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn input(tool: &str, path: &str, ok: bool) -> HookInput {
            HookInput {
                hook_event_name: "PostToolUse".into(),
                tool_name: Some(tool.into()),
                tool_input: Some(serde_json::json!({ "path": path })),
                tool_response: Some(serde_json::json!({ "ok": ok, "content": "" })),
                ..Default::default()
            }
        }

        async fn note(s: &Setup, timeout: Duration, input: HookInput) -> Option<String> {
            let hook = LintHook::new(Arc::new(Sandbox::off()), timeout)
                .searching(vec![s.bin.path().to_path_buf()]);
            let ctx = ToolContext {
                workspace: s.ws.path().to_path_buf(),
                ..Default::default()
            };
            let run = hook.run(&input, &ctx).await;
            assert_eq!(run.exit_code, Some(0));
            if run.stdout.is_empty() {
                return None;
            }
            let v: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
            Some(v["additionalContext"].as_str().unwrap().to_string())
        }

        const RUST: &[(&str, &str)] = &[
            ("Cargo.toml", "[package]\n"),
            ("src/a.rs", "fn  main(){}\n"),
        ];

        #[tokio::test]
        async fn a_linters_complaints_come_back_with_the_tool_result() {
            let s = setup(RUST);
            // Echoes its args, like a real formatter's diff would name the file.
            fake(
                s.bin.path(),
                "rustfmt",
                "echo \"Diff in $4 at line 1\"; pwd; exit 1",
            );
            let got = note(&s, DEFAULT_TIMEOUT, input("edit_file", "src/a.rs", true))
                .await
                .unwrap();
            assert!(
                got.starts_with(
                    "lint (rustfmt --check) on `src/a.rs` — problems in the file after your edit"
                ),
                "{got}"
            );
            assert!(got.contains("Diff in src/a.rs at line 1"), "{got}");
            // It ran in the workspace.
            let ws = dunce::canonicalize(s.ws.path()).unwrap();
            assert!(got.contains(&ws.display().to_string()), "{got}");
        }

        #[tokio::test]
        async fn a_clean_file_a_failed_call_and_other_tools_add_nothing() {
            let s = setup(RUST);
            fake(s.bin.path(), "rustfmt", "exit 0");
            assert_eq!(
                note(&s, DEFAULT_TIMEOUT, input("edit_file", "src/a.rs", true)).await,
                None
            );
            fake(s.bin.path(), "rustfmt", "echo bad; exit 1");
            assert_eq!(
                note(&s, DEFAULT_TIMEOUT, input("write_file", "src/a.rs", false)).await,
                None
            );
            // Outside the workspace, or not there at all.
            assert_eq!(
                note(&s, DEFAULT_TIMEOUT, input("edit_file", "../x.rs", true)).await,
                None
            );
            assert_eq!(
                note(&s, DEFAULT_TIMEOUT, input("edit_file", "src/gone.rs", true)).await,
                None
            );
        }

        #[tokio::test]
        async fn a_missing_linter_is_silent() {
            let s = setup(RUST);
            assert_eq!(
                note(&s, DEFAULT_TIMEOUT, input("edit_file", "src/a.rs", true)).await,
                None
            );
        }

        #[tokio::test]
        async fn a_hung_linter_is_killed_at_the_timeout() {
            let s = setup(&[("go.mod", "module x\n"), ("a.go", "package x\n")]);
            fake(
                s.bin.path(),
                "gofmt",
                "(sleep 3; touch survived) & sleep 30",
            );
            let start = std::time::Instant::now();
            let got = note(
                &s,
                Duration::from_millis(300),
                input("edit_file", "a.go", true),
            )
            .await
            .unwrap();
            assert!(start.elapsed() < Duration::from_secs(3));
            assert_eq!(
                got,
                "lint (gofmt -l) on `a.go` timed out after 300ms; nothing was checked"
            );
            // The whole group went, the background child too.
            let mut left = 0;
            while left < 40 && !s.ws.path().join("survived").exists() {
                tokio::time::sleep(Duration::from_millis(100)).await;
                left += 1;
            }
            assert!(!s.ws.path().join("survived").exists());
        }

        #[tokio::test]
        async fn tsc_output_is_kept_to_the_edited_file_and_node_modules_wins() {
            let s = setup(&[
                ("tsconfig.json", "{}"),
                ("src/a.ts", "let x: number = 'a';\n"),
                ("src/b.ts", ""),
            ]);
            let local = s.ws.path().join("node_modules/.bin");
            std::fs::create_dir_all(&local).unwrap();
            fake(
                &local,
                "tsc",
                "echo \"src/a.ts(1,5): error TS2322: nope\"; echo \"src/b.ts(3,1): error TS1: other\"; exit 2",
            );
            fake(s.bin.path(), "tsc", "echo wrong tsc; exit 2");
            let got = note(&s, DEFAULT_TIMEOUT, input("edit_file", "src/a.ts", true))
                .await
                .unwrap();
            assert!(
                got.ends_with("\nsrc/a.ts(1,5): error TS2322: nope"),
                "{got}"
            );
            // Only the other file's errors: nothing to say about this one.
            assert_eq!(
                note(&s, DEFAULT_TIMEOUT, input("edit_file", "src/b.ts", false)).await,
                None
            );
        }

        #[tokio::test]
        async fn long_output_is_cut() {
            let s = setup(&[("ruff.toml", ""), ("a.py", "")]);
            fake(
                s.bin.path(),
                "ruff",
                "i=0; while [ $i -lt 100 ]; do echo \"a.py:$i: E1\"; i=$((i+1)); done; exit 1",
            );
            let got = note(&s, DEFAULT_TIMEOUT, input("edit_file", "a.py", true))
                .await
                .unwrap();
            assert_eq!(got.lines().count(), 1 + MAX_LINES + 1, "{got}");
            assert!(got.ends_with("… 60 more lines"));
        }
    }
}

//! The starter suite (`evals/starter`) is sound: it loads, its smoke subset
//! is 3-5 tasks, every task's reference solution passes its grader, and an
//! untouched workspace (and, for the verify tasks, the plausible first try)
//! fails it. Needs `python3`; without it the test says so and passes.

use ferrule_eval::fixture::{run_command, Fixture};
use ferrule_eval::Suite;
use ferrule_sandbox::Sandbox;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn starter() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/starter")
}

#[test]
fn the_starter_suite_loads_with_a_smoke_subset_and_the_advertised_tags() {
    let suite = Suite::load(&starter()).unwrap();
    assert_eq!(suite.name, "starter");
    assert!(suite.tasks.len() >= 18, "{}", suite.tasks.len());
    let smoke = suite.select(&["smoke".into()], &[]).unwrap();
    assert!((3..=5).contains(&smoke.len()), "{}", smoke.len());
    for tag in ["context", "verify", "code", "data"] {
        let n = suite.select(&[tag.into()], &[]).unwrap().len();
        assert!(n >= 3, "only {n} {tag} task(s)");
        assert!(
            smoke.iter().any(|t| t.tags.iter().any(|x| x == tag)),
            "no {tag} task in smoke"
        );
    }
    for t in &suite.tasks {
        assert!(t.grade.command.is_some(), "{} has no command grader", t.id);
        let sol = starter().join("solutions").join(&t.id);
        assert!(sol.join("solve.sh").is_file(), "{} has no solve.sh", t.id);
        if t.tags.iter().any(|x| x == "verify") {
            assert!(t.check.is_some(), "verify task {} has no check", t.id);
        }
    }
}

#[tokio::test]
async fn every_reference_solution_passes_and_an_untouched_workspace_fails() {
    let python = Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !python {
        eprintln!("python3 not found: skipping (the starter suite's graders need it)");
        return;
    }
    let suite = Suite::load(&starter()).unwrap();
    let sandbox = Arc::new(Sandbox::off());
    let tmp = tempfile::tempdir().unwrap();
    let mut wrong = Vec::new();
    for t in &suite.tasks {
        let grade = t.grade.command.as_deref().unwrap();
        let sol = starter().join("solutions").join(&t.id);

        let fx = Fixture::prepare(
            tmp.path(),
            &format!("{}--untouched", t.id),
            t,
            &sandbox,
            false,
        )
        .await
        .unwrap();
        if run_command(grade, &fx.workspace, &sandbox, 120)
            .await
            .is_ok()
        {
            wrong.push(format!("{}: an untouched workspace passes", t.id));
        }
        drop(fx);

        let first_try = sol.join("first_try.sh");
        if first_try.is_file() {
            let fx = Fixture::prepare(tmp.path(), &format!("{}--first", t.id), t, &sandbox, false)
                .await
                .unwrap();
            let sh = format!("sh \"{}\"", first_try.display());
            run_command(&sh, &fx.workspace, &sandbox, 120)
                .await
                .unwrap_or_else(|e| panic!("{}: first_try.sh: {e}", t.id));
            if run_command(grade, &fx.workspace, &sandbox, 120)
                .await
                .is_ok()
            {
                wrong.push(format!("{}: the wrong first try passes", t.id));
            }
            if let Some(check) = &t.check {
                if run_command(check, &fx.workspace, &sandbox, 120)
                    .await
                    .is_ok()
                {
                    wrong.push(format!("{}: the check misses the first try", t.id));
                }
            }
        }

        let fx = Fixture::prepare(tmp.path(), &format!("{}--solved", t.id), t, &sandbox, false)
            .await
            .unwrap();
        let sh = format!("sh \"{}\"", sol.join("solve.sh").display());
        run_command(&sh, &fx.workspace, &sandbox, 120)
            .await
            .unwrap_or_else(|e| panic!("{}: solve.sh: {e}", t.id));
        if let Err(e) = run_command(grade, &fx.workspace, &sandbox, 120).await {
            wrong.push(format!("{}: the reference solution fails: {e}", t.id));
        }
        if let Some(check) = &t.check {
            if let Err(e) = run_command(check, &fx.workspace, &sandbox, 120).await {
                wrong.push(format!("{}: the check rejects the solution: {e}", t.id));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

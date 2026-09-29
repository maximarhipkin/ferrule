//! M29 `--edit-tools`: which file-writing tools each variant's model is
//! offered, and what their schemas cost per call. No network.

use ferrule_core::{
    CompletionRequest, CompletionResponse, CoreError, HarnessProfile, Message, Provider, Usage,
};
use ferrule_eval::variant::{build, Build};
use ferrule_eval::{EditTools, Variant};
use ferrule_sandbox::Sandbox;
use std::sync::{Arc, Mutex};

/// Answers "done" and keeps the tool names of every request.
#[derive(Default)]
struct Recording(Mutex<Vec<Vec<String>>>);

#[async_trait::async_trait]
impl Provider for Recording {
    fn name(&self) -> &str {
        "recording"
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, CoreError> {
        let names = req.tools.iter().map(|t| t.name.clone()).collect();
        self.0.lock().unwrap().push(names);
        Ok(CompletionResponse {
            message: Message::assistant(Some("done".into()), vec![], None),
            usage: Usage::default(),
        })
    }
}

async fn offered(variant: Variant, edit_tools: EditTools) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let (workspace, state) = (dir.path().join("w"), dir.path().join("s"));
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let provider = Arc::new(Recording::default());
    let sandbox = Arc::new(Sandbox::off());
    let profile = HarnessProfile::generic();
    let mut agent = build(Build {
        variant,
        provider: provider.clone(),
        profile: &profile,
        sandbox: &sandbox,
        memory_tools: None,
        workspace: &workspace,
        state: &state,
        max_iterations: 3,
        check: None,
        transcript: None,
        playbook: None,
        edit_tools,
    });
    let (tx, _rx) = tokio::sync::mpsc::channel(256);
    agent.run("go", tx).await.unwrap();
    let seen = provider.0.lock().unwrap();
    seen[0].clone()
}

#[tokio::test]
async fn write_only_takes_edit_file_away_from_both_variants() {
    for variant in [Variant::Engineered, Variant::Naive] {
        let both = offered(variant, EditTools::Both).await;
        assert!(
            both.iter().any(|t| t == "edit_file"),
            "{variant:?}: {both:?}"
        );
        assert!(both.iter().any(|t| t == "write_file"));

        let write_only = offered(variant, EditTools::WriteOnly).await;
        assert!(!write_only.iter().any(|t| t == "edit_file"), "{variant:?}");
        assert!(write_only.iter().any(|t| t == "write_file"));
        // Nothing else differs.
        let mut rest = both.clone();
        rest.retain(|t| t != "edit_file");
        assert_eq!(rest, write_only, "{variant:?}");
    }
}

#[test]
fn edit_tools_parse() {
    assert_eq!(EditTools::parse("both"), Some(EditTools::Both));
    assert_eq!(EditTools::parse("write-only"), Some(EditTools::WriteOnly));
    assert_eq!(EditTools::parse("edit-only"), None);
    assert_eq!(EditTools::default(), EditTools::Both);
}

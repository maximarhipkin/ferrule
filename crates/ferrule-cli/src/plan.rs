//! M19 plan mode in the binary: `ferrule run --plan`, `ferrule plan …`
//! and `/plan` in a chat (docs/m19-trust-cost.md §8). The exploration
//! runs with the tree marked planning, so every agent built for it is
//! read-only; the approved plan runs in the same session, with the normal
//! tools and gates.

use crate::{config, trust, RootRun};
use anyhow::{anyhow, Result};
use ferrule_gateway::{Channel, InboundMessage, OutboundMessage, Router};
use ferrule_trust::plan::execution_prompt;
use ferrule_trust::{ChatRef, Hub, Plan, PlanStatus, PlanStore, Route};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(clap::Subcommand)]
pub enum PlanCmd {
    /// Every plan, newest first
    List,
    /// Approve a proposed plan and run it in its session and workspace.
    /// Gated commands in it ask the terminal, or are refused without one
    Approve {
        id: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value_t = 60)]
        max_iterations: usize,
        /// Show model reasoning in the event stream
        #[arg(long)]
        show_reasoning: bool,
    },
    /// Reject a proposed plan
    Reject { id: String },
}

fn store() -> Result<PlanStore> {
    Ok(PlanStore::new(config::data_dir()?.join("plans")))
}

fn record(hub: &Hub, event: &str, plan: &Plan, run: Option<&str>, extra: Value) {
    let mut detail = json!({"plan": plan.id, "sha256": plan.sha256});
    if let (Value::Object(d), Value::Object(e)) = (&mut detail, extra) {
        d.extend(e);
    }
    hub.audit()
        .record(chrono::Utc::now(), event, Some(&plan.session), run, detail);
}

fn status_name(s: PlanStatus) -> &'static str {
    match s {
        PlanStatus::Proposed => "proposed",
        PlanStatus::Approved => "approved",
        PlanStatus::Rejected => "rejected",
        PlanStatus::Executed => "executed",
    }
}

/// `ferrule run --plan`: explore read-only, save the plan, and run it
/// once approved at the terminal; without one, say how to approve it.
pub async fn run(
    prompt: &str,
    provider: Option<String>,
    workspace: PathBuf,
    max_iterations: usize,
    show_reasoning: bool,
) -> Result<()> {
    let (cfg, _) = config::Config::load()?;
    let hub = trust::hub(&cfg)?;
    let workspace = dunce::canonicalize(&workspace).unwrap_or(workspace);
    let session = uuid::Uuid::new_v4().to_string();
    trust::set_planning(&session, true);
    let explored = crate::run_root(
        prompt,
        provider.clone(),
        workspace.clone(),
        max_iterations,
        show_reasoning,
        &session,
        false,
    )
    .await;
    trust::set_planning(&session, false);
    let text = match explored? {
        Ok(RootRun {
            text,
            incomplete: None,
            ..
        }) => text,
        other => {
            println!("\n\x1b[1;33mno plan: the exploration didn't finish\x1b[0m");
            return crate::finish_run(other);
        }
    };
    let store = store()?;
    let plan = store.propose(prompt, &workspace, &session, &text)?;
    record(
        &hub,
        "plan_proposed",
        &plan,
        hub.run_id(&session).as_deref(),
        json!({"route": "cli"}),
    );
    println!(
        "\n\x1b[1;36mplan {}:\x1b[0m\n{}\n",
        plan.id,
        plan.text.trim()
    );
    let Route::Terminal(prompter) = trust::terminal_route() else {
        println!(
            "plan {id} saved. `ferrule plan approve {id}` runs it; `ferrule plan reject {id}` drops it.",
            id = plan.id
        );
        return Ok(());
    };
    let asked = hub
        .ask_terminal(
            &session,
            &format!("plan {}", plan.id),
            &format!("Run plan {}?", plan.id),
            prompter,
        )
        .await;
    match asked {
        Ok(()) => {
            execute(
                &hub,
                &store,
                &plan.id,
                provider,
                max_iterations,
                show_reasoning,
                "terminal",
            )
            .await
        }
        Err(why) => {
            reject(&hub, &store, &plan.id, &why)?;
            println!("plan {} rejected; nothing was run", plan.id);
            Ok(())
        }
    }
}

/// Approves `id` and runs it in its own session, replaying what the
/// exploration read.
async fn execute(
    hub: &Hub,
    store: &PlanStore,
    id: &str,
    provider: Option<String>,
    max_iterations: usize,
    show_reasoning: bool,
    by: &str,
) -> Result<()> {
    let plan = store
        .decide(id, PlanStatus::Approved)
        .map_err(|e| anyhow!(e))?;
    record(hub, "plan_approved", &plan, None, json!({"by": by}));
    let answer = crate::run_root(
        &execution_prompt(&plan),
        provider,
        PathBuf::from(&plan.workspace),
        max_iterations,
        show_reasoning,
        &plan.session,
        true,
    )
    .await?;
    let outcome = match &answer {
        Ok(r) => r.incomplete.clone().unwrap_or_else(|| "finished".into()),
        Err(e) => format!("failed: {e}"),
    };
    executed(hub, store, plan, &outcome)?;
    crate::finish_run(answer)
}

/// Marks an approved plan carried out by its session's latest run.
fn executed(hub: &Hub, store: &PlanStore, mut plan: Plan, outcome: &str) -> Result<()> {
    let run = hub.run_id(&plan.session);
    plan.status = PlanStatus::Executed;
    plan.executed_by = run.clone();
    store.save(&plan)?;
    record(
        hub,
        "plan_executed",
        &plan,
        run.as_deref(),
        json!({"outcome": outcome}),
    );
    Ok(())
}

fn reject(hub: &Hub, store: &PlanStore, id: &str, why: &str) -> Result<()> {
    let plan = store
        .decide(id, PlanStatus::Rejected)
        .map_err(|e| anyhow!(e))?;
    record(hub, "plan_rejected", &plan, None, json!({"why": why}));
    Ok(())
}

pub async fn cmd(op: PlanCmd) -> Result<()> {
    let (cfg, _) = config::Config::load()?;
    let hub = trust::hub(&cfg)?;
    let store = store()?;
    match op {
        PlanCmd::List => {
            let plans = store.list();
            if plans.is_empty() {
                println!("no plans");
            }
            for p in plans {
                let first = p.task.lines().next().unwrap_or("");
                println!(
                    "{}  {:<9}  {}  {}  {}",
                    p.id,
                    status_name(p.status),
                    p.created,
                    p.workspace,
                    first
                );
            }
        }
        PlanCmd::Approve {
            id,
            provider,
            max_iterations,
            show_reasoning,
        } => {
            execute(
                &hub,
                &store,
                &id,
                provider,
                max_iterations,
                show_reasoning,
                "ferrule plan approve",
            )
            .await?
        }
        PlanCmd::Reject { id } => {
            reject(&hub, &store, &id, "ferrule plan reject")?;
            println!("plan {id} rejected");
        }
    }
    Ok(())
}

/// `/plan <task>` from a chat on any channel: returns the acknowledgement
/// and runs the rest in the background — explore read-only in a session of
/// its own (`plan__<id>`, on a channel nothing listens to), ask the owner
/// chat, and on `yes` run the plan there and send the answer to the chat
/// that asked, on its own channel.
pub fn chats(
    router: Arc<Router>,
    hub: Arc<Hub>,
    channels: HashMap<String, Arc<dyn Channel>>,
    workspace: PathBuf,
) -> Arc<dyn Fn(ChatRef, String) -> String + Send + Sync> {
    Arc::new(move |chat, task| {
        tokio::spawn(chat_plan(
            router.clone(),
            hub.clone(),
            channels.get(&chat.channel).cloned(),
            workspace.clone(),
            chat,
            task,
        ));
        "Planning (read-only; nothing changes yet). The plan goes to the owner for approval.".into()
    })
}

async fn chat_plan(
    router: Arc<Router>,
    hub: Arc<Hub>,
    channel: Option<Arc<dyn Channel>>,
    workspace: PathBuf,
    chat: ChatRef,
    task: String,
) {
    let say = |text: String| {
        let channel = channel.clone();
        let chat_id = chat.chat.clone();
        async move {
            if let Some(ch) = channel {
                let _ = ch
                    .send(OutboundMessage {
                        channel: ch.name().to_string(),
                        chat_id,
                        text,
                        reply_to: None,
                        attachments: vec![],
                    })
                    .await;
            }
        }
    };
    if let Err(e) = chat_plan_inner(&router, &hub, &workspace, &chat, &task, &say).await {
        say(format!("/plan failed: {e}")).await;
    }
}

async fn chat_plan_inner<F, Fut>(
    router: &Router,
    hub: &Hub,
    workspace: &Path,
    chat: &ChatRef,
    task: &str,
    say: &F,
) -> Result<()>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let pid = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let sid = ferrule_gateway::session::session_id(PLAN_CHANNEL, &pid);
    trust::seat(
        &sid,
        Route::Owner {
            chat_label: format!(
                "the plan asked for in {} chat {}",
                chat.channel_title(),
                chat.chat
            ),
        },
    );
    let turn = |text: String| InboundMessage {
        channel: PLAN_CHANNEL.into(),
        chat_id: pid.clone(),
        sender: chat.chat.clone(),
        sender_id: None,
        message_id: String::new(),
        text,
        attachments: vec![],
        reply_to: None,
        ts: chrono::Utc::now().timestamp(),
    };
    trust::set_planning(&sid, true);
    let explored = router.dispatch_and_wait(turn(task.to_string())).await;
    trust::set_planning(&sid, false);
    // The next turn gets a new agent, with the tools of a normal run.
    router.retire(&sid);
    let text = match explored {
        Ok(r) if r.incomplete.is_none() => r.text,
        Ok(r) => {
            say(format!(
                "No plan: the exploration stopped ({}). {}",
                r.incomplete.unwrap_or_default(),
                r.text
            ))
            .await;
            return Ok(());
        }
        Err(e) => return Err(anyhow!(e)),
    };
    let store = store()?;
    let plan = store.propose(task, workspace, &sid, &text)?;
    record(
        hub,
        "plan_proposed",
        &plan,
        hub.run_id(&sid).as_deref(),
        json!({"route": chat.channel, "chat": chat.audit_value()}),
    );
    let question = format!(
        "Plan {} (asked in chat {}):\n\n{}\n\nTask: {task}",
        plan.id,
        chat.chat,
        plan.text.trim()
    );
    let timeout = Duration::from_secs(hub.config().plan_timeout_secs);
    let asked = hub
        .ask_owner(&sid, &format!("plan {}", plan.id), &question, timeout)
        .await;
    if let Err(why) = asked {
        reject(hub, &store, &plan.id, &why)?;
        say(format!("Plan {} wasn't run: {why}.", plan.id)).await;
        return Ok(());
    }
    let plan = store
        .decide(&plan.id, PlanStatus::Approved)
        .map_err(|e| anyhow!(e))?;
    record(
        hub,
        "plan_approved",
        &plan,
        None,
        json!({"by": chat.channel}),
    );
    say(format!("Plan {} approved; running it.", plan.id)).await;
    let ran = router
        .dispatch_and_wait(turn(execution_prompt(&plan)))
        .await;
    router.retire(&sid);
    let (outcome, answer) = match ran {
        Ok(r) => match r.incomplete {
            Some(why) => (why.clone(), format!("(incomplete: {why}) {}", r.text)),
            None => ("finished".to_string(), r.text),
        },
        Err(e) => (
            format!("failed: {e}"),
            format!("The plan's run failed: {e}"),
        ),
    };
    executed(hub, &store, plan, &outcome)?;
    say(answer).await;
    Ok(())
}

/// The channel plan sessions run on: no adapter has this name, so their
/// answers are only returned, never sent.
const PLAN_CHANNEL: &str = "plan";

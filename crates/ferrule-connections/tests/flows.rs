//! The M20 flows end to end against mocks: a provider that is an OAuth
//! server (DCR, PKCE) and an MCP server wanting a bearer token, a relay
//! speaking the Worker's contract, and the owner's side recorded.

mod common;

use common::*;
use ferrule_connections::catalog::{AuthKind, ReadOnly, Service};
use ferrule_connections::{tools, Actor, Chat, Connections, ConnectionsConfig, State};
use ferrule_core::tool::{Tool, ToolContext, ToolSource};
use ferrule_extensions::{AllowList, ExtensionManager, Layout, ManagerConfig};
use ferrule_mcp::ServerHost;
use ferrule_sandbox::Sandbox;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const RELAY_KEY: &str = "relay-key-for-tests-0123456789";

struct World {
    dir: tempfile::TempDir,
    provider: Provider,
    relay: Option<MockRelay>,
    events: Arc<Recorder>,
    conns: Arc<Connections>,
    owner: Chat,
}

enum RelayMode {
    Up,
    Unreachable,
}

fn mock_service(provider: &Provider) -> Service {
    let mut s = Service::from_url(&provider.mcp_url()).unwrap();
    s.name = "mock".into();
    s.title = "Mock".into();
    s.scopes = vec!["read".into()];
    s.write_scopes = vec!["read".into(), "write".into()];
    s.read_only = ReadOnly::Scope;
    s
}

fn keyed_service(provider: &Provider) -> Service {
    let mut s = mock_service(provider);
    s.name = "keyed".into();
    s.title = "Keyed".into();
    s.auth = AuthKind::ApiKey;
    s.scopes = vec![];
    s.write_scopes = vec![];
    s.read_only = ReadOnly::None;
    s
}

async fn world_with(mode: RelayMode, cloudflared: Option<String>) -> World {
    let dir = tempfile::tempdir().unwrap();
    let provider = Provider::start().await;
    let relay = match mode {
        RelayMode::Up => Some(MockRelay::start(RELAY_KEY).await),
        RelayMode::Unreachable => None,
    };
    let cfg = ConnectionsConfig {
        relay_url: Some(
            relay
                .as_ref()
                .map(|r| r.url.clone())
                .unwrap_or_else(|| "http://127.0.0.1:1".into()),
        ),
        cloudflared: Some(cloudflared.unwrap_or_else(|| "off".into())),
        custom: vec![mock_service(&provider), keyed_service(&provider)],
        ..Default::default()
    };
    let events = Arc::new(Recorder::default());
    let host = ServerHost {
        sandbox: Arc::new(Sandbox::off()),
        workspace: dir.path().to_path_buf(),
        state_dir: dir.path().join("state"),
    };
    let conns = Connections::new(
        &dir.path().join("private"),
        cfg,
        Arc::new(|name: &str| (name == "FERRULE_RELAY_KEY").then(|| RELAY_KEY.to_string())),
        events.clone(),
        Some(host),
    )
    .unwrap()
    .with_timing(Duration::from_secs(20), Duration::from_millis(20));
    World {
        dir,
        provider,
        relay,
        events,
        conns,
        owner: Chat {
            channel: "telegram".into(),
            id: "42".into(),
        },
    }
}

async fn world(mode: RelayMode) -> World {
    world_with(mode, None).await
}

/// M17's manager, following the connections the way the gateway does.
fn manager(w: &World) -> Arc<ExtensionManager> {
    let mgr = ExtensionManager::new(ManagerConfig {
        layout: Layout::new(w.dir.path().join("data")),
        allow: AllowList::default(),
        sandbox: Arc::new(Sandbox::off()),
        workspace: w.dir.path().to_path_buf(),
        skills: None,
    });
    let (conns, m) = (w.conns.clone(), mgr.clone());
    let mut changes = conns.subscribe();
    tokio::spawn(async move {
        loop {
            m.set_configured(conns.servers()).await;
            if changes.changed().await.is_err() {
                break;
            }
        }
    });
    mgr
}

fn names(mgr: &ExtensionManager) -> Vec<String> {
    let mut n: Vec<String> = mgr.tools().iter().map(|t| t.definition().name).collect();
    n.sort();
    n
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn call(tool: &Arc<dyn Tool>, args: serde_json::Value) -> Result<String, String> {
    tool.call(args, &ToolContext::default())
        .await
        .map(|o| o.content)
        .map_err(|e| e.to_string())
}

fn tool(mgr: &ExtensionManager, name: &str) -> Arc<dyn Tool> {
    mgr.tools()
        .into_iter()
        .find(|t| t.definition().name == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

/// The browser lands on the relay's `/cb`; the slot opens on ferrule's
/// first poll, so a very fast owner may get a 404 and reload.
async fn land_on_relay(relay: &MockRelay, location: &str) {
    assert!(location.starts_with(&relay.url), "redirected to the relay");
    for _ in 0..200 {
        match relay.visit(location).await {
            200 => return,
            404 => tokio::time::sleep(Duration::from_millis(20)).await,
            s => panic!("the relay answered {s}"),
        }
    }
    panic!("the relay slot never opened");
}

/// The owner connects `mock` through the relay; the reply's link.
async fn connect_via_relay(w: &World, command: &str) -> ferrule_connections::Reply {
    let reply = w
        .conns
        .intercept(&Actor::Owner(w.owner.clone()), command)
        .await
        .expect("a connect reply");
    let location = w.provider.approve(&url_button(&reply.buttons)).await;
    land_on_relay(w.relay.as_ref().unwrap(), &location).await;
    w.events.wait_told("is connected").await;
    reply
}

fn assert_no_secret(what: &str, text: &str) {
    assert!(
        !text.contains("SECRET"),
        "a secret leaked into {what}: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_asks_the_owner_taps_once_and_the_tools_go_live_with_the_token_unseen() {
    let w = world(RelayMode::Up).await;
    let mgr = manager(&w);
    let agent_tools = tools::tools(&w.conns, w.owner.clone());
    let request = agent_tools
        .iter()
        .find(|t| t.definition().name == tools::REQUEST)
        .unwrap();

    // The agent notices a missing service and asks.
    let out = call(
        request,
        json!({"service": "mock", "reason": "to search the tracker"}),
    )
    .await
    .unwrap();
    assert!(out.contains("Asked the owner"), "{out}");
    let told = w.events.told.lock().unwrap().clone();
    assert_eq!(told.len(), 1, "one message to the owner");
    let (text, buttons) = &told[0];
    assert!(
        text.contains("Scopes: read") && text.contains("to search the tracker"),
        "{text}"
    );
    assert_eq!(command_buttons(buttons), ["/connect mock", "/decline mock"]);
    // Asking twice doesn't nag.
    let again = call(request, json!({"service": "mock"})).await.unwrap();
    assert!(again.contains("already been asked"), "{again}");
    assert_eq!(w.events.told.lock().unwrap().len(), 1);

    // The owner taps "Connect Mock" (the callback arrives as that command).
    let reply = connect_via_relay(&w, "/connect mock").await;
    assert!(reply.text.contains("Sign in to Mock"), "{}", reply.text);

    // PKCE, a registered client, read-only scope.
    let q = w.provider.st.lock().unwrap().authorize_queries[0].clone();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["scope"], "read");
    assert!(q["redirect_uri"].ends_with("/cb"));
    assert_eq!(q["resource"], w.provider.mcp_url());

    // The tools are live in the same manager, without a restart.
    until("the mock tools", || {
        names(&mgr).contains(&"mcp__mock__search".to_string())
    })
    .await;
    let got = call(&tool(&mgr, "mcp__mock__search"), json!({"q": "bugs"}))
        .await
        .unwrap();
    assert_eq!(got, "found 3 issues");
    assert!(w
        .provider
        .st
        .lock()
        .unwrap()
        .seen_auth
        .iter()
        .any(|a| a.starts_with("Bearer at-SECRET-")));

    // The agent is told in the chat it asked from.
    let connected = w.events.connected.lock().unwrap().clone();
    assert_eq!(connected, vec![("mock".to_string(), Some(w.owner.clone()))]);
    let snap = w.conns.snapshot().unwrap();
    assert_eq!(snap.connections[0].requested_by, "agent");
    assert_eq!(snap.connections[0].via, "relay");
    assert_eq!(snap.connections[0].tools, Some(2));
    assert!(snap.asked.is_empty() && snap.pending.is_empty());

    // The relay slot was read once and is spent.
    let relay = w.relay.as_ref().unwrap();
    assert!(relay
        .st
        .lock()
        .unwrap()
        .slots
        .values()
        .all(|s| s.used && s.value.is_none()));

    // Nowhere a token could be seen: the store file, what the owner and
    // the audit log got, the agent's tools, the snapshot, the MCP configs.
    let file = std::fs::read_to_string(w.conns.store().path()).unwrap();
    assert_no_secret("the store file", &file);
    assert_no_secret(
        "the owner's messages and the audit log",
        &w.events.everything(),
    );
    assert_no_secret("the snapshot", &serde_json::to_string(&snap).unwrap());
    let list = agent_tools
        .iter()
        .find(|t| t.definition().name == tools::LIST)
        .unwrap();
    assert_no_secret("connection_list", &call(list, json!({})).await.unwrap());
    assert_no_secret("the MCP configs", &format!("{:?}", w.conns.servers()));
    assert_no_secret(
        "the tool definitions",
        &format!(
            "{:?}",
            mgr.tools()
                .iter()
                .map(|t| t.definition())
                .collect::<Vec<_>>()
        ),
    );
    for t in w.provider.tokens_issued() {
        assert!(!file.contains(&t));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_relay_and_a_tunnel_the_owner_pastes_the_address_back_once() {
    let w = world(RelayMode::Unreachable).await;
    let owner = Actor::Owner(w.owner.clone());
    let reply = w.conns.intercept(&owner, "/connect mock").await.unwrap();
    assert!(
        reply.text.contains("Copy that page's address"),
        "{}",
        reply.text
    );
    let location = w.provider.approve(&url_button(&reply.buttons)).await;
    assert!(
        location.starts_with("http://127.0.0.1:8976/callback?"),
        "{location}"
    );

    // Tampered: another state is refused and the flow survives.
    let tampered = location.replace("state=", "state=x");
    let r = w.conns.intercept(&owner, &tampered).await.unwrap();
    assert!(
        r.text.contains("doesn't match a pending connection"),
        "{}",
        r.text
    );
    assert_eq!(w.conns.snapshot().unwrap().pending, ["mock"]);

    // Someone else in a group can't use it.
    let stranger = Actor::Other(Chat {
        channel: "telegram".into(),
        id: "666".into(),
    });
    let r = w.conns.intercept(&stranger, "/connect mock").await.unwrap();
    assert_eq!(r.text, "Only the owner can manage connections.");

    // The real one connects.
    let r = w
        .conns
        .intercept(&owner, &format!("here: {location}"))
        .await
        .unwrap();
    assert!(
        r.text.contains("finishing the Mock connection"),
        "{}",
        r.text
    );
    w.events.wait_told("is connected").await;
    assert_eq!(w.conns.snapshot().unwrap().connections[0].via, "paste");

    // Replayed: refused, nothing changes.
    let r = w.conns.intercept(&owner, &location).await.unwrap();
    assert!(
        r.text.contains("doesn't match a pending connection"),
        "{}",
        r.text
    );
    assert_eq!(w.conns.snapshot().unwrap().connections.len(), 1);
    assert_no_secret("everything", &w.events.everything());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stranger_pasting_a_live_code_cancels_the_flow() {
    let w = world(RelayMode::Unreachable).await;
    let owner = Actor::Owner(w.owner.clone());
    let reply = w.conns.intercept(&owner, "/connect mock").await.unwrap();
    let location = w.provider.approve(&url_button(&reply.buttons)).await;
    let stranger = Actor::Other(Chat {
        channel: "telegram".into(),
        id: "666".into(),
    });
    let r = w.conns.intercept(&stranger, &location).await.unwrap();
    assert!(r.text.contains("only the owner's are used"), "{}", r.text);
    w.events.wait_told("someone other than the owner").await;
    let r = w.conns.intercept(&owner, &location).await.unwrap();
    assert!(r.text.contains("doesn't match"), "{}", r.text);
    assert!(w.conns.snapshot().unwrap().connections.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_link_expires_and_says_so() {
    let dir_world = world(RelayMode::Unreachable).await;
    // A fresh service with a short wait.
    let conns = Connections::new(
        &dir_world.dir.path().join("private2"),
        dir_world.conns.config().clone(),
        Arc::new(|_: &str| None),
        dir_world.events.clone(),
        None,
    )
    .unwrap()
    .with_timing(Duration::from_millis(300), Duration::from_millis(20));
    conns
        .intercept(&Actor::Owner(dir_world.owner.clone()), "/connect mock")
        .await
        .unwrap();
    let (text, buttons) = dir_world.events.wait_told("didn't work").await;
    assert!(text.contains("the link expired"), "{text}");
    assert_eq!(command_buttons(&buttons), ["/connect mock"]);
    assert!(conns.snapshot().unwrap().pending.is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_relay_a_quick_tunnel_carries_the_callback_for_dcr() {
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = tempfile::tempdir().unwrap();
    let port_file = bin_dir.path().join("port");
    // A stand-in cloudflared: says its URL and notes which port it fronts.
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in http://127.0.0.1:*) echo \"${{a##*:}}\" > {port};; esac; done\n\
         echo 'INF |  https://quick-test-tunnel.trycloudflare.com  |' >&2\nexec sleep 60\n",
        port = port_file.display()
    );
    let bin = bin_dir.path().join("cloudflared");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let w = world_with(RelayMode::Unreachable, Some(bin.display().to_string())).await;
    let reply = w
        .conns
        .intercept(&Actor::Owner(w.owner.clone()), "/connect mock")
        .await
        .unwrap();
    assert!(
        !reply.text.contains("Copy that page's address"),
        "{}",
        reply.text
    );
    let location = w.provider.approve(&url_button(&reply.buttons)).await;
    assert!(
        location.starts_with("https://quick-test-tunnel.trycloudflare.com/callback?"),
        "{location}"
    );
    // cloudflared would forward this to the loopback listener.
    let port = std::fs::read_to_string(&port_file).unwrap();
    let local = location.replace(
        "https://quick-test-tunnel.trycloudflare.com",
        &format!("http://127.0.0.1:{}", port.trim()),
    );
    let status = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(&local)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    assert!(
        !status.text().await.unwrap().contains("code-"),
        "the page never echoes the code"
    );
    w.events.wait_told("is connected").await;
    assert_eq!(w.conns.snapshot().unwrap().connections[0].via, "tunnel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_token_is_refreshed_and_a_refused_refresh_suspends_the_tools_with_one_notice() {
    let w = world(RelayMode::Up).await;
    let mgr = manager(&w);
    connect_via_relay(&w, "/connect mock").await;
    until("the mock tools", || {
        names(&mgr).contains(&"mcp__mock__search".to_string())
    })
    .await;
    let search = tool(&mgr, "mcp__mock__search");

    // The token expires at the provider: a 401, one refresh, the call works.
    w.provider.expire_access();
    assert_eq!(call(&search, json!({})).await.unwrap(), "found 3 issues");
    assert_eq!(w.provider.st.lock().unwrap().refreshes, 1);
    let snap = w.conns.snapshot().unwrap();
    assert!(snap.connections[0].refreshed_at.is_some());
    assert_eq!(snap.connections[0].state, State::Connected);

    // Now the grant is gone.
    w.provider.expire_access();
    w.provider.st.lock().unwrap().refuse_refresh = true;
    let err = call(&search, json!({})).await.unwrap_err();
    assert!(err.contains("needs reconnecting"), "{err}");
    assert_no_secret("the tool error", &err);
    let (text, buttons) = w.events.wait_told("needs reconnecting").await;
    assert!(text.contains("tools are suspended"), "{text}");
    assert_eq!(command_buttons(&buttons), ["/connect mock"]);

    // Suspended: out of the MCP set, and no second notice.
    until("the tools to go", || {
        !names(&mgr).iter().any(|n| n.starts_with("mcp__mock__"))
    })
    .await;
    let _ = call(&search, json!({})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let notices = w
        .events
        .told
        .lock()
        .unwrap()
        .iter()
        .filter(|(t, _)| t.contains("needs reconnecting"))
        .count();
    assert_eq!(notices, 1);
    let snap = w.conns.snapshot().unwrap();
    assert_eq!(snap.connections[0].state, State::NeedsReconnect);
    assert!(w.conns.servers().is_empty());
    assert!(w
        .conns
        .intercept(&Actor::Owner(w.owner.clone()), "/connections")
        .await
        .unwrap()
        .text
        .contains("needs reconnecting"));

    // Reconnecting brings them back.
    w.provider.st.lock().unwrap().refuse_refresh = false;
    w.events.told.lock().unwrap().clear();
    connect_via_relay(&w, "/connect mock").await;
    until("the tools to return", || {
        names(&mgr).contains(&"mcp__mock__search".to_string())
    })
    .await;
    assert_eq!(
        w.conns.snapshot().unwrap().connections[0].state,
        State::Connected
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_revokes_deletes_the_token_and_removes_the_tools() {
    let w = world(RelayMode::Up).await;
    let mgr = manager(&w);
    connect_via_relay(&w, "/connect mock").await;
    until("the mock tools", || {
        names(&mgr).contains(&"mcp__mock__search".to_string())
    })
    .await;
    let before = w.provider.tokens_issued();
    assert_eq!(before.len(), 2);

    // Not the owner: refused, still connected.
    let agent = Actor::Agent(w.owner.clone());
    let r = w.conns.intercept(&agent, "/disconnect mock").await.unwrap();
    assert_eq!(r.text, "Only the owner can manage connections.");
    assert_eq!(w.conns.snapshot().unwrap().connections.len(), 1);

    let r = w
        .conns
        .intercept(
            &Actor::Owner(w.owner.clone()),
            "/disconnect@ferrule_bot mock",
        )
        .await
        .unwrap();
    assert!(r.text.contains("Mock revoked the grant"), "{}", r.text);
    let mut revoked = w.provider.st.lock().unwrap().revoked.clone();
    revoked.sort();
    let mut issued = before.clone();
    issued.sort();
    assert_eq!(revoked, issued, "both tokens revoked");
    assert!(w.provider.tokens_issued().is_empty());
    assert!(w.conns.store().load().unwrap().is_empty());
    let file = std::fs::read_to_string(w.conns.store().path()).unwrap();
    assert!(!file.contains("\"mock\""));
    until("the tools to go", || {
        !names(&mgr).iter().any(|n| n.starts_with("mcp__mock__"))
    })
    .await;
    assert!(w
        .events
        .events()
        .contains(&"connection_removed".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agent_cannot_approve_its_own_request_or_read_a_token() {
    let w = world(RelayMode::Up).await;
    let agent_tools = tools::tools(&w.conns, w.owner.clone());
    let mut offered: Vec<String> = agent_tools.iter().map(|t| t.definition().name).collect();
    offered.sort();
    assert_eq!(
        offered,
        [tools::LIST, tools::REQUEST],
        "no tool to connect or read"
    );
    assert!(agent_tools.iter().all(|t| !t.changes_files()));

    w.conns.request(&w.owner, "mock", true, "").await.unwrap();
    // The agent's own text, however it's phrased, isn't the owner's tap.
    let agent = Actor::Agent(w.owner.clone());
    for text in ["/connect mock", "/connect mock write", "/decline mock"] {
        let r = w.conns.intercept(&agent, text).await.unwrap();
        assert_eq!(r.text, "Only the owner can manage connections.");
    }
    assert!(w.conns.start(&agent, "mock", false).await.is_err());
    let stranger = Actor::Other(Chat {
        channel: "telegram".into(),
        id: "666".into(),
    });
    assert!(w.conns.start(&stranger, "mock", false).await.is_err());
    let snap = w.conns.snapshot().unwrap();
    assert!(snap.pending.is_empty(), "no flow started");
    assert_eq!(snap.asked, ["mock"], "still waiting on the owner");
    assert_eq!(
        w.events
            .events()
            .iter()
            .filter(|e| *e == "connection_refused")
            .count(),
        3
    );

    // The owner declines: the agent is told not to ask again for now.
    let r = w
        .conns
        .intercept(&Actor::Owner(w.owner.clone()), "/decline mock")
        .await
        .unwrap();
    assert!(r.text.starts_with("Declined"), "{}", r.text);
    let again = w.conns.request(&w.owner, "mock", false, "").await.unwrap();
    assert!(again.contains("declined"), "{again}");

    // Write access is the owner's explicit choice.
    let reply = w
        .conns
        .intercept(&Actor::Owner(w.owner.clone()), "/connect mock write")
        .await
        .unwrap();
    let url = url::Url::parse(&url_button(&reply.buttons)).unwrap();
    let scope = url.query_pairs().find(|(k, _)| k == "scope").unwrap().1;
    assert_eq!(scope, "read write");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_key_goes_through_the_encrypted_form_and_never_through_the_chat() {
    const KEY: &str = "lin_api_SECRET_KEY_123";
    let w = world(RelayMode::Up).await;
    w.provider.st.lock().unwrap().access.insert(KEY.into());
    let mgr = manager(&w);

    w.conns.request(&w.owner, "keyed", false, "").await.unwrap();
    let reply = w
        .conns
        .intercept(&Actor::Owner(w.owner.clone()), "/connect keyed")
        .await
        .unwrap();
    assert!(
        reply.text.contains("encrypted in your browser"),
        "{}",
        reply.text
    );
    let link = url_button(&reply.buttons);
    let relay = w.relay.as_ref().unwrap();
    assert!(link.starts_with(&format!("{}/key#", relay.url)), "{link}");
    let fragment: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(link.split_once('#').unwrap().1.as_bytes())
            .into_owned()
            .collect();

    // The browser encrypts and posts to the slot.
    let envelope = encrypt_key(&fragment["k"], &fragment["s"], KEY);
    assert!(!envelope.to_string().contains(KEY));
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut status = 0;
    for _ in 0..200 {
        status = http
            .post(format!("{}/drop/{}", relay.url, fragment["s"]))
            .json(&envelope)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        if status != 404 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, 200);
    w.events.wait_told("is connected").await;
    until("the keyed tools", || {
        names(&mgr).contains(&"mcp__keyed__search".to_string())
    })
    .await;
    assert_eq!(
        call(&tool(&mgr, "mcp__keyed__search"), json!({}))
            .await
            .unwrap(),
        "found 3 issues"
    );
    assert!(w
        .provider
        .st
        .lock()
        .unwrap()
        .seen_auth
        .contains(&format!("Bearer {KEY}")));

    // The key is nowhere a person or the model reads.
    for (what, text) in [
        ("the reply", format!("{reply:?}")),
        ("the owner's messages and the audit", w.events.everything()),
        (
            "the store",
            std::fs::read_to_string(w.conns.store().path()).unwrap(),
        ),
        (
            "the snapshot",
            serde_json::to_string(&w.conns.snapshot().unwrap()).unwrap(),
        ),
        ("the configs", format!("{:?}", w.conns.servers())),
    ] {
        assert!(!text.contains("SECRET"), "the key leaked into {what}");
    }
    // A second envelope for the spent slot is refused by the relay.
    let replay = http
        .post(format!("{}/drop/{}", relay.url, fragment["s"]))
        .json(&envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 409);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_key_without_the_relay_only_goes_in_at_the_terminal() {
    let w = world(RelayMode::Unreachable).await;
    let r = w
        .conns
        .intercept(&Actor::Owner(w.owner.clone()), "/connect keyed")
        .await
        .unwrap();
    assert!(
        r.text.contains("ferrule connections add keyed"),
        "{}",
        r.text
    );
    assert!(r.buttons.is_empty());
    w.provider.st.lock().unwrap().access.insert("k-123".into());
    let out = w.conns.add_key("keyed", "  k-123 \n").await.unwrap();
    assert!(
        out.contains("Keyed is connected (read-only, 2 tools)"),
        "{out}"
    );
    assert_eq!(w.conns.snapshot().unwrap().connections[0].via, "terminal");

    // A key the service stops accepting: one notice, suspended.
    w.provider.st.lock().unwrap().access.clear();
    let _mgr = manager(&w);
    w.events.wait_told("needs reconnecting").await;
    let states: Vec<State> = w
        .conns
        .snapshot()
        .unwrap()
        .connections
        .iter()
        .map(|c| c.state)
        .collect();
    assert_eq!(states, [State::NeedsReconnect]);
}

#[tokio::test]
async fn a_relay_value_of_the_wrong_kind_is_not_a_sign_in() {
    // An envelope posted to an OAuth flow's slot doesn't connect anything.
    let w = world(RelayMode::Up).await;
    let reply = w
        .conns
        .intercept(&Actor::Owner(w.owner.clone()), "/connect mock")
        .await
        .unwrap();
    let url = url_button(&reply.buttons);
    let state = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let relay = w.relay.as_ref().unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let form = ferrule_connections::keyform::KeyForm::new().unwrap();
    let envelope = encrypt_key(&form.public, &state, "whatever");
    for _ in 0..200 {
        let s = http
            .post(format!("{}/drop/{state}", relay.url))
            .json(&envelope)
            .send()
            .await
            .unwrap()
            .status();
        if s != 404 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (text, _) = w.events.wait_told("didn't work").await;
    assert!(
        text.contains("something other than a sign-in code"),
        "{text}"
    );
    assert!(w.conns.snapshot().unwrap().connections.is_empty());
}

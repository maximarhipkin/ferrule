use super::*;
use crate::update::state::{EventKind, State};
use ferrule_gateway::{GatewayError, InboundMessage, OutboundMessage};
use std::sync::Mutex;

#[derive(Default)]
struct Told(Mutex<Vec<String>>);

#[async_trait::async_trait]
impl Owner for Told {
    fn tell(&self, text: String) {
        self.0.lock().unwrap().push(text);
    }
    async fn ask(&self, _: &str, _: &str) -> bool {
        false
    }
}

impl Told {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

struct Polling(Option<SystemTime>);

#[async_trait::async_trait]
impl Channel for Polling {
    fn name(&self) -> &str {
        "telegram"
    }
    async fn run(&self, _: tokio::sync::mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        Ok(())
    }
    async fn send(&self, _: OutboundMessage) -> Result<(), GatewayError> {
        Ok(())
    }
    fn polls(&self) -> bool {
        true
    }
    fn last_ok_poll(&self) -> Option<SystemTime> {
        self.0
    }
}

fn check(data: &Path) -> Check {
    Check {
        data: data.to_path_buf(),
        claude: None,
        plans: vec![],
        channels: vec![],
        started: SystemTime::now(),
        disk_floor: 0,
    }
}

#[tokio::test]
async fn a_problem_is_told_once_and_its_fix_once_and_a_quiet_round_says_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let owner = Told::default();
    let mut c = check(tmp.path());
    assert!(c.tick(&owner).await.is_empty());
    assert!(owner.take().is_empty(), "nothing wrong, nothing said");

    c.disk_floor = u64::MAX;
    let p = c.tick(&owner).await;
    assert!(p["disk"].contains("MB free on the disk"), "{p:?}");
    let told = owner.take();
    assert_eq!(told.len(), 1);
    assert!(told[0].starts_with("Self-check:\n• only "), "{told:?}");

    c.tick(&owner).await;
    assert!(owner.take().is_empty(), "the same set: silence");

    c.disk_floor = 0;
    c.tick(&owner).await;
    let told = owner.take();
    assert_eq!(told.len(), 1);
    assert!(told[0].contains("• fixed: only "), "{told:?}");
    assert_eq!(last(tmp.path()).unwrap().1, Problems::new());
}

#[tokio::test]
async fn a_failing_update_check_a_pinned_rollback_and_a_quiet_channel_are_problems() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = crate::update::state_dir(tmp.path());
    let now = state::now();
    let mut st = State {
        last_check: Some(now),
        last_check_ok: Some(false),
        last_ok: Some(now - 4 * 86_400),
        last_error: Some("github.com: connection refused".into()),
        ..State::default()
    };
    st.push(EventKind::RolledBack, "0.6.0", "0.6.1", "");
    st.pinned.push("v0.6.1".into());
    st.save(&dir).unwrap();

    let mut c = check(tmp.path());
    c.channels = vec![Arc::new(Polling(Some(
        SystemTime::now() - Duration::from_secs(11 * 60),
    )))];
    let p = c.problems().await;
    assert_eq!(
        p["updates"],
        "the update check has failed for 4 days: github.com: connection refused"
    );
    assert!(
        p["pinned"].contains("the next release is offered as usual"),
        "{p:?}"
    );
    assert!(
        p["channel:telegram"].starts_with("telegram hasn't connected for 11"),
        "{p:?}"
    );

    // A check that failed once, a day after the last good one, isn't yet.
    let mut st = State::load(&dir);
    st.last_ok = Some(now - 86_400);
    st.pinned.clear();
    st.save(&dir).unwrap();
    c.channels = vec![Arc::new(Polling(Some(SystemTime::now())))];
    assert!(c.problems().await.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_data_dir_it_cant_write_is_a_problem() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes anywhere
    }
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o555)).unwrap();
    let p = check(&data).problems().await;
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(p["data"].contains("isn't writable"), "{p:?}");
}

#[tokio::test]
async fn free_space_reads_the_disk() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(free_space(tmp.path()).unwrap() > 0);
}

//! M31: mock Discord and Slack servers (REST/Web API and the WebSocket),
//! shared by the gateway's tests and ferrule-cli's three-channel daemon
//! test (through `#[path]`). Each runs on its own runtime, so a sync test
//! can drive a daemon subprocess against it as easily as an async one.
#![allow(dead_code)]

pub mod discord;
pub mod http;
pub mod slack;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The mocks' runtime, shared by every mock in a test binary.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}

/// Waits (async) until `f` holds, or panics with `what` after `limit`.
pub async fn until(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits (blocking) until `f` holds, or panics with `what` after `limit`.
pub fn wait(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

/// A listener on 127.0.0.1 for the mocks' runtime (`from_std` it there:
/// a test may itself be async, so nothing here blocks on the runtime).
pub fn bind() -> (std::net::TcpListener, u16) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    (l, port)
}

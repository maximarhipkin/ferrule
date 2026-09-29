//! M30: a model download against a local server. A file whose hash isn't
//! the pinned one is refused, with nothing left under its real name.

use ferrule_embed::download::{self, DownloadError, ModelFile, ModelSpec, Presence};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const BODY: &[u8] = b"pretend these are 64 bytes of embedding weights, give or take...";

/// Serves `BODY` for every GET; returns the base URL and the paths asked for.
fn server() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let paths: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = paths.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            log.lock().unwrap().push(
                first
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string(),
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                BODY.len()
            );
            let _ = stream.write_all(BODY);
        }
    });
    (base, paths)
}

fn spec(sha256: &str, size: u64) -> ModelSpec {
    let files: &'static [ModelFile] = Box::leak(Box::new([ModelFile {
        name: "weights.bin",
        size,
        sha256: Box::leak(sha256.to_string().into_boxed_str()),
    }]));
    ModelSpec {
        repo: "test/model",
        revision: "abcdef0123456789",
        name: "model",
        files,
        rows: 0,
        dim: 0,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn a_checksum_mismatch_is_refused_and_nothing_is_kept() {
    let (base, paths) = server();
    let dir = tempfile::tempdir().unwrap();
    let s = spec(&"ab".repeat(32), BODY.len() as u64);
    let err = download::download(&client(), &s, &base, dir.path(), |_, _, _| {})
        .await
        .unwrap_err();
    assert!(matches!(err, DownloadError::Checksum { .. }), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("SHA-256 mismatch, refusing it"), "{msg}");
    assert!(msg.contains(&"ab".repeat(32)), "{msg}");
    assert!(msg.contains(&download::sha256_bytes(BODY)), "{msg}");
    assert!(!dir.path().join("weights.bin").exists());
    assert!(!dir.path().join("weights.bin.partial").exists());
    assert_ne!(download::presence(&s, dir.path()), Presence::Present);
    // The pinned revision is in the URL, not a branch.
    assert_eq!(
        paths.lock().unwrap()[0],
        "/test/model/resolve/abcdef0123456789/weights.bin"
    );
}

#[tokio::test]
async fn a_short_file_is_refused_too() {
    let (base, _) = server();
    let dir = tempfile::tempdir().unwrap();
    let s = spec(&download::sha256_bytes(BODY), BODY.len() as u64 + 10);
    let err = download::download(&client(), &s, &base, dir.path(), |_, _, _| {})
        .await
        .unwrap_err();
    assert!(matches!(err, DownloadError::Size { .. }), "{err:?}");
    assert!(!dir.path().join("weights.bin").exists());
}

#[tokio::test]
async fn a_matching_file_lands_verifies_and_is_not_fetched_twice() {
    let (base, paths) = server();
    let dir = tempfile::tempdir().unwrap();
    let s = spec(&download::sha256_bytes(BODY), BODY.len() as u64);
    assert_eq!(
        download::presence(&s, dir.path()),
        Presence::Incomplete(vec!["weights.bin".into()])
    );
    let mut last = (0, 0);
    download::download(&client(), &s, &base, dir.path(), |_, done, total| {
        last = (done, total)
    })
    .await
    .unwrap();
    assert_eq!(last, (BODY.len() as u64, BODY.len() as u64));
    assert_eq!(std::fs::read(dir.path().join("weights.bin")).unwrap(), BODY);
    assert_eq!(download::presence(&s, dir.path()), Presence::Present);
    download::verify(&s, dir.path()).unwrap();
    download::download(&client(), &s, &base, dir.path(), |_, _, _| {})
        .await
        .unwrap();
    assert_eq!(paths.lock().unwrap().len(), 1);

    // A file changed on disk after the download fails verify.
    std::fs::write(dir.path().join("weights.bin"), vec![b'x'; BODY.len()]).unwrap();
    let err = download::verify(&s, dir.path()).unwrap_err();
    assert!(err.contains("download again"), "{err}");
}

/// The real download from Hugging Face, pinned revision and hashes.
/// `FERRULE_EMBED_DOWNLOAD_DIR` is where the files go (≈531 MB);
/// `FERRULE_EXTRA_CA` optionally names a PEM bundle to trust as well
/// (a TLS-intercepting proxy).
#[tokio::test]
#[ignore = "downloads 531 MB: set FERRULE_EMBED_DOWNLOAD_DIR"]
async fn downloads_the_real_model() {
    let dir = std::env::var("FERRULE_EMBED_DOWNLOAD_DIR").expect("FERRULE_EMBED_DOWNLOAD_DIR");
    let mut builder = reqwest::Client::builder();
    if let Ok(ca) = std::env::var("FERRULE_EXTRA_CA") {
        for cert in reqwest::Certificate::from_pem_bundle(&std::fs::read(ca).unwrap()).unwrap() {
            builder = builder.add_root_certificate(cert);
        }
    }
    let spec = download::POTION_MULTILINGUAL;
    let dir = std::path::Path::new(&dir).join(spec.tag());
    let started = std::time::Instant::now();
    download::download(
        &builder.build().unwrap(),
        &spec,
        download::HUGGING_FACE,
        &dir,
        |_, _, _| {},
    )
    .await
    .unwrap();
    println!(
        "downloaded to {} in {:.1?}",
        dir.display(),
        started.elapsed()
    );
    download::verify(&spec, &dir).unwrap();
    assert_eq!(download::presence(&spec, &dir), Presence::Present);
}

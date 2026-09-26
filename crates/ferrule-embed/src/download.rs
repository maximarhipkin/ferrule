//! Fetching a local model's files: pinned revision, pinned SHA-256, and
//! nothing lands under its real name unless it matches.
//!
//! No weights ship with ferrule. The owner opts in (`ferrule setup`, or
//! `ferrule memory model download`), and the files go to the data dir.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// One file of a model, as pinned.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: &'static str,
}

/// A model at a pinned revision.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    /// Hugging Face repo, `owner/name`.
    pub repo: &'static str,
    /// A full commit hash, never a branch: a branch can move.
    pub revision: &'static str,
    /// The name used in the model id and the directory.
    pub name: &'static str,
    pub files: &'static [ModelFile],
    /// The embedding matrix: rows × dim, F32.
    pub rows: usize,
    pub dim: usize,
}

/// `minishlab/potion-multilingual-128M`: a model2vec static model
/// distilled from BAAI/bge-m3, 500,353 tokens × 256 dimensions.
pub const POTION_MULTILINGUAL: ModelSpec = ModelSpec {
    repo: "minishlab/potion-multilingual-128M",
    revision: "73908c3438cf03b6a01bcb9611d62b23d0726f08",
    name: "potion-multilingual-128M",
    files: &[
        ModelFile {
            name: "tokenizer.json",
            size: 18_616_131,
            sha256: "19f1909063da3cfe3bd83a782381f040dccea475f4816de11116444a73e1b6a1",
        },
        ModelFile {
            name: "model.safetensors",
            size: 512_361_560,
            sha256: "14b5eb39cb4ce5666da8ad1f3dc6be4346e9b2d601c073302fa0a31bf7943397",
        },
    ],
    rows: 500_353,
    dim: 256,
};

impl ModelSpec {
    /// `name@<first 7 of the revision>`: what goes into the model id.
    pub fn tag(&self) -> String {
        format!("{}@{}", self.name, &self.revision[..7])
    }

    /// Where the files live under the data dir.
    pub fn dir(&self, data_dir: &Path) -> PathBuf {
        data_dir.join("models").join(self.tag())
    }

    /// The download size, in bytes.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn url(&self, base: &str, file: &str) -> String {
        format!(
            "{}/{}/resolve/{}/{file}",
            base.trim_end_matches('/'),
            self.repo,
            self.revision
        )
    }
}

pub const HUGGING_FACE: &str = "https://huggingface.co";

/// What's on disk, judged by file sizes only (cheap enough for every
/// start; [`verify`] hashes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    Missing,
    /// Some files are there, or some have the wrong size.
    Incomplete(Vec<String>),
    Present,
}

pub fn presence(spec: &ModelSpec, dir: &Path) -> Presence {
    let bad: Vec<String> = spec
        .files
        .iter()
        .filter(|f| {
            std::fs::metadata(dir.join(f.name))
                .map(|m| m.len() != f.size)
                .unwrap_or(true)
        })
        .map(|f| f.name.to_string())
        .collect();
    if bad.is_empty() {
        Presence::Present
    } else if bad.len() == spec.files.len() && !dir.exists() {
        Presence::Missing
    } else {
        Presence::Incomplete(bad)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("{file}: SHA-256 mismatch, refusing it (expected {expected}, got {actual}); the partial download was deleted and nothing was enabled")]
    Checksum {
        file: String,
        expected: String,
        actual: String,
    },
    #[error("{file}: expected {expected} bytes, got {actual}; the partial download was deleted")]
    Size {
        file: String,
        expected: u64,
        actual: u64,
    },
    #[error("{file}: HTTP {status} from {url}")]
    Http {
        file: String,
        status: u16,
        url: String,
    },
    #[error("{file}: {message}")]
    Transport { file: String, message: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a file, streamed.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// Hashes every file against the pin. `Err` names the first that fails.
pub fn verify(spec: &ModelSpec, dir: &Path) -> Result<(), String> {
    for f in spec.files {
        let path = dir.join(f.name);
        let actual = sha256_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if actual != f.sha256 {
            return Err(format!(
                "{}: SHA-256 {actual}, expected {}; delete it and download again",
                path.display(),
                f.sha256
            ));
        }
    }
    Ok(())
}

/// Downloads every file of `spec` that isn't already there with the right
/// size, from `base` (normally [`HUGGING_FACE`]). Each streams to
/// `<file>.partial`, hashed on the way, and is renamed into place only if
/// its size and hash match the pin; otherwise the partial file is deleted
/// and the whole download fails. `progress(file, done, total)` is called
/// as bytes arrive.
pub async fn download(
    client: &reqwest::Client,
    spec: &ModelSpec,
    base: &str,
    dir: &Path,
    mut progress: impl FnMut(&str, u64, u64),
) -> Result<(), DownloadError> {
    std::fs::create_dir_all(dir)?;
    for f in spec.files {
        let dest = dir.join(f.name);
        if std::fs::metadata(&dest)
            .map(|m| m.len() == f.size)
            .unwrap_or(false)
        {
            progress(f.name, f.size, f.size);
            continue;
        }
        let partial = dir.join(format!("{}.partial", f.name));
        let result = fetch(client, spec, base, f, &partial, &mut progress).await;
        match result {
            Ok(()) => std::fs::rename(&partial, &dest)?,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
        }
    }
    Ok(())
}

async fn fetch(
    client: &reqwest::Client,
    spec: &ModelSpec,
    base: &str,
    f: &ModelFile,
    partial: &Path,
    progress: &mut impl FnMut(&str, u64, u64),
) -> Result<(), DownloadError> {
    let url = spec.url(base, f.name);
    let transport = |e: reqwest::Error| DownloadError::Transport {
        file: f.name.into(),
        message: e.without_url().to_string(),
    };
    let mut resp = client.get(&url).send().await.map_err(transport)?;
    if !resp.status().is_success() {
        return Err(DownloadError::Http {
            file: f.name.into(),
            status: resp.status().as_u16(),
            url,
        });
    }
    let mut out = std::fs::File::create(partial)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut done = 0u64;
    while let Some(chunk) = resp.chunk().await.map_err(transport)? {
        done += chunk.len() as u64;
        if done > f.size {
            return Err(DownloadError::Size {
                file: f.name.into(),
                expected: f.size,
                actual: done,
            });
        }
        ctx.update(&chunk);
        out.write_all(&chunk)?;
        progress(f.name, done, f.size);
    }
    out.sync_all()?;
    drop(out);
    if done != f.size {
        return Err(DownloadError::Size {
            file: f.name.into(),
            expected: f.size,
            actual: done,
        });
    }
    let actual = hex(ctx.finish().as_ref());
    if actual != f.sha256 {
        return Err(DownloadError::Checksum {
            file: f.name.into(),
            expected: f.sha256.into(),
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_pin_the_revision() {
        let s = POTION_MULTILINGUAL;
        assert_eq!(
            s.url(HUGGING_FACE, "tokenizer.json"),
            "https://huggingface.co/minishlab/potion-multilingual-128M/resolve/73908c3438cf03b6a01bcb9611d62b23d0726f08/tokenizer.json"
        );
        assert_eq!(s.tag(), "potion-multilingual-128M@73908c3");
        assert_eq!(s.total_size(), 530_977_691);
        // The matrix file is the header (8 + 80 bytes) and rows × dim f32s.
        assert_eq!(s.files[1].size, 8 + 80 + (s.rows * s.dim * 4) as u64);
    }

    #[test]
    fn sha256_of_known_input() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

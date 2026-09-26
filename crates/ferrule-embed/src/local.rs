//! A model2vec static embedding model, in process.
//!
//! Embedding a text: tokenise it (no special tokens), drop unknown tokens,
//! keep the first 512, look up each token's row in the embedding matrix,
//! average the rows and normalise. There is no network model and no matrix
//! multiply.
//!
//! The matrix (512 MB for potion-multilingual-128M) stays on disk: the
//! safetensors header is parsed here and each row is read where it lies,
//! so memory holds only the tokenizer. The tokenizer is re-hashed against
//! its pin each time a process loads it; the matrix is checked for its
//! exact size and header shape (hashing 512 MB on every start would cost
//! a second).

use crate::download::{self, ModelSpec};
use crate::{normalize, EmbedError, Embedded, Embedder, ModelId, Purpose};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokenizers::Tokenizer;

/// Tokens per text, as model2vec.
const MAX_TOKENS: usize = 512;
/// Characters read per text before tokenising: far more than 512 tokens.
const MAX_CHARS: usize = 8 * 1024;

#[derive(Clone)]
pub struct StaticEmbedder {
    inner: Arc<Inner>,
}

struct Inner {
    spec: ModelSpec,
    dir: PathBuf,
    id: ModelId,
    loaded: OnceLock<Result<Loaded, String>>,
}

struct Loaded {
    tokenizer: Tokenizer,
    matrix: File,
    /// Where row 0 starts in the file.
    data_start: u64,
    unk: Option<u32>,
}

impl StaticEmbedder {
    /// Nothing is read until the first embed (or [`StaticEmbedder::load`]).
    pub fn new(spec: ModelSpec, dir: impl Into<PathBuf>) -> Self {
        let id = ModelId::new("local", &spec.tag(), spec.dim);
        Self {
            inner: Arc::new(Inner {
                spec,
                dir: dir.into(),
                id,
                loaded: OnceLock::new(),
            }),
        }
    }

    /// Loads the model now; the error says what's wrong and what to do.
    pub fn load(&self) -> Result<(), EmbedError> {
        self.inner.loaded().map(|_| ())
    }

    /// The same as [`Embedder::embed`], without the async wrapper.
    pub fn embed_sync(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.inner.embed(texts)
    }
}

impl Inner {
    fn loaded(&self) -> Result<&Loaded, EmbedError> {
        self.loaded
            .get_or_init(|| load(&self.spec, &self.dir))
            .as_ref()
            .map_err(|e| EmbedError::Model(e.clone()))
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let m = self.loaded()?;
        let dim = self.spec.dim;
        let mut rows: HashMap<u32, Vec<f32>> = HashMap::new();
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let clipped: String = text.chars().take(MAX_CHARS).collect();
            let enc = m
                .tokenizer
                .encode_fast(clipped, false)
                .map_err(|e| EmbedError::Model(format!("tokenizing failed: {e}")))?;
            let ids: Vec<u32> = enc
                .get_ids()
                .iter()
                .copied()
                .filter(|id| Some(*id) != m.unk && (*id as usize) < self.spec.rows)
                .take(MAX_TOKENS)
                .collect();
            let mut v = vec![0f32; dim];
            for id in &ids {
                if !rows.contains_key(id) {
                    let row = read_row(m, *id, dim)
                        .map_err(|e| EmbedError::Model(format!("reading the model: {e}")))?;
                    rows.insert(*id, row);
                }
                for (acc, x) in v.iter_mut().zip(&rows[id]) {
                    *acc += x;
                }
            }
            if !ids.is_empty() {
                let n = ids.len() as f32;
                v.iter_mut().for_each(|x| *x /= n);
            }
            normalize(&mut v);
            out.push(v);
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Embedder for StaticEmbedder {
    fn model(&self) -> &ModelId {
        &self.inner.id
    }

    async fn embed(&self, texts: &[String], _purpose: Purpose) -> Result<Embedded, EmbedError> {
        let inner = self.inner.clone();
        let texts = texts.to_vec();
        let vectors = tokio::task::spawn_blocking(move || inner.embed(&texts))
            .await
            .map_err(|e| EmbedError::Model(format!("the embedder stopped: {e}")))??;
        Ok(Embedded { vectors, tokens: 0 })
    }
}

fn load(spec: &ModelSpec, dir: &Path) -> Result<Loaded, String> {
    let fetch = "run `ferrule memory model download` (or `ferrule setup`)";
    if let download::Presence::Missing | download::Presence::Incomplete(_) =
        download::presence(spec, dir)
    {
        return Err(format!(
            "the local embedding model isn't downloaded (or is incomplete) in {}; {fetch}",
            dir.display()
        ));
    }
    let tok_file = spec
        .files
        .iter()
        .find(|f| f.name == "tokenizer.json")
        .ok_or("the model spec has no tokenizer.json")?;
    let tok_path = dir.join(tok_file.name);
    let bytes = std::fs::read(&tok_path).map_err(|e| format!("{}: {e}", tok_path.display()))?;
    let actual = download::sha256_bytes(&bytes);
    if actual != tok_file.sha256 {
        return Err(format!(
            "{}: SHA-256 {actual}, expected {}; delete the model directory and {fetch}",
            tok_path.display(),
            tok_file.sha256
        ));
    }
    let tokenizer = Tokenizer::from_bytes(&bytes)
        .map_err(|e| format!("{}: not a tokenizer: {e}", tok_path.display()))?;
    drop(bytes);
    let unk = ["[UNK]", "<unk>"]
        .iter()
        .find_map(|t| tokenizer.token_to_id(t));

    let mat_path = dir.join("model.safetensors");
    let matrix = File::open(&mat_path).map_err(|e| format!("{}: {e}", mat_path.display()))?;
    let data_start =
        check_header(&matrix, spec).map_err(|e| format!("{}: {e}", mat_path.display()))?;
    Ok(Loaded {
        tokenizer,
        matrix,
        data_start,
        unk,
    })
}

/// Parses the safetensors header and checks it holds exactly one F32
/// tensor `embeddings` of the pinned shape, filling the rest of the file.
/// Returns where the data starts.
fn check_header(f: &File, spec: &ModelSpec) -> Result<u64, String> {
    let mut len = [0u8; 8];
    read_at(f, &mut len, 0).map_err(|e| e.to_string())?;
    let n = u64::from_le_bytes(len);
    if n == 0 || n > 1 << 20 {
        return Err(format!("a safetensors header of {n} bytes"));
    }
    let mut header = vec![0u8; n as usize];
    read_at(f, &mut header, 8).map_err(|e| e.to_string())?;
    let header: serde_json::Value =
        serde_json::from_slice(&header).map_err(|e| format!("bad safetensors header: {e}"))?;
    let t = &header["embeddings"];
    let bytes = (spec.rows * spec.dim * 4) as u64;
    let want_shape = serde_json::json!([spec.rows, spec.dim]);
    let want_offsets = serde_json::json!([0, bytes]);
    if t["dtype"] != "F32" || t["shape"] != want_shape || t["data_offsets"] != want_offsets {
        return Err(format!(
            "unexpected tensor {t}; expected F32 {want_shape} at {want_offsets}"
        ));
    }
    let start = 8 + n;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    if size != start + bytes {
        return Err(format!("{size} bytes, expected {}", start + bytes));
    }
    Ok(start)
}

fn read_row(m: &Loaded, id: u32, dim: usize) -> std::io::Result<Vec<f32>> {
    let mut buf = vec![0u8; dim * 4];
    read_at(
        &m.matrix,
        &mut buf,
        m.data_start + u64::from(id) * (dim as u64) * 4,
    )?;
    Ok(crate::from_bytes(&buf).unwrap_or_default())
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, offset)
}

#[cfg(windows)]
fn read_at(f: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "short read",
                ))
            }
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::ModelFile;

    /// A tiny word-level tokenizer and a 4 × 3 matrix, written as the real
    /// files would be, with their hashes pinned in a spec.
    fn tiny(dir: &Path, tamper: bool) -> ModelSpec {
        let tok = serde_json::json!({
            "version": "1.0",
            "truncation": null, "padding": null, "added_tokens": [],
            "normalizer": {"type": "Lowercase"},
            "pre_tokenizer": {"type": "Whitespace"},
            "post_processor": null, "decoder": null,
            "model": {"type": "WordLevel", "unk_token": "[UNK]",
                      "vocab": {"[UNK]": 0, "deploy": 1, "render": 2, "cat": 3}}
        })
        .to_string();
        std::fs::write(dir.join("tokenizer.json"), &tok).unwrap();
        let rows: [[f32; 3]; 4] = [
            [9.0, 9.0, 9.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ];
        let header = br#"{"embeddings":{"dtype":"F32","shape":[4,3],"data_offsets":[0,48]}}"#;
        let mut file = (header.len() as u64).to_le_bytes().to_vec();
        file.extend_from_slice(header);
        for r in rows {
            for x in r {
                file.extend_from_slice(&x.to_le_bytes());
            }
        }
        std::fs::write(dir.join("model.safetensors"), &file).unwrap();
        let tok_sha = if tamper {
            "0".repeat(64)
        } else {
            download::sha256_bytes(tok.as_bytes())
        };
        let files: &'static [ModelFile] = Box::leak(Box::new([
            ModelFile {
                name: "tokenizer.json",
                size: tok.len() as u64,
                sha256: Box::leak(tok_sha.into_boxed_str()),
            },
            ModelFile {
                name: "model.safetensors",
                size: file.len() as u64,
                sha256: Box::leak(download::sha256_bytes(&file).into_boxed_str()),
            },
        ]));
        ModelSpec {
            repo: "test/tiny",
            revision: "0123456789abcdef",
            name: "tiny",
            files,
            rows: 4,
            dim: 3,
        }
    }

    #[tokio::test]
    async fn pools_rows_drops_unknown_tokens_and_normalises() {
        let dir = tempfile::tempdir().unwrap();
        let e = StaticEmbedder::new(tiny(dir.path(), false), dir.path());
        assert_eq!(e.model().as_str(), "local:tiny@0123456/3");
        let texts = vec![
            "Deploy render".into(),
            "deploy zebra".into(),
            "zebra".into(),
        ];
        let v = e.embed(&texts, Purpose::Document).await.unwrap().vectors;
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!((v[0][0] - h).abs() < 1e-6 && (v[0][1] - h).abs() < 1e-6 && v[0][2] == 0.0);
        // "zebra" is [UNK]: dropped, not pooled in as row 0.
        assert_eq!(v[1], vec![1.0, 0.0, 0.0]);
        assert_eq!(v[2], vec![0.0, 0.0, 0.0]);
        assert!(download::verify(&tiny(dir.path(), false), dir.path()).is_ok());
    }

    #[test]
    fn a_missing_or_tampered_model_is_refused_with_a_way_out() {
        let dir = tempfile::tempdir().unwrap();
        let spec = tiny(dir.path(), false);
        let missing = StaticEmbedder::new(spec, dir.path().join("nowhere"));
        let err = missing.load().unwrap_err().to_string();
        assert!(err.contains("ferrule memory model download"), "{err}");

        let tampered = StaticEmbedder::new(tiny(dir.path(), true), dir.path());
        let err = tampered.load().unwrap_err().to_string();
        assert!(err.contains("SHA-256"), "{err}");

        // A matrix of the right size but another shape.
        let mut wrong = spec;
        wrong.rows = 3;
        wrong.dim = 4;
        let err = StaticEmbedder::new(wrong, dir.path())
            .load()
            .unwrap_err()
            .to_string();
        assert!(err.contains("unexpected tensor"), "{err}");
    }
}

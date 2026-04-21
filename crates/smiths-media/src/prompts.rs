//! WAV-backed prompt library for IVR playback (slice 4.2 / P9).
//!
//! The IVR runtime refers to prompts by path (`"prompts/welcome.wav"`);
//! resolving that path to decoded PCM is this module's job. Prompts
//! are small (a few seconds), re-used across calls, and must not
//! re-hit disk on every ring — so we keep a bounded LRU of decoded
//! `Arc<Vec<i16>>` blobs keyed by path.
//!
//! ## What it decodes
//!
//! PCM16 LE mono WAV files. One channel, 16-bit signed LE samples,
//! optional `fact` / `LIST` chunks tolerated. Anything else (float,
//! µ-law encoded, stereo) errors with a specific message — prompts
//! shouldn't be recorded in a half-weird format and masking a
//! format error behind a "silent playback" would be worse than a
//! clean refusal at load time.
//!
//! The caller owns rate conversion — prompts in the library are
//! returned at whatever sample rate the file declares; the
//! playback path resamples down to 8 kHz for PCMU if needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use thiserror::Error;

/// Defaults. Operators tune via [`PromptLibrary::with_capacity`] /
/// [`PromptLibrary::with_root`].
const DEFAULT_CAPACITY: usize = 64;

/// Every error the library can surface on load. Distinct variants so
/// the caller (IVR runtime, `record_prompt` tool) can map them into
/// the right `ToolError` / `ProviderError` bucket.
#[derive(Debug, Error)]
pub enum PromptError {
    /// Path didn't resolve to a readable file.
    #[error("prompt `{path}`: not found")]
    NotFound {
        /// Path as requested by the caller.
        path: String,
    },
    /// File exists but isn't a RIFF/WAVE container.
    #[error("prompt `{path}`: malformed WAV: {reason}")]
    Malformed {
        /// Path as requested by the caller.
        path: String,
        /// Why the parser gave up.
        reason: String,
    },
    /// Format we don't support (float samples, multi-channel, µ-law).
    #[error("prompt `{path}`: unsupported format: {reason}")]
    Unsupported {
        /// Path as requested by the caller.
        path: String,
        /// Why the format isn't acceptable.
        reason: String,
    },
}

/// Decoded audio handed back on cache hits and fresh decodes. The
/// `Arc` keeps re-lookups cheap — the IVR runtime clones the handle
/// into its per-call playback buffer without re-decoding.
#[derive(Clone, Debug)]
pub struct Prompt {
    /// Canonical resolved path used as the cache key.
    pub path: PathBuf,
    /// Sample rate declared by the WAV `fmt ` chunk.
    pub sample_rate: u32,
    /// Decoded PCM16 LE samples (mono).
    pub samples: Arc<Vec<i16>>,
}

impl Prompt {
    /// Wall-clock duration of the prompt, based on the declared
    /// sample rate. Returns `Duration::ZERO` when the caller
    /// stashed an empty prompt through [`PromptLibrary::insert_raw`].
    #[must_use]
    pub fn duration(&self) -> std::time::Duration {
        if self.sample_rate == 0 {
            return std::time::Duration::ZERO;
        }
        // Sample counts for prompts are tens of thousands, well
        // inside f64's 52-bit mantissa. The explicit `as` keeps the
        // path branch-free.
        #[allow(clippy::cast_precision_loss)]
        let secs = self.samples.len() as f64 / f64::from(self.sample_rate);
        std::time::Duration::from_secs_f64(secs)
    }
}

/// LRU of decoded prompts. Cheap to clone — internals live behind
/// one `Arc<Mutex<..>>` so every call site shares the cache.
#[derive(Clone)]
pub struct PromptLibrary {
    root: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    capacity: usize,
    /// Path → (insertion order, prompt). We bump the order on every
    /// hit so eviction picks the coldest entry.
    entries: HashMap<PathBuf, (u64, Prompt)>,
    counter: u64,
}

impl std::fmt::Debug for PromptLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("PromptLibrary")
            .field("root", &self.root)
            .field("len", &inner.entries.len())
            .field("capacity", &inner.capacity)
            .finish()
    }
}

impl PromptLibrary {
    /// Build a library rooted at `root`. Callers may pass an empty
    /// path to disable root-relative resolution — in that case
    /// every lookup must use an absolute path.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            inner: Arc::new(Mutex::new(Inner {
                capacity: DEFAULT_CAPACITY,
                entries: HashMap::new(),
                counter: 0,
            })),
        }
    }

    /// Override the LRU capacity. Evicts immediately if the new
    /// cap is below the current population.
    #[must_use]
    pub fn with_capacity(self, capacity: usize) -> Self {
        {
            let mut guard = self.lock();
            guard.capacity = capacity.max(1);
            while guard.entries.len() > guard.capacity {
                guard.evict_oldest();
            }
        }
        self
    }

    /// Root the library was created with.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Retrieve or decode a prompt. Cache hits are `O(1)` + an
    /// `Arc` clone; misses fall through to [`Self::load_from_disk`]
    /// and insert. Idempotent — re-entry with the same path
    /// re-serves the cached copy.
    pub fn get(&self, path: impl AsRef<Path>) -> Result<Prompt, PromptError> {
        let resolved = self.resolve(path.as_ref());
        {
            let mut guard = self.lock();
            if guard.entries.contains_key(&resolved) {
                guard.counter += 1;
                let ordinal = guard.counter;
                if let Some(slot) = guard.entries.get_mut(&resolved) {
                    slot.0 = ordinal;
                    return Ok(slot.1.clone());
                }
            }
        }
        let prompt = Self::load_from_disk(&resolved)?;
        let mut guard = self.lock();
        guard.counter += 1;
        let ordinal = guard.counter;
        guard
            .entries
            .insert(resolved.clone(), (ordinal, prompt.clone()));
        if guard.entries.len() > guard.capacity {
            guard.evict_oldest();
        }
        Ok(prompt)
    }

    /// Insert a pre-decoded prompt straight into the cache without
    /// hitting disk. The `record_prompt` MCP tool uses this to stash
    /// a just-recorded WAV for immediate IVR playback before the
    /// bytes hit the filesystem.
    pub fn insert_raw(&self, path: impl AsRef<Path>, sample_rate: u32, samples: Vec<i16>) {
        let resolved = self.resolve(path.as_ref());
        let prompt = Prompt {
            path: resolved.clone(),
            sample_rate,
            samples: Arc::new(samples),
        };
        let mut guard = self.lock();
        guard.counter += 1;
        let ordinal = guard.counter;
        guard.entries.insert(resolved, (ordinal, prompt));
        if guard.entries.len() > guard.capacity {
            guard.evict_oldest();
        }
    }

    /// Drop every cached entry. Useful for tests + for the
    /// `reload_prompts` MCP surface (future).
    pub fn clear(&self) {
        let mut guard = self.lock();
        guard.entries.clear();
    }

    /// Current number of cached prompts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// `true` iff the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().entries.is_empty()
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() || self.root.as_os_str().is_empty() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Read + decode a WAV file off disk. Public so
    /// `record_prompt` can pre-validate a freshly-written file
    /// before letting it into the cache.
    pub fn load_from_disk(path: &Path) -> Result<Prompt, PromptError> {
        let bytes = std::fs::read(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PromptError::NotFound {
                    path: path.display().to_string(),
                }
            } else {
                PromptError::Malformed {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                }
            }
        })?;
        let (sample_rate, samples) =
            decode_wav(&bytes).map_err(|reason| PromptError::Malformed {
                path: path.display().to_string(),
                reason,
            })?;
        Ok(Prompt {
            path: path.to_path_buf(),
            sample_rate,
            samples: Arc::new(samples),
        })
    }
}

impl Inner {
    fn evict_oldest(&mut self) {
        let Some((path, _)) = self
            .entries
            .iter()
            .min_by_key(|(_, (order, _))| *order)
            .map(|(p, (o, _))| (p.clone(), *o))
        else {
            return;
        };
        self.entries.remove(&path);
    }
}

/// Minimal RIFF/WAVE decoder. Handles the `fmt ` chunk (format
/// tag 1, PCM16 LE, mono), skips non-`data` chunks, returns
/// `(sample_rate, samples)` on success. Every error path names
/// exactly what failed — prompt authors should hear "stereo not
/// supported" instead of "unknown error".
fn decode_wav(bytes: &[u8]) -> Result<(u32, Vec<i16>), String> {
    if bytes.len() < 44 {
        return Err("file too small for a RIFF header".into());
    }
    if &bytes[..4] != b"RIFF" {
        return Err("missing RIFF magic".into());
    }
    if &bytes[8..12] != b"WAVE" {
        return Err("missing WAVE magic".into());
    }
    let mut cursor = 12usize;
    let mut format: Option<(u32, u16, u16, u16)> = None; // (rate, bits, channels, format_tag)
    let mut samples: Option<Vec<i16>> = None;
    while cursor + 8 <= bytes.len() {
        let tag = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        cursor += 8;
        if cursor + size > bytes.len() {
            return Err(format!(
                "chunk `{}` overruns file",
                String::from_utf8_lossy(tag)
            ));
        }
        let body = &bytes[cursor..cursor + size];
        match tag {
            b"fmt " => {
                if body.len() < 16 {
                    return Err("fmt chunk too small".into());
                }
                let tag = u16::from_le_bytes([body[0], body[1]]);
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                format = Some((rate, bits, channels, tag));
            }
            b"data" => {
                let Some((rate, bits, channels, tag)) = format else {
                    return Err("data chunk before fmt".into());
                };
                if tag != 1 {
                    return Err(format!("format_tag {tag}: only PCM (1) supported"));
                }
                if channels != 1 {
                    return Err(format!("{channels}-channel audio; mono only"));
                }
                if bits != 16 {
                    return Err(format!("{bits}-bit samples; 16-bit only"));
                }
                if body.len() % 2 != 0 {
                    return Err("data chunk length is odd".into());
                }
                let mut out = Vec::with_capacity(body.len() / 2);
                for chunk in body.chunks_exact(2) {
                    out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                }
                samples = Some(out);
                let _ = rate; // used on return below
            }
            _ => {
                // Skip unknown chunks (LIST, fact, ...).
            }
        }
        // WAV chunks are padded to even size.
        let advance = size + (size & 1);
        cursor += advance;
    }
    let Some((rate, _, _, _)) = format else {
        return Err("no fmt chunk".into());
    };
    let samples = samples.ok_or_else(|| "no data chunk".to_string())?;
    Ok((rate, samples))
}

/// Encode `samples` as a mono PCM16 LE WAV blob with `sample_rate`.
/// Used by the `record_prompt` MCP tool to write incoming audio to
/// disk in a format the library can load back. `write_all`-friendly.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // prompts are seconds-long; fits u32
pub fn encode_wav(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    let byte_rate = sample_rate * 2;
    let data_size = (samples.len() * 2) as u32;
    let chunk_size = 36 + data_size;
    let mut out = Vec::with_capacity(44 + (samples.len() * 2));
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&chunk_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // format tag = PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels = 1
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_wav(path: &Path, rate: u32, samples: &[i16]) {
        std::fs::write(path, encode_wav(rate, samples)).unwrap();
    }

    #[test]
    fn round_trip_encode_decode() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("beep.wav");
        write_wav(&path, 8000, &[0, 1000, -1000, 32767, -32768]);
        let p = PromptLibrary::load_from_disk(&path).unwrap();
        assert_eq!(p.sample_rate, 8000);
        assert_eq!(&*p.samples, &[0, 1000, -1000, 32767, -32768]);
        assert_eq!(
            p.duration(),
            std::time::Duration::from_secs_f64(5.0 / 8000.0)
        );
    }

    #[test]
    fn rejects_non_mono() {
        let mut blob = encode_wav(8000, &[0, 0]);
        // Force channels = 2.
        blob[22] = 2;
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("stereo.wav");
        std::fs::write(&path, &blob).unwrap();
        match PromptLibrary::load_from_disk(&path) {
            Err(PromptError::Malformed { reason, .. }) => {
                assert!(reason.contains("mono"), "{reason}");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn library_caches_hit_on_second_lookup() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("a.wav");
        write_wav(&path, 8000, &[1, 2, 3]);
        let lib = PromptLibrary::with_root(tmp.path());
        let a = lib.get("a.wav").unwrap();
        // Delete the file — a second `get` should still succeed
        // off the cache.
        std::fs::remove_file(&path).unwrap();
        let b = lib.get("a.wav").unwrap();
        assert_eq!(a.samples.as_ptr(), b.samples.as_ptr());
    }

    #[test]
    fn lru_evicts_oldest_when_over_capacity() {
        let tmp = tempdir().unwrap();
        for name in ["a.wav", "b.wav", "c.wav"] {
            write_wav(&tmp.path().join(name), 8000, &[0]);
        }
        let lib = PromptLibrary::with_root(tmp.path()).with_capacity(2);
        lib.get("a.wav").unwrap();
        lib.get("b.wav").unwrap();
        // Touch `a` to keep it hot, then fault `c` in — `b` is the
        // coldest and should evict.
        lib.get("a.wav").unwrap();
        lib.get("c.wav").unwrap();
        assert_eq!(lib.len(), 2);
        std::fs::remove_file(tmp.path().join("b.wav")).unwrap();
        // Now `b` is only on disk (if at all) — library miss
        // should try to reload and fail with NotFound.
        match lib.get("b.wav") {
            Err(PromptError::NotFound { .. }) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn insert_raw_bypasses_disk() {
        let lib = PromptLibrary::with_root("");
        lib.insert_raw("/tmp/none.wav", 16000, vec![10, 20, 30]);
        let p = lib.get("/tmp/none.wav").unwrap();
        assert_eq!(p.sample_rate, 16000);
        assert_eq!(&*p.samples, &[10, 20, 30]);
    }

    #[test]
    fn missing_file_is_not_found() {
        let lib = PromptLibrary::with_root("/nonexistent-root");
        match lib.get("nope.wav") {
            Err(PromptError::NotFound { .. }) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}

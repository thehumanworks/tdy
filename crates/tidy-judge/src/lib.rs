//! Judges: decision models behind one interface.
//!
//! A [`Judge`] answers a [`Request`] (shared text/images + independent typed
//! questions) with probabilities. Backends: OpenAI Decisions
//! (`gpt-6-luna`), SystemOne-compatible APIs (TypeSafe Jev, Cloudflare
//! Clef/Clef-flash), and a deterministic `mock` for tests. [`Cached`] and
//! [`Cascade`] wrap any judge.

pub mod http;
pub mod ir;
pub mod mock;
pub mod openai;
pub mod systemone;

pub use ir::{Answer, Caps, Image, QKind, Question, Request, Response};

use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Debug, thiserror::Error, Clone)]
pub enum JudgeError {
    #[error("judge configuration: {0}")]
    Config(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("judge API error: {0}")]
    Api(String),
    #[error("unexpected judge response: {0}")]
    Protocol(String),
}

pub trait Judge: Send + Sync {
    /// Stable identifier, e.g. `luna:openai-decisions:gpt-6-luna`. Used in
    /// cache keys and calibration, so it must change when the model does.
    fn id(&self) -> String;
    fn caps(&self) -> Caps;
    fn decide(&self, req: &Request) -> Result<Response, JudgeError>;
    fn usage(&self) -> Option<&http::Usage> {
        None
    }
}

/// Backend description (mirrors `[judge.backends.*]` in the policy).
#[derive(Clone, Debug, Default)]
pub struct BackendConfig {
    pub name: String,
    pub kind: String,
    pub model: String,
    pub endpoint: Option<String>,
    pub api_key_env: Option<String>,
    pub images: Option<bool>,
    pub max_questions: Option<usize>,
    pub timeout_secs: Option<u64>,
    pub concurrency: usize,
}

/// Resolve an API key: the named env var, else `fnox get NAME`.
pub fn resolve_key(var: &str) -> Option<String> {
    if let Ok(v) = std::env::var(var) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    if std::env::var_os("TIDY_NO_FNOX").is_some() {
        return None;
    }
    let out = std::process::Command::new("fnox").args(["get", var]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    let k = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !k.is_empty()).then_some(k)
}

pub fn build(cfg: &BackendConfig) -> Result<Box<dyn Judge>, JudgeError> {
    let timeout = Duration::from_secs(cfg.timeout_secs.unwrap_or(60));
    let conc = cfg.concurrency.max(1);
    match cfg.kind.as_str() {
        "openai-decisions" => {
            let var = cfg.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
            let key = resolve_key(var).ok_or_else(|| JudgeError::Config(format!("judge `{}`: {var} is not set (export it or run via `fnox exec --`)", cfg.name)))?;
            let caps = Caps { images: cfg.images.unwrap_or(true), max_images: 8, max_questions: cfg.max_questions.unwrap_or(32) };
            Ok(Box::new(openai::OpenAiDecisions::new(&cfg.name, &cfg.model, cfg.endpoint.as_deref(), &key, timeout, conc, caps)?))
        }
        "systemone" => {
            let default_var = if cfg.endpoint.as_deref().is_some_and(|e| e.contains("cloudflare.com")) { "CLOUDFLARE_API_TOKEN" } else { "TYPESAFE_API_KEY" };
            let var = cfg.api_key_env.as_deref().unwrap_or(default_var);
            let local = cfg.endpoint.as_deref().is_some_and(|e| e.starts_with("http://localhost") || e.starts_with("http://127.0.0.1"));
            let key = match resolve_key(var) {
                Some(k) => k,
                None if local => String::new(),
                None => return Err(JudgeError::Config(format!("judge `{}`: {var} is not set", cfg.name))),
            };
            // Clef accepts up to 4 images; Jev is text-only.
            let clef = cfg.model.contains("clef");
            let caps = Caps { images: cfg.images.unwrap_or(clef), max_images: 4, max_questions: cfg.max_questions.unwrap_or(64) };
            Ok(Box::new(systemone::SystemOne::new(&cfg.name, &cfg.model, cfg.endpoint.as_deref(), &key, timeout, conc, caps)?))
        }
        "mock" => Ok(Box::new(mock::Mock::new(&cfg.name))),
        k => Err(JudgeError::Config(format!("unknown judge kind `{k}`"))),
    }
}

// ---- cache ---------------------------------------------------------------------

/// Persistent decision cache keyed by (judge id, request digest). Unchanged
/// items under an unchanged policy cost nothing on rescans.
pub struct Cached<J> {
    inner: J,
    map: Mutex<HashMap<String, Response>>,
    file: Mutex<Option<std::fs::File>>,
    pub hits: AtomicUsize,
    pub misses: AtomicUsize,
}

impl<J: Judge> Cached<J> {
    pub fn open(inner: J, dir: Option<PathBuf>) -> Self {
        let mut map = HashMap::new();
        let mut file = None;
        if let Some(dir) = dir {
            let _ = std::fs::create_dir_all(&dir);
            let id = inner.id();
            let path = dir.join(format!("{}.jsonl", &blake3::hash(id.as_bytes()).to_hex()[..16]));
            if let Ok(f) = std::fs::File::open(&path) {
                for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                    if let Ok((k, r)) = serde_json::from_str::<(String, Response)>(&line) {
                        map.insert(k, r);
                    }
                }
            }
            file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        }
        Cached { inner, map: Mutex::new(map), file: Mutex::new(file), hits: AtomicUsize::new(0), misses: AtomicUsize::new(0) }
    }
}

impl<J: Judge> Judge for Cached<J> {
    fn id(&self) -> String {
        self.inner.id()
    }
    fn caps(&self) -> Caps {
        self.inner.caps()
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        let key = req.digest();
        if let Some(r) = self.map.lock().get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            let mut r = r.clone();
            r.cached = true;
            return Ok(r);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let r = self.inner.decide(req)?;
        if let Some(f) = self.file.lock().as_mut() {
            if let Ok(mut line) = serde_json::to_vec(&(&key, &r)) {
                line.push(b'\n');
                let _ = f.write_all(&line);
            }
        }
        self.map.lock().insert(key, r.clone());
        Ok(r)
    }
    fn usage(&self) -> Option<&http::Usage> {
        self.inner.usage()
    }
}

impl Judge for Box<dyn Judge> {
    fn id(&self) -> String {
        (**self).id()
    }
    fn caps(&self) -> Caps {
        (**self).caps()
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        (**self).decide(req)
    }
    fn usage(&self) -> Option<&http::Usage> {
        (**self).usage()
    }
}

// ---- cascade ---------------------------------------------------------------------

/// Ask the cheapest judge first; re-ask the next stage when any predicate
/// answer lands in the uncertain band.
pub struct Cascade {
    pub stages: Vec<Box<dyn Judge>>,
    pub band: (f64, f64),
}

impl Cascade {
    fn uncertain(&self, r: &Response) -> bool {
        r.answers.values().any(|a| match a {
            Answer::Predicate { p } => *p > self.band.0 && *p < self.band.1,
            Answer::Refusal => true,
            _ => false,
        })
    }
}

impl Judge for Cascade {
    fn id(&self) -> String {
        format!("cascade[{}]", self.stages.iter().map(|s| s.id()).collect::<Vec<_>>().join(" → "))
    }
    fn caps(&self) -> Caps {
        // The intersection, so every stage can answer every request.
        let mut c = self.stages[0].caps();
        for s in &self.stages[1..] {
            let o = s.caps();
            c.images &= o.images;
            c.max_images = c.max_images.min(o.max_images);
            c.max_questions = c.max_questions.min(o.max_questions);
        }
        c
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        let mut last = None;
        for (i, s) in self.stages.iter().enumerate() {
            let r = s.decide(req)?;
            if i + 1 == self.stages.len() || !self.uncertain(&r) {
                return Ok(r);
            }
            last = Some(r);
        }
        last.ok_or_else(|| JudgeError::Config("empty cascade".into()))
    }
}

// ---- batching ------------------------------------------------------------------

/// Split a request whose questions exceed the judge's limit, answer the
/// parts in sequence and merge.
pub fn decide_split(j: &dyn Judge, req: &Request) -> Result<Response, JudgeError> {
    let max = j.caps().max_questions.max(1);
    if req.questions.len() <= max {
        return j.decide(req);
    }
    let mut merged = Response::default();
    for chunk in req.questions.chunks(max) {
        let part = Request { text: req.text.clone(), images: req.images.clone(), questions: chunk.to_vec() };
        let r = j.decide(&part)?;
        merged.answers.extend(r.answers);
        merged.input_tokens += r.input_tokens;
        merged.latency_ms = merged.latency_ms.max(r.latency_ms);
        merged.judge = r.judge;
        merged.cached = r.cached;
    }
    Ok(merged)
}

/// Run many requests with bounded concurrency. `progress(done, total)` is
/// called after each completion.
pub fn decide_many(j: &dyn Judge, reqs: &[Request], concurrency: usize, progress: &(dyn Fn(usize, usize) + Sync)) -> Vec<Result<Response, JudgeError>> {
    let n = reqs.len();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<Result<Response, JudgeError>>>> = (0..n).map(|_| Mutex::new(None)).collect();
    // Fail fast on auth errors instead of hammering the API.
    let fatal: Mutex<Option<JudgeError>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..concurrency.clamp(1, n.max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let r = if let Some(e) = fatal.lock().clone() { Err(e) } else { decide_split(j, &reqs[i]) };
                    if let Err(e @ (JudgeError::Auth(_) | JudgeError::Config(_))) = &r {
                        *fatal.lock() = Some(e.clone());
                    }
                    *results[i].lock() = Some(r);
                    progress(done.fetch_add(1, Ordering::Relaxed) + 1, n);
                }
            });
        }
    });
    results.into_iter().map(|m| m.into_inner().unwrap_or_else(|| Err(JudgeError::Api("not run".into())))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hits_and_persists() {
        let t = tempfile::tempdir().unwrap();
        let req = Request { text: "a holiday photo of a dog".into(), images: vec![], questions: vec![Question::predicate("dog", "a picture of a dog")] };
        let c = Cached::open(mock::Mock::new("m"), Some(t.path().to_path_buf()));
        let a = c.decide(&req).unwrap();
        let b = c.decide(&req).unwrap();
        assert!(!a.cached && b.cached);
        assert_eq!(c.misses.load(Ordering::Relaxed), 1);
        drop(c);
        let c2 = Cached::open(mock::Mock::new("m"), Some(t.path().to_path_buf()));
        assert!(c2.decide(&req).unwrap().cached);
        assert_eq!(c2.misses.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn split_and_many() {
        struct Small(mock::Mock);
        impl Judge for Small {
            fn id(&self) -> String {
                self.0.id()
            }
            fn caps(&self) -> Caps {
                Caps { max_questions: 2, ..self.0.caps() }
            }
            fn decide(&self, r: &Request) -> Result<Response, JudgeError> {
                assert!(r.questions.len() <= 2);
                self.0.decide(r)
            }
        }
        let j = Small(mock::Mock::new("m"));
        let qs: Vec<Question> = (0..5).map(|i| Question::predicate(format!("q{i}"), "dog")).collect();
        let reqs: Vec<Request> = (0..10).map(|i| Request { text: format!("dog {i}"), images: vec![], questions: qs.clone() }).collect();
        let out = decide_many(&j, &reqs, 4, &|_, _| {});
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|r| r.as_ref().unwrap().answers.len() == 5));
    }
}

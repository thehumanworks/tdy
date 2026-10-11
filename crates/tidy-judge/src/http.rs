//! Blocking HTTP transport with retries, jittered backoff and an adaptive
//! (AIMD) concurrency gate shared by all worker threads.

use crate::JudgeError;
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const RETRYABLE: &[u16] = &[408, 409, 425, 429, 500, 502, 503, 504, 529];

#[derive(Debug, Default)]
pub struct Usage {
    pub requests: AtomicU64,
    pub retries: AtomicU64,
    pub input_tokens: AtomicU64,
    pub failures: AtomicU64,
}

impl Usage {
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.retries.load(Ordering::Relaxed),
            self.input_tokens.load(Ordering::Relaxed),
            self.failures.load(Ordering::Relaxed),
        )
    }
}

struct Gate {
    limit: f64,
    in_flight: usize,
    paused_until: Option<Instant>,
}

/// On a 429 every thread pauses briefly and allowed concurrency halves;
/// each success grows it back by about one slot per round.
pub struct Limiter {
    max: f64,
    gate: Mutex<Gate>,
    cv: Condvar,
}

impl Limiter {
    pub fn new(max: usize) -> Self {
        let max = max.max(1) as f64;
        Limiter { max, gate: Mutex::new(Gate { limit: max, in_flight: 0, paused_until: None }), cv: Condvar::new() }
    }
    fn acquire(&self) {
        let mut g = self.gate.lock();
        loop {
            let wait = g.paused_until.and_then(|t| t.checked_duration_since(Instant::now())).filter(|d| !d.is_zero());
            match wait {
                None if g.in_flight < (g.limit as usize).max(1) => {
                    g.in_flight += 1;
                    return;
                }
                None => self.cv.wait(&mut g),
                Some(d) => {
                    self.cv.wait_for(&mut g, d.max(Duration::from_millis(50)));
                }
            }
        }
    }
    fn release(&self, throttled: bool) {
        let mut g = self.gate.lock();
        g.in_flight -= 1;
        let now = Instant::now();
        if throttled {
            if g.paused_until.is_none_or(|t| now >= t) {
                g.limit = (g.limit / 2.0).max(1.0);
                g.paused_until = Some(now + Duration::from_millis(1000 + (jitter() * 1000.0) as u64));
            }
        } else {
            g.limit = self.max.min(g.limit + 1.0 / g.limit.max(1.0));
        }
        self.cv.notify_all();
    }
}

pub fn jitter() -> f64 {
    static S: AtomicU64 = AtomicU64::new(0);
    let mut x = S.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64) | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    S.store(x, Ordering::Relaxed);
    (x >> 11) as f64 / (1u64 << 53) as f64
}

pub struct Http {
    agent: ureq::Agent,
    url: String,
    headers: Vec<(String, String)>,
    pub usage: Usage,
    limiter: Limiter,
    max_retries: u32,
}

/// Refuse to send credentials anywhere but https (or http on localhost).
pub fn validate_url(url: &str) -> Result<(), JudgeError> {
    let lower = url.to_ascii_lowercase();
    let ok = lower.starts_with("https://")
        || lower.starts_with("http://localhost")
        || lower.starts_with("http://127.0.0.1")
        || lower.starts_with("http://[::1]");
    let host_part = lower.split("://").nth(1).unwrap_or("");
    let authority = host_part.split('/').next().unwrap_or("");
    if !ok || authority.contains('@') || authority.is_empty() {
        return Err(JudgeError::Config(format!("refusing to send credentials to {url} (https, or http on localhost, only)")));
    }
    Ok(())
}

impl Http {
    pub fn new(url: String, headers: Vec<(String, String)>, timeout: Duration, concurrency: usize) -> Self {
        let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_global(Some(timeout))
            .max_idle_connections(concurrency.max(4))
            .max_idle_connections_per_host(concurrency.max(4))
            .user_agent(concat!("tidy/", env!("CARGO_PKG_VERSION")))
            .tls_config(tls)
            .build()
            .into();
        Http { agent, url, headers, usage: Usage::default(), limiter: Limiter::new(concurrency), max_retries: 6 }
    }

    /// POST JSON, retrying transient failures. Returns the parsed body.
    pub fn post(&self, body: &serde_json::Value) -> Result<serde_json::Value, JudgeError> {
        let bytes = serde_json::to_vec(body).map_err(|e| JudgeError::Protocol(e.to_string()))?;
        let mut last = String::new();
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                self.usage.retries.fetch_add(1, Ordering::Relaxed);
                let secs = (0.5 * 2f64.powi(attempt as i32 - 1)).min(20.0) * (0.5 + jitter());
                std::thread::sleep(Duration::from_secs_f64(secs));
            }
            self.limiter.acquire();
            let mut req = self.agent.post(&self.url).header("Content-Type", "application/json");
            for (k, v) in &self.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let sent = req.send(&bytes[..]);
            let (status, text, retry_after) = match sent {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let retry_after =
                        resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok());
                    let text = resp.body_mut().with_config().limit(64 << 20).read_to_string().unwrap_or_default();
                    (status, text, retry_after)
                }
                Err(e) => {
                    self.limiter.release(false);
                    last = e.to_string();
                    continue;
                }
            };
            self.limiter.release(status == 429 || status == 529);
            self.usage.requests.fetch_add(1, Ordering::Relaxed);
            if status == 200 {
                return serde_json::from_str(&text).map_err(|e| JudgeError::Protocol(format!("unreadable response: {e}")));
            }
            let snippet: String = text.chars().take(400).collect();
            if status == 401 || status == 403 {
                self.usage.failures.fetch_add(1, Ordering::Relaxed);
                return Err(JudgeError::Auth(format!("HTTP {status}: {snippet}")));
            }
            if RETRYABLE.contains(&status) {
                last = format!("HTTP {status}: {snippet}");
                if let Some(s) = retry_after {
                    std::thread::sleep(Duration::from_secs_f64(s.clamp(0.0, 30.0)));
                }
                continue;
            }
            self.usage.failures.fetch_add(1, Ordering::Relaxed);
            return Err(JudgeError::Api(format!("HTTP {status}: {snippet}")));
        }
        self.usage.failures.fetch_add(1, Ordering::Relaxed);
        Err(JudgeError::Api(format!("gave up after {} retries: {last}", self.max_retries)))
    }
}

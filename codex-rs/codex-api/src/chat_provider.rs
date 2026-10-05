//! provider.rs: provider config + resilience layer for relay-station APIs.
//!
//! Sits next to chat_wire.rs (same crate: `use crate::chat_wire::...`).
//! No HTTP client inside: you pass in status codes, headers and bytes from
//! whatever client codex-rs uses (reqwest), so everything here is unit-testable.
//! UNTESTED: written without a Rust toolchain. Run `cargo test` first.
//!
//! Intended request loop:
//!   let chain = registry.fallback_chain("relay_a")?;
//!   for provider in chain {
//!       let key  = provider.resolve_api_key(&|k| std::env::var(k).ok())?;
//!       let url  = provider.endpoint_url();
//!       let mut attempt = 0;
//!       loop {
//!           // send request; on non-2xx read body + Retry-After header:
//!           let class = classify_http(status, &body, retry_after.as_deref());
//!           match policy.decide(attempt, class, rand01()) {
//!               Some(wait) => { sleep(wait); attempt += 1; continue; }
//!               None => break, // next provider only for Retryable/RateLimited/Quota classes
//!           }
//!       }
//!   }
//!   Mid-stream: feed bytes to SseParser, payloads to StreamAccumulator, and
//!   enforce `check_timeout`. On an unclean end use `advise_after_stream_end`.

use crate::chat_wire::{ChatEvent, Quirks};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

// ───────────────────────── Errors ─────────────────────────

#[derive(Debug)]
pub enum ProviderError {
    MissingKey(String),
    InvalidBaseUrl(String),
    UnknownProvider(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::MissingKey(k) => write!(f, "API key not found (looked for: {k})"),
            ProviderError::InvalidBaseUrl(u) => write!(f, "base_url must start with http:// or https://: {u}"),
            ProviderError::UnknownProvider(n) => write!(f, "unknown provider: {n}"),
        }
    }
}

impl std::error::Error for ProviderError {}

// ───────────────────────── Provider config ─────────────────────────

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    Chat,
    Responses,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>`
    XApiKey,
    /// custom header name from `auth_header` (default `api-key`, as used by Azure)
    Header,
    /// no auth (local servers)
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub name: String,
    /// e.g. "https://relay.example.com/v1" (the /chat/completions part is added for you)
    pub base_url: String,
    pub wire_api: WireApi,
    /// environment variable holding the key (preferred)
    pub env_key: Option<String>,
    /// plaintext key in config (discouraged: ends up in backups and screenshots)
    pub api_key: Option<String>,
    pub auth_style: AuthStyle,
    pub auth_header: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
    pub query_params: BTreeMap<String, String>,
    /// requested model name -> the name this relay expects
    pub model_map: BTreeMap<String, String>,
    pub default_model: Option<String>,
    /// provider keys to try, in order, if this one is down or out of quota
    pub fallback: Vec<String>,
    pub connect_timeout_secs: u64,
    /// reasoning models can be silent for a long time before the first token
    pub first_byte_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
    pub max_retries: u32,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
    pub quirks: Quirks,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig {
            name: String::new(),
            base_url: String::new(),
            wire_api: WireApi::Chat,
            env_key: None,
            api_key: None,
            auth_style: AuthStyle::Bearer,
            auth_header: None,
            extra_headers: BTreeMap::new(),
            query_params: BTreeMap::new(),
            model_map: BTreeMap::new(),
            default_model: None,
            fallback: Vec::new(),
            connect_timeout_secs: 15,
            first_byte_timeout_secs: 120,
            stream_idle_timeout_secs: 60,
            max_retries: 4,
            retry_base_ms: 800,
            retry_max_ms: 15_000,
            quirks: Quirks::default(),
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

impl ProviderConfig {
    /// Returns warnings (not errors) for things that usually go wrong with relays.
    pub fn validate(&self) -> Result<Vec<String>, ProviderError> {
        let b = self.base_url.trim();
        let rest = if let Some(r) = b.strip_prefix("https://") {
            r
        } else if let Some(r) = b.strip_prefix("http://") {
            r
        } else {
            return Err(ProviderError::InvalidBaseUrl(b.to_string()));
        };
        if rest.is_empty() {
            return Err(ProviderError::InvalidBaseUrl(b.to_string()));
        }
        let mut warnings = Vec::new();
        let host = rest.split('/').next().unwrap_or("");
        if b.starts_with("http://") {
            let local = host.starts_with("localhost")
                || host.starts_with("127.")
                || host.starts_with("192.168.")
                || host.starts_with("10.");
            if !local {
                warnings.push("plain http to a remote host: the API key is sent unencrypted".to_string());
            }
        }
        if !rest.contains('/') {
            warnings.push("base_url has no path; most OpenAI-compatible relays need a /v1 suffix".to_string());
        }
        if self.api_key.is_some() {
            warnings.push("api_key is stored in plaintext; prefer env_key".to_string());
        }
        Ok(warnings)
    }

    pub fn endpoint_url(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        let suffix = match self.wire_api {
            WireApi::Chat => "/chat/completions",
            WireApi::Responses => "/responses",
        };
        // users often paste the full endpoint; don't double it
        let mut url = if base.ends_with(suffix) {
            base.to_string()
        } else {
            format!("{base}{suffix}")
        };
        if !self.query_params.is_empty() {
            let q: Vec<String> = self
                .query_params
                .iter()
                .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
                .collect();
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(&q.join("&"));
        }
        url
    }

    pub fn resolve_api_key(
        &self,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<String>, ProviderError> {
        if self.auth_style == AuthStyle::None {
            return Ok(None);
        }
        if let Some(name) = &self.env_key {
            if let Some(v) = env(name) {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    return Ok(Some(v));
                }
            }
        }
        if let Some(k) = &self.api_key {
            if !k.trim().is_empty() {
                return Ok(Some(k.trim().to_string()));
            }
        }
        Err(ProviderError::MissingKey(
            self.env_key.clone().unwrap_or_else(|| "(no env_key or api_key set)".to_string()),
        ))
    }

    pub fn headers(&self, api_key: Option<&str>) -> Vec<(String, String)> {
        let mut h: Vec<(String, String)> = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
        ];
        if let Some(k) = api_key {
            match self.auth_style {
                AuthStyle::Bearer => h.push(("Authorization".to_string(), format!("Bearer {k}"))),
                AuthStyle::XApiKey => h.push(("x-api-key".to_string(), k.to_string())),
                AuthStyle::Header => {
                    let name = self.auth_header.clone().unwrap_or_else(|| "api-key".to_string());
                    h.push((name, k.to_string()));
                }
                AuthStyle::None => {}
            }
        }
        for (k, v) in &self.extra_headers {
            h.push((k.clone(), v.clone()));
        }
        h
    }

    pub fn map_model(&self, requested: &str) -> String {
        self.model_map.get(requested).cloned().unwrap_or_else(|| requested.to_string())
    }
}

pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderConfig>,
}

impl ProviderRegistry {
    pub fn new(providers: BTreeMap<String, ProviderConfig>) -> Self {
        ProviderRegistry { providers }
    }

    pub fn get(&self, key: &str) -> Option<&ProviderConfig> {
        self.providers.get(key)
    }

    /// The provider itself, then its `fallback` list in order (one level, no recursion).
    /// Each fallback provider must use its own model: take `default_model` from it.
    pub fn fallback_chain(&self, start: &str) -> Result<Vec<&ProviderConfig>, ProviderError> {
        let first = self
            .providers
            .get(start)
            .ok_or_else(|| ProviderError::UnknownProvider(start.to_string()))?;
        let mut out = vec![first];
        let mut seen = vec![start.to_string()];
        for n in &first.fallback {
            if seen.contains(n) {
                continue;
            }
            let p = self
                .providers
                .get(n)
                .ok_or_else(|| ProviderError::UnknownProvider(n.clone()))?;
            seen.push(n.clone());
            out.push(p);
        }
        Ok(out)
    }
}

// ───────────────────────── Error classification & retry ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// 5xx, Cloudflare 52x, 408/409/425, transient WAF page, network errors.
    Retryable,
    /// 429 without a quota message. Honour Retry-After if present.
    RateLimited { retry_after: Option<Duration> },
    /// Out of credit (402, or 429 with a quota/balance message). Do not retry; try a fallback.
    QuotaExhausted,
    /// 401 / 403. Wrong or revoked key. Do not retry.
    Auth,
    /// Prompt too long. Trigger context compaction, then resend.
    ContextOverflow,
    /// 404 mentioning a model. Check model_map / default_model.
    ModelNotFound,
    /// 400 / 422. Usually a request field the relay rejects: fix via quirks.
    BadRequest,
    Fatal,
}

fn parse_retry_after(s: &str) -> Option<Duration> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0 && *v < 86_400.0)
        .map(Duration::from_secs_f64)
}

pub fn classify_http(status: u16, body: &str, retry_after: Option<&str>) -> ErrorClass {
    let b = body.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| b.contains(n));
    match status {
        402 => ErrorClass::QuotaExhausted,
        429 => {
            if has(&[
                "insufficient_quota",
                "insufficient quota",
                "exceeded your current quota",
                "quota exceeded",
                "insufficient balance",
                "余额",
                "额度",
            ]) {
                ErrorClass::QuotaExhausted
            } else {
                ErrorClass::RateLimited { retry_after: retry_after.and_then(parse_retry_after) }
            }
        }
        401 => ErrorClass::Auth,
        403 => {
            if has(&["cloudflare", "just a moment", "<html"]) {
                ErrorClass::Retryable // transient WAF challenge on the relay
            } else {
                ErrorClass::Auth
            }
        }
        400 | 413 | 422 => {
            if status == 413
                || has(&[
                    "context_length_exceeded",
                    "maximum context length",
                    "context window",
                    "too many tokens",
                    "prompt is too long",
                    "input is too long",
                    "token limit",
                    "上下文长度",
                    "上下文超",
                ])
            {
                ErrorClass::ContextOverflow
            } else {
                ErrorClass::BadRequest
            }
        }
        404 => {
            if has(&["model"]) {
                ErrorClass::ModelNotFound
            } else {
                ErrorClass::BadRequest // usually a wrong base_url
            }
        }
        408 | 409 | 425 | 500..=599 => ErrorClass::Retryable,
        _ => ErrorClass::Fatal,
    }
}

/// Network-level failure (no HTTP status). Certificate errors never succeed on retry.
pub fn classify_transport(is_cert_error: bool) -> ErrorClass {
    if is_cert_error {
        ErrorClass::Fatal
    } else {
        ErrorClass::Retryable
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base: Duration,
    pub max: Duration,
}

impl RetryPolicy {
    pub fn from_provider(p: &ProviderConfig) -> Self {
        RetryPolicy {
            max_retries: p.max_retries,
            base: Duration::from_millis(p.retry_base_ms),
            max: Duration::from_millis(p.retry_max_ms),
        }
    }

    /// `attempt` is 0 for the first failure. `jitter` in 0..1 (pass a random number).
    /// Returns how long to wait before retrying, or None to stop.
    pub fn decide(&self, attempt: u32, class: ErrorClass, jitter: f64) -> Option<Duration> {
        if attempt >= self.max_retries {
            return None;
        }
        let j = jitter.clamp(0.0, 1.0);
        let backoff = || {
            let shift = attempt.min(16);
            let exp = self.base.saturating_mul(1u32 << shift);
            let capped = exp.min(self.max);
            let half = capped / 2;
            half + half.mul_f64(j) // "equal jitter": between 50% and 100% of the cap
        };
        match class {
            ErrorClass::Retryable => Some(backoff()),
            ErrorClass::RateLimited { retry_after } => match retry_after {
                // a server asking for minutes of waiting is not an interactive retry
                Some(ra) if ra > Duration::from_secs(120) => None,
                Some(ra) => Some(ra),
                None => Some(backoff()),
            },
            _ => None,
        }
    }
}

// ───────────────────────── Timeouts ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    FirstByte,
    Idle,
}

/// Call on a timer. Count ANY received bytes (including SSE keep-alive comments)
/// as activity when computing `since_last_chunk`.
pub fn check_timeout(
    p: &ProviderConfig,
    got_first_chunk: bool,
    since_request: Duration,
    since_last_chunk: Duration,
) -> Option<TimeoutKind> {
    if !got_first_chunk {
        if since_request > Duration::from_secs(p.first_byte_timeout_secs) {
            return Some(TimeoutKind::FirstByte);
        }
        return None;
    }
    if since_last_chunk > Duration::from_secs(p.stream_idle_timeout_secs) {
        Some(TimeoutKind::Idle)
    } else {
        None
    }
}

// ───────────────────────── SSE parser ─────────────────────────

/// Incremental SSE parser. Handles chunk boundaries anywhere (even inside a
/// multi-byte UTF-8 character), CRLF, multi-line `data:`, comment lines, a
/// missing final blank line, and relays that send bare JSON lines (NDJSON).
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        SseParser::default()
    }

    /// Returns the `data` payload of every event completed by these bytes.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let line_cow = String::from_utf8_lossy(&raw[..raw.len() - 1]);
            let line = line_cow.trim_end_matches('\r');
            self.handle_line(line, &mut events);
        }
        events
    }

    /// Call at end of stream: dispatches an event that lacked its blank line.
    pub fn flush(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.buf.is_empty() {
            let raw = std::mem::take(&mut self.buf);
            let line_cow = String::from_utf8_lossy(&raw);
            let line = line_cow.trim_end_matches('\r').to_string();
            self.handle_line(&line, &mut events);
        }
        if !self.data.is_empty() {
            events.push(self.data.join("\n"));
            self.data.clear();
        }
        events
    }

    fn handle_line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(self.data.join("\n"));
                self.data.clear();
            }
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        if line.starts_with('{') && self.data.is_empty() {
            events.push(line.to_string()); // bare JSON line, no "data:" prefix
            return;
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (line, ""),
        };
        if field == "data" {
            self.data.push(value.to_string());
        }
        // event / id / retry fields are not needed for chat completions
    }
}

// ───────────────────────── Stream-break advice ─────────────────────────

#[derive(Debug, Default, Clone)]
pub struct StreamProgress {
    /// Text or reasoning already shown to the user.
    pub emitted_output: bool,
}

impl StreamProgress {
    pub fn observe(&mut self, ev: &ChatEvent) {
        if matches!(ev, ChatEvent::TextDelta(_) | ChatEvent::ReasoningDelta(_)) {
            self.emitted_output = true;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAdvice {
    /// Stream ended normally.
    Complete,
    /// Nothing was shown yet: resend the same request.
    RetrySilently,
    /// Something was already shown: tell the UI to discard the partial turn, then resend.
    RetryDiscardPartial,
}

/// Retrying is side-effect free because StreamAccumulator only releases tool
/// calls from `finish()`: nothing has executed when a stream breaks mid-turn.
/// So on an unclean end, do NOT call `finish()`; drop the accumulator and retry.
/// Only if retries are exhausted, call `finish()` to surface what exists.
pub fn advise_after_stream_end(ended_cleanly: bool, progress: &StreamProgress) -> RetryAdvice {
    if ended_cleanly {
        RetryAdvice::Complete
    } else if progress.emitted_output {
        RetryAdvice::RetryDiscardPartial
    } else {
        RetryAdvice::RetrySilently
    }
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_wire::{RepairOutcome, StreamAccumulator};

    fn provider(base: &str) -> ProviderConfig {
        ProviderConfig { base_url: base.to_string(), ..ProviderConfig::default() }
    }

    #[test]
    fn endpoint_url_variants() {
        assert_eq!(provider("https://r.example.com/v1").endpoint_url(), "https://r.example.com/v1/chat/completions");
        assert_eq!(provider("https://r.example.com/v1/").endpoint_url(), "https://r.example.com/v1/chat/completions");
        assert_eq!(
            provider("https://r.example.com/v1/chat/completions").endpoint_url(),
            "https://r.example.com/v1/chat/completions"
        );
        let mut p = provider("https://r.example.com/v1");
        p.wire_api = WireApi::Responses;
        assert_eq!(p.endpoint_url(), "https://r.example.com/v1/responses");
        p.query_params.insert("api-version".to_string(), "2024 06".to_string());
        assert_eq!(p.endpoint_url(), "https://r.example.com/v1/responses?api-version=2024%2006");
    }

    #[test]
    fn validate_warns_and_rejects() {
        assert!(provider("relay.example.com").validate().is_err());
        let w = provider("http://relay.example.com").validate().unwrap();
        assert_eq!(w.len(), 2); // plaintext http + no path
        let w = provider("http://127.0.0.1:8080/v1").validate().unwrap();
        assert!(w.is_empty());
    }

    #[test]
    fn key_and_headers() {
        let mut p = provider("https://r.example.com/v1");
        p.env_key = Some("RELAY_KEY".to_string());
        let env = |k: &str| if k == "RELAY_KEY" { Some(" sk-abc \n".to_string()) } else { None };
        let key = p.resolve_api_key(&env).unwrap().unwrap();
        assert_eq!(key, "sk-abc");
        let h = p.headers(Some(&key));
        assert!(h.contains(&("Authorization".to_string(), "Bearer sk-abc".to_string())));

        p.auth_style = AuthStyle::Header;
        let h = p.headers(Some(&key));
        assert!(h.contains(&("api-key".to_string(), "sk-abc".to_string())));

        let none_env = |_: &str| None;
        assert!(matches!(
            provider("https://r.example.com/v1").resolve_api_key(&none_env),
            Err(ProviderError::MissingKey(_))
        ));
    }

    #[test]
    fn model_mapping_and_fallback_chain() {
        let mut a = provider("https://a.example.com/v1");
        a.model_map.insert("gpt-5".to_string(), "relay-gpt5".to_string());
        a.fallback = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(a.map_model("gpt-5"), "relay-gpt5");
        assert_eq!(a.map_model("other"), "other");
        let b = provider("https://b.example.com/v1");
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), a);
        m.insert("b".to_string(), b);
        let reg = ProviderRegistry::new(m);
        assert_eq!(reg.fallback_chain("a").unwrap().len(), 2); // a, b (duplicates skipped)
        assert!(matches!(reg.fallback_chain("zzz"), Err(ProviderError::UnknownProvider(_))));
    }

    #[test]
    fn classify_statuses() {
        assert_eq!(classify_http(401, "", None), ErrorClass::Auth);
        assert_eq!(classify_http(402, "", None), ErrorClass::QuotaExhausted);
        assert_eq!(classify_http(503, "", None), ErrorClass::Retryable);
        assert_eq!(classify_http(524, "", None), ErrorClass::Retryable);
        assert_eq!(classify_http(429, r#"{"error":{"code":"insufficient_quota"}}"#, None), ErrorClass::QuotaExhausted);
        assert_eq!(
            classify_http(429, "slow down", Some("3")),
            ErrorClass::RateLimited { retry_after: Some(Duration::from_secs(3)) }
        );
        assert_eq!(
            classify_http(400, "This model's maximum context length is 128000 tokens", None),
            ErrorClass::ContextOverflow
        );
        assert_eq!(classify_http(400, "unknown field tool_choice", None), ErrorClass::BadRequest);
        assert_eq!(classify_http(404, "The model `x` does not exist", None), ErrorClass::ModelNotFound);
        assert_eq!(classify_http(403, "<html>Just a moment...</html>", None), ErrorClass::Retryable);
    }

    #[test]
    fn retry_decisions() {
        let p = RetryPolicy { max_retries: 3, base: Duration::from_millis(500), max: Duration::from_secs(8) };
        assert_eq!(p.decide(0, ErrorClass::Retryable, 0.0), Some(Duration::from_millis(250)));
        assert_eq!(p.decide(0, ErrorClass::Retryable, 1.0), Some(Duration::from_millis(500)));
        assert_eq!(p.decide(1, ErrorClass::Retryable, 0.0), Some(Duration::from_millis(500)));
        assert_eq!(p.decide(3, ErrorClass::Retryable, 0.5), None);
        assert_eq!(
            p.decide(0, ErrorClass::RateLimited { retry_after: Some(Duration::from_secs(2)) }, 0.5),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            p.decide(0, ErrorClass::RateLimited { retry_after: Some(Duration::from_secs(600)) }, 0.5),
            None
        );
        assert_eq!(p.decide(0, ErrorClass::Auth, 0.5), None);
        assert_eq!(p.decide(0, ErrorClass::QuotaExhausted, 0.5), None);
    }

    #[test]
    fn timeouts() {
        let p = ProviderConfig::default(); // first byte 120s, idle 60s
        let s = Duration::from_secs;
        assert_eq!(check_timeout(&p, false, s(100), s(100)), None);
        assert_eq!(check_timeout(&p, false, s(121), s(121)), Some(TimeoutKind::FirstByte));
        assert_eq!(check_timeout(&p, true, s(500), s(30)), None);
        assert_eq!(check_timeout(&p, true, s(500), s(61)), Some(TimeoutKind::Idle));
    }

    #[test]
    fn sse_handles_split_chunks_crlf_and_multibyte() {
        let mut sse = SseParser::new();
        let full = "data: {\"a\":\"你好\"}\r\n\r\n: ping\n\ndata: [DONE]\n\n".as_bytes().to_vec();
        // split inside the multi-byte character
        let cut = full.iter().position(|&b| b == 0xE4).unwrap() + 1;
        let mut got = sse.push(&full[..cut]);
        got.extend(sse.push(&full[cut..]));
        assert_eq!(got, vec![r#"{"a":"你好"}"#.to_string(), "[DONE]".to_string()]);
    }

    #[test]
    fn sse_multiline_ndjson_and_flush() {
        let mut sse = SseParser::new();
        let got = sse.push(b"data: line1\ndata: line2\n\n{\"x\":1}\n");
        assert_eq!(got, vec!["line1\nline2".to_string(), r#"{"x":1}"#.to_string()]);
        // last event without a trailing blank line
        assert!(sse.push(b"data: tail").is_empty());
        assert_eq!(sse.flush(), vec!["tail".to_string()]);
    }

    #[test]
    fn stream_break_advice() {
        let mut prog = StreamProgress::default();
        assert_eq!(advise_after_stream_end(false, &prog), RetryAdvice::RetrySilently);
        prog.observe(&ChatEvent::TextDelta("hi".to_string()));
        assert_eq!(advise_after_stream_end(false, &prog), RetryAdvice::RetryDiscardPartial);
        assert_eq!(advise_after_stream_end(true, &prog), RetryAdvice::Complete);
    }

    #[test]
    fn done_without_finish_reason_counts_as_clean() {
        let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"w","arguments":"{\"a\":\"x"}}]}}]}"#;
        let mut acc = StreamAccumulator::new(Quirks::default());
        acc.feed(chunk).unwrap();
        assert!(!acc.ended_cleanly());
        acc.feed("[DONE]").unwrap();
        assert!(acc.ended_cleanly());
        let ev = acc.finish();
        match &ev[0] {
            ChatEvent::ToolCall(t) => assert_eq!(t.repair, RepairOutcome::Truncated),
            other => panic!("expected tool call, got {other:?}"),
        }
    }
}

//! chat_wire.rs: Chat Completions adapter for a forked Codex.
//!
//! Depends on `serde_json` and `codex-model-provider-info` (the ChatQuirks type).
//! UNTESTED: written without a Rust toolchain. Run `cargo test` first.
//!
//! Wiring into codex-rs (names may differ in your version):
//!   request : `build_chat_request(...)`, POST to `{base_url}/chat/completions`.
//!   stream  : for every SSE event's `data:` string call
//!             `StreamAccumulator::feed(data)`; when the stream ends (EOF or
//!             `[DONE]`) call `finish()`. Map the events:
//!               TextDelta      -> output-text delta
//!               ReasoningDelta -> reasoning-content delta
//!               ToolCall       -> completed function-call item
//!               Usage          -> token usage
//!               Completed      -> response completed
//!   Unrecoverable tool calls: do NOT execute them. Return a tool error to the
//!   model ("arguments were not valid JSON, resend") so it retries.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

// ───────────────────────── Quirks (per-provider config) ─────────────────────────

// The quirks type lives in codex-model-provider-info so `config.toml` can
// deserialize it per provider (`[model_providers.<id>.quirks]`); re-exported
// here under the name the wire code uses.
pub use codex_model_provider_info::ChatQuirks as Quirks;

// ───────────────────────── Errors ─────────────────────────

#[derive(Debug)]
pub enum ChatWireError {
    BadChunk(String),
    Api(String),
}

impl fmt::Display for ChatWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatWireError::BadChunk(e) => write!(f, "malformed stream chunk: {e}"),
            ChatWireError::Api(e) => write!(f, "provider returned an error: {e}"),
        }
    }
}

impl std::error::Error for ChatWireError {}

// ───────────────────────── Request side ─────────────────────────

#[derive(Debug, Clone)]
pub enum HistoryItem {
    System(String),
    User(String),
    Assistant(String),
    Reasoning(String),
    ToolCall { call_id: String, name: String, arguments: String },
    ToolOutput { call_id: String, output: String },
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Function-style apply_patch for chat models that do not understand freeform tools.
/// Check whether your codex-rs version already ships a function variant before using this.
pub fn apply_patch_function_spec() -> ToolSpec {
    ToolSpec {
        name: "apply_patch".to_string(),
        description: "Edit files by applying a patch. `input` is the full patch text, \
                      starting with `*** Begin Patch` and ending with `*** End Patch`."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "input": { "type": "string", "description": "The full patch text." }
            },
            "required": ["input"]
        }),
    }
}

/// Extract the patch text from the function-call arguments of `apply_patch_function_spec`.
pub fn apply_patch_input_from_args(args: &str) -> Option<String> {
    let (fixed, outcome) = repair_tool_args(args);
    if outcome == RepairOutcome::Unrecoverable {
        return None;
    }
    let v: Value = serde_json::from_str(&fixed).ok()?;
    v.get("input")?.as_str().map(|s| s.to_string())
}

#[derive(Default)]
struct Asst {
    text: String,
    reasoning: Option<String>,
    calls: Vec<(String, String, String)>, // (id, name, arguments)
}

fn normalize_args(args: &str) -> String {
    let (fixed, outcome) = repair_tool_args(args);
    if outcome == RepairOutcome::Unrecoverable {
        "{}".to_string()
    } else {
        fixed
    }
}

fn flush(msgs: &mut Vec<Value>, asst: &mut Option<Asst>, outputs: &HashMap<String, String>, q: &Quirks) {
    let a = match asst.take() {
        Some(a) => a,
        None => return,
    };
    if a.text.is_empty() && a.calls.is_empty() {
        return;
    }
    let mut m = Map::new();
    m.insert("role".to_string(), json!("assistant"));
    if a.calls.is_empty() {
        m.insert("content".to_string(), json!(a.text));
    } else {
        let content = if a.text.is_empty() && q.null_content_on_tool_calls {
            Value::Null
        } else {
            json!(a.text)
        };
        m.insert("content".to_string(), content);
        if q.echo_reasoning {
            if let Some(r) = &a.reasoning {
                m.insert("reasoning_content".to_string(), json!(r));
            }
        }
        let calls: Vec<Value> = a
            .calls
            .iter()
            .map(|(id, name, args)| {
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": normalize_args(args) }
                })
            })
            .collect();
        m.insert("tool_calls".to_string(), Value::Array(calls));
    }
    msgs.push(Value::Object(m));
    // Strict relays require every tool call to be answered right after the assistant message.
    for (id, _, _) in &a.calls {
        let out = outputs
            .get(id)
            .cloned()
            .unwrap_or_else(|| "(no output recorded)".to_string());
        msgs.push(json!({ "role": "tool", "tool_call_id": id, "content": out }));
    }
}

/// Convert Codex-style history into a Chat Completions request body.
/// - all System items are merged into ONE leading system message
/// - consecutive assistant text / tool calls become one assistant message
/// - tool outputs are re-attached right after their assistant message
/// - orphan tool outputs are dropped; calls without output get a placeholder
/// - reasoning is dropped unless `echo_reasoning` is set
pub fn build_chat_request(
    model: &str,
    history: &[HistoryItem],
    tools: &[ToolSpec],
    max_output_tokens: Option<u32>,
    q: &Quirks,
) -> Value {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut outputs: HashMap<String, String> = HashMap::new();
    for item in history {
        match item {
            HistoryItem::System(s) => system_parts.push(s.as_str()),
            HistoryItem::ToolOutput { call_id, output } => {
                outputs.insert(call_id.clone(), output.clone());
            }
            _ => {}
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    if !system_parts.is_empty() {
        messages.push(json!({ "role": "system", "content": system_parts.join("\n\n") }));
    }

    let mut asst: Option<Asst> = None;
    for item in history {
        match item {
            HistoryItem::System(_) | HistoryItem::ToolOutput { .. } => {}
            HistoryItem::User(t) => {
                flush(&mut messages, &mut asst, &outputs, q);
                messages.push(json!({ "role": "user", "content": t }));
            }
            HistoryItem::Assistant(t) => {
                if asst.as_ref().map(|a| !a.calls.is_empty()).unwrap_or(false) {
                    flush(&mut messages, &mut asst, &outputs, q);
                }
                let a = asst.get_or_insert_with(Asst::default);
                a.text.push_str(t);
            }
            HistoryItem::Reasoning(r) => {
                if asst.as_ref().map(|a| !a.calls.is_empty()).unwrap_or(false) {
                    flush(&mut messages, &mut asst, &outputs, q);
                }
                let a = asst.get_or_insert_with(Asst::default);
                match &mut a.reasoning {
                    Some(existing) => existing.push_str(r),
                    None => a.reasoning = Some(r.clone()),
                }
            }
            HistoryItem::ToolCall { call_id, name, arguments } => {
                let a = asst.get_or_insert_with(Asst::default);
                a.calls.push((call_id.clone(), name.clone(), arguments.clone()));
            }
        }
    }
    flush(&mut messages, &mut asst, &outputs, q);

    let mut body = Map::new();
    body.insert("model".to_string(), json!(model));
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert("stream".to_string(), json!(true));
    if !q.omit_stream_options {
        body.insert("stream_options".to_string(), json!({ "include_usage": true }));
    }
    if !tools.is_empty() {
        let specs: Vec<Value> = tools
            .iter()
            .map(|t| {
                let params = if t.parameters.is_null() {
                    json!({ "type": "object", "properties": {} })
                } else {
                    t.parameters.clone()
                };
                json!({
                    "type": "function",
                    "function": { "name": t.name, "description": t.description, "parameters": params }
                })
            })
            .collect();
        body.insert("tools".to_string(), Value::Array(specs));
        if !q.omit_tool_choice {
            body.insert("tool_choice".to_string(), json!("auto"));
        }
        if let Some(p) = q.parallel_tool_calls {
            body.insert("parallel_tool_calls".to_string(), json!(p));
        }
    }
    if let Some(n) = max_output_tokens {
        body.insert(q.max_tokens_field.clone(), json!(n));
    }
    Value::Object(body)
}

// ───────────────────────── Tool-argument JSON repair ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairOutcome {
    /// Already a valid JSON object.
    Valid,
    /// Cosmetic fixes only (fences, trailing comma, double-encoding, leading prose).
    Repaired,
    /// Unterminated structure was closed. Content may be cut short.
    Truncated,
    /// Could not produce a JSON object. Raw text is returned unchanged.
    Unrecoverable,
}

fn strip_fences(t: &str) -> &str {
    let mut s = t.trim();
    if let Some(rest) = s.strip_prefix("```") {
        s = match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        s = s.trim_end();
        if let Some(r) = s.strip_suffix("```") {
            s = r;
        }
        s = s.trim();
    }
    s
}

fn trim_trailing_comma(out: &mut String) {
    let t = out.trim_end().len();
    out.truncate(t);
    if out.ends_with(',') {
        out.pop();
    }
}

/// Close open strings/objects/arrays and drop trailing commas.
/// Returns (text, was_truncated). None if brackets are mismatched.
fn balance(s: &str) -> Option<(String, bool)> {
    let mut out = String::with_capacity(s.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    for ch in s.chars() {
        if in_str {
            out.push(ch);
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = true;
                out.push(ch);
            }
            '{' => {
                stack.push('}');
                out.push(ch);
            }
            '[' => {
                stack.push(']');
                out.push(ch);
            }
            '}' | ']' => {
                if stack.pop() != Some(ch) {
                    return None;
                }
                trim_trailing_comma(&mut out);
                out.push(ch);
                if stack.is_empty() {
                    break; // ignore anything after the root value closes
                }
            }
            _ => out.push(ch),
        }
    }
    let mut truncated = false;
    if in_str {
        truncated = true;
        if esc {
            out.pop(); // dangling backslash
        }
        out.push('"');
    }
    if !stack.is_empty() {
        truncated = true;
    }
    loop {
        let t = out.trim_end().len();
        out.truncate(t);
        if out.ends_with(',') {
            out.pop();
            continue;
        }
        if out.ends_with(':') {
            out.push_str("null");
        }
        break;
    }
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    Some((out, truncated))
}

/// Last resort: cut back to the last complete top-level field and close the object.
fn truncate_to_last_field(s: &str) -> Option<String> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut commas: Vec<usize> = Vec::new();
    for (i, ch) in s.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    for &pos in commas.iter().rev() {
        if let Some((fixed, _)) = balance(&s[..pos]) {
            let ok = serde_json::from_str::<Value>(&fixed)
                .map(|v| v.is_object())
                .unwrap_or(false);
            if ok {
                return Some(fixed);
            }
        }
    }
    None
}

pub fn repair_tool_args(raw: &str) -> (String, RepairOutcome) {
    let t = raw.trim();
    if t.is_empty() {
        return ("{}".to_string(), RepairOutcome::Valid);
    }
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        match &v {
            Value::Object(_) => return (t.to_string(), RepairOutcome::Valid),
            Value::String(inner) => {
                // double-encoded: "{\"a\":1}"
                let it = inner.trim();
                let ok = serde_json::from_str::<Value>(it)
                    .map(|x| x.is_object())
                    .unwrap_or(false);
                if ok {
                    return (it.to_string(), RepairOutcome::Repaired);
                }
                return (raw.to_string(), RepairOutcome::Unrecoverable);
            }
            _ => return (raw.to_string(), RepairOutcome::Unrecoverable),
        }
    }

    let mut candidate = strip_fences(t);
    if let Some(pos) = candidate.find('{') {
        if pos > 0 {
            candidate = &candidate[pos..]; // leading prose before the object
        }
    }
    if let Some((fixed, trunc)) = balance(candidate) {
        let ok = serde_json::from_str::<Value>(&fixed)
            .map(|v| v.is_object())
            .unwrap_or(false);
        if ok {
            let outcome = if trunc { RepairOutcome::Truncated } else { RepairOutcome::Repaired };
            return (fixed, outcome);
        }
    }
    if let Some(fixed) = truncate_to_last_field(candidate) {
        return (fixed, RepairOutcome::Truncated);
    }
    (raw.to_string(), RepairOutcome::Unrecoverable)
}

// ───────────────────────── Response side ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishKind {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub total: u64,
    pub cached_input: u64,
    pub reasoning_output: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallDone {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub repair: RepairOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall(ToolCallDone),
    Usage(TokenUsage),
    Completed { finish: FinishKind },
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

pub struct StreamAccumulator {
    quirks: Quirks,
    calls: BTreeMap<u32, PartialCall>,
    last_index: Option<u32>,
    synthetic: u32,
    finish: Option<FinishKind>,
    usage: Option<TokenUsage>,
    done: bool,
}

fn map_finish(s: &str) -> FinishKind {
    match s {
        "stop" | "end_turn" => FinishKind::Stop,
        "tool_calls" | "function_call" | "tool_use" => FinishKind::ToolCalls,
        "length" | "max_tokens" => FinishKind::Length,
        "content_filter" | "safety" => FinishKind::ContentFilter,
        _ => FinishKind::Other,
    }
}

fn arg_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(), // some relays send arguments as an object
    }
}

fn content_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    out.push_str(t);
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        _ => None,
    }
}

fn parse_usage(u: &Value) -> TokenUsage {
    let g = |v: &Value, k: &str| -> u64 { v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) };
    let input = g(u, "prompt_tokens");
    let output = g(u, "completion_tokens");
    let mut total = g(u, "total_tokens");
    if total == 0 {
        total = input + output;
    }
    let mut cached = u.get("prompt_tokens_details").map(|d| g(d, "cached_tokens")).unwrap_or(0);
    if cached == 0 {
        cached = g(u, "prompt_cache_hit_tokens"); // DeepSeek-style
    }
    let reasoning = u
        .get("completion_tokens_details")
        .map(|d| g(d, "reasoning_tokens"))
        .unwrap_or(0);
    TokenUsage { input, output, total, cached_input: cached, reasoning_output: reasoning }
}

impl StreamAccumulator {
    pub fn new(quirks: Quirks) -> Self {
        StreamAccumulator {
            quirks,
            calls: BTreeMap::new(),
            last_index: None,
            synthetic: 0,
            finish: None,
            usage: None,
            done: false,
        }
    }

    fn new_slot(&mut self) -> u32 {
        let n = self.calls.keys().next_back().map(|k| k + 1).unwrap_or(0);
        self.last_index = Some(n);
        n
    }

    /// Decide which partial call a tool_call fragment belongs to.
    /// Handles relays that omit `index`, reuse index 0 for every call,
    /// or send continuation fragments without an id.
    fn resolve_slot(&mut self, index: Option<u32>, id: Option<&str>) -> u32 {
        if let Some(i) = index {
            if let Some(id) = id {
                let conflict = self
                    .calls
                    .get(&i)
                    .map(|c| !c.id.is_empty() && c.id != id)
                    .unwrap_or(false);
                if conflict {
                    return self.new_slot(); // same index, new id: a new call
                }
            }
            self.last_index = Some(i);
            return i;
        }
        if let Some(id) = id {
            let found = self.calls.iter().find(|(_, c)| c.id == id).map(|(k, _)| *k);
            if let Some(k) = found {
                self.last_index = Some(k);
                return k;
            }
            return self.new_slot();
        }
        match self.last_index {
            Some(k) => k,
            None => self.new_slot(),
        }
    }

    /// Feed the `data:` payload of one SSE event.
    pub fn feed(&mut self, data: &str) -> Result<Vec<ChatEvent>, ChatWireError> {
        let mut out: Vec<ChatEvent> = Vec::new();
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(out);
        }
        if data.is_empty() || !data.starts_with('{') {
            return Ok(out); // keep-alives and comments
        }
        let v: Value = serde_json::from_str(data).map_err(|e| ChatWireError::BadChunk(e.to_string()))?;

        if let Some(err) = v.get("error") {
            if !err.is_null() {
                return Err(ChatWireError::Api(err.to_string()));
            }
        }
        if let Some(u) = v.get("usage") {
            if !u.is_null() {
                self.usage = Some(parse_usage(u));
            }
        }

        let choice = match v.get("choices").and_then(|c| c.get(0)) {
            Some(c) => c,
            None => return Ok(out), // e.g. the trailing usage-only chunk
        };
        // Some relays put the whole message in `message` instead of `delta`.
        let delta = choice.get("delta").or_else(|| choice.get("message"));

        if let Some(text) = delta.and_then(|d| d.get("content")).and_then(content_text) {
            if !text.is_empty() {
                out.push(ChatEvent::TextDelta(text));
            }
        }

        let fields: Vec<String> = match &self.quirks.reasoning_field {
            Some(f) => vec![f.clone()],
            None => vec![
                "reasoning_content".to_string(),
                "reasoning".to_string(),
                "thinking".to_string(),
            ],
        };
        if let Some(d) = delta {
            for f in &fields {
                if let Some(s) = d.get(f.as_str()).and_then(|x| x.as_str()) {
                    if !s.is_empty() {
                        out.push(ChatEvent::ReasoningDelta(s.to_string()));
                        break;
                    }
                }
            }
        }

        if let Some(tcs) = delta.and_then(|d| d.get("tool_calls")).and_then(|t| t.as_array()) {
            for tc in tcs {
                let index = tc.get("index").and_then(|i| i.as_u64()).map(|i| i as u32);
                let id = tc.get("id").and_then(|i| i.as_str()).filter(|s| !s.is_empty());
                let func = tc.get("function");
                let name = func
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .filter(|s| !s.is_empty());
                let args = func
                    .and_then(|f| f.get("arguments"))
                    .map(arg_to_string)
                    .unwrap_or_default();

                let slot = self.resolve_slot(index, id);
                let call = self.calls.entry(slot).or_default();
                if call.id.is_empty() {
                    if let Some(id) = id {
                        call.id = id.to_string();
                    }
                }
                if let Some(n) = name {
                    if call.name.is_empty() {
                        call.name = n.to_string();
                    } else if call.name != n {
                        call.name.push_str(n); // name split across chunks
                    } // identical repeat: ignore
                }
                call.args.push_str(&args);
            }
        }

        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.finish = Some(map_finish(fr));
        }
        Ok(out)
    }

    /// True if the provider signalled a normal end: a finish_reason other than
    /// "length", or `[DONE]` with no finish_reason. A stream that just stops
    /// (connection reset, idle timeout) is NOT clean.
    pub fn ended_cleanly(&self) -> bool {
        match self.finish {
            Some(FinishKind::Length) => false,
            Some(_) => true,
            None => self.done,
        }
    }

    /// Call once when the stream ends. Emits tool calls, usage, then Completed.
    /// If `ended_cleanly()` is false, prefer retrying (see provider.rs) over calling this.
    pub fn finish(&mut self) -> Vec<ChatEvent> {
        let mut out: Vec<ChatEvent> = Vec::new();
        let clean = self.ended_cleanly();
        let calls = std::mem::take(&mut self.calls);
        let had_calls = !calls.is_empty();

        for (idx, c) in calls {
            if c.name.is_empty() && c.args.is_empty() {
                continue;
            }
            let call_id = if c.id.is_empty() {
                self.synthetic += 1;
                format!("call_{}_{}", idx, self.synthetic)
            } else {
                c.id.clone()
            };
            let (mut arguments, mut repair) = if self.quirks.repair_tool_json {
                repair_tool_args(&c.args)
            } else {
                let ok = serde_json::from_str::<Value>(&c.args)
                    .map(|v| v.is_object())
                    .unwrap_or(false);
                (c.args.clone(), if ok { RepairOutcome::Valid } else { RepairOutcome::Unrecoverable })
            };
            // A closed-off structure is only trustworthy if the model finished normally.
            // If the stream was cut or hit max length, never run a half-written call.
            if repair == RepairOutcome::Truncated && !clean {
                arguments = c.args.clone();
                repair = RepairOutcome::Unrecoverable;
            }
            out.push(ChatEvent::ToolCall(ToolCallDone { call_id, name: c.name, arguments, repair }));
        }

        if let Some(u) = self.usage.take() {
            out.push(ChatEvent::Usage(u));
        }

        let finish = match (self.finish.take(), had_calls) {
            (Some(FinishKind::Length), _) => FinishKind::Length,
            (Some(FinishKind::ContentFilter), _) => FinishKind::ContentFilter,
            (_, true) => FinishKind::ToolCalls, // some relays say "stop" even with tool calls
            (Some(f), false) => f,
            (None, false) => FinishKind::Stop,
        };
        out.push(ChatEvent::Completed { finish });
        out
    }
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> Vec<ChatEvent> {
        let mut acc = StreamAccumulator::new(Quirks::default());
        let mut out = Vec::new();
        for c in chunks {
            out.extend(acc.feed(c).unwrap());
        }
        out.extend(acc.finish());
        out
    }

    #[test]
    fn repair_cases() {
        assert_eq!(repair_tool_args(r#"{"a":1}"#).1, RepairOutcome::Valid);
        assert_eq!(repair_tool_args("").0, "{}");

        let (s, o) = repair_tool_args(r#"{"a":1,}"#);
        assert_eq!((s.as_str(), o), (r#"{"a":1}"#, RepairOutcome::Repaired));

        let (s, o) = repair_tool_args("```json\n{\"a\":1}\n```");
        assert_eq!((s.as_str(), o), (r#"{"a":1}"#, RepairOutcome::Repaired));

        let (s, o) = repair_tool_args(r#""{\"a\":1}""#);
        assert_eq!((s.as_str(), o), (r#"{"a":1}"#, RepairOutcome::Repaired));

        let (s, o) = repair_tool_args(r#"{"path":"a.txt","content":"hel"#);
        assert_eq!(o, RepairOutcome::Truncated);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["content"], "hel");

        let (s, o) = repair_tool_args(r#"{"a":"#);
        assert_eq!((s.as_str(), o), (r#"{"a":null}"#, RepairOutcome::Truncated));

        assert_eq!(repair_tool_args("not json at all").1, RepairOutcome::Unrecoverable);
    }

    #[test]
    fn split_arguments_are_joined() {
        let ev = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            "[DONE]",
        ]);
        assert_eq!(
            ev[0],
            ChatEvent::ToolCall(ToolCallDone {
                call_id: "c1".into(),
                name: "shell".into(),
                arguments: r#"{"cmd":"ls"}"#.into(),
                repair: RepairOutcome::Valid,
            })
        );
        match &ev[1] {
            ChatEvent::Usage(u) => assert_eq!((u.input, u.output, u.total), (10, 5, 15)),
            other => panic!("expected usage, got {other:?}"),
        }
        assert_eq!(ev[2], ChatEvent::Completed { finish: FinishKind::ToolCalls });
    }

    #[test]
    fn calls_without_index_get_separate_slots() {
        let ev = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"f","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"g","arguments":"{}"}}]}}]}"#,
        ]);
        let names: Vec<String> = ev
            .iter()
            .filter_map(|e| if let ChatEvent::ToolCall(t) = e { Some(t.name.clone()) } else { None })
            .collect();
        assert_eq!(names, vec!["f", "g"]);
    }

    #[test]
    fn reused_index_with_new_id_is_a_new_call() {
        let ev = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"b","function":{"name":"g","arguments":"{}"}}]}}]}"#,
        ]);
        let n = ev.iter().filter(|e| matches!(e, ChatEvent::ToolCall(_))).count();
        assert_eq!(n, 2);
    }

    #[test]
    fn truncated_call_at_length_is_not_runnable() {
        let ev = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"write","arguments":"{\"a\":\"x"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ]);
        match &ev[0] {
            ChatEvent::ToolCall(t) => assert_eq!(t.repair, RepairOutcome::Unrecoverable),
            other => panic!("expected tool call, got {other:?}"),
        }
        assert_eq!(ev[1], ChatEvent::Completed { finish: FinishKind::Length });
    }

    #[test]
    fn text_and_reasoning_stream_through() {
        let ev = run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#,
            r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(ev[0], ChatEvent::ReasoningDelta("hm".into()));
        assert_eq!(ev[1], ChatEvent::TextDelta("hi".into()));
        assert_eq!(ev[2], ChatEvent::Completed { finish: FinishKind::Stop });
    }

    #[test]
    fn request_builder_pairs_calls_and_outputs() {
        let history = vec![
            HistoryItem::System("sys".into()),
            HistoryItem::User("go".into()),
            HistoryItem::Reasoning("thinking".into()),
            HistoryItem::Assistant("ok".into()),
            HistoryItem::ToolCall { call_id: "c1".into(), name: "f".into(), arguments: "{}".into() },
            HistoryItem::ToolOutput { call_id: "c1".into(), output: "done".into() },
            HistoryItem::ToolCall { call_id: "c2".into(), name: "g".into(), arguments: "{}".into() },
            HistoryItem::ToolOutput { call_id: "orphan".into(), output: "x".into() },
        ];
        let body = build_chat_request("m", &history, &[], None, &Quirks::default());
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5); // system, user, assistant, tool c1, tool c2
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[3]["content"], "done");
        assert_eq!(msgs[4]["content"], "(no output recorded)");
        assert!(msgs[2].get("reasoning_content").is_none());
    }
}

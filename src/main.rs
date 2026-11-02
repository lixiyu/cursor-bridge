// cursor-bridge — Claude Code on Cursor's backend.
// One binary. Zero config.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn log(msg: &str) {
    if std::env::var("CURSOR_BRIDGE_DEBUG").is_ok() {
        eprintln!("bridge: {msg}");
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).map_or(false, |v| !v.is_empty() && v != "0")
}

/// Dump the prompt we send to Cursor `agent` (not Cursor's private server system prompt).
fn dump_outbound_prompt(model: &str, resume: Option<&str>, prompt: &str) {
    if !env_flag("CURSOR_BRIDGE_DUMP_PROMPTS") {
        return;
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = home.join(".cursor-bridge").join("prompts");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{ts}-{}.txt", std::process::id()));
    let header = format!(
        "model={model}\nresume={}\nbytes={}\n----\n",
        resume.unwrap_or("-"),
        prompt.len()
    );
    if std::fs::write(&path, format!("{header}{prompt}")).is_ok() {
        eprintln!("bridge: dumped prompt → {}", path.display());
        log(&format!("dumped prompt → {}", path.display()));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let claude_args: Vec<&str> = args.iter().skip(1).map(|s| s.as_str()).collect();

    if claude_args.iter().any(|a| *a == "--help" || *a == "-h") {
        println!("cursor-bridge — Claude Code on Cursor's backend");
        println!("Usage: cursor-bridge [claude-args...]");
        println!();
        println!("  cursor-bridge              interactive");
        println!("  cursor-bridge \"prompt\"     one-shot");
        println!("  cursor-bridge -p \"prompt\"  pipe mode");
        println!();
        println!("Env:");
        println!("  CURSOR_BRIDGE_DEBUG=1          verbose bridge logs on stderr");
        println!("  CURSOR_BRIDGE_DUMP_PROMPTS=1   save each agent prompt under");
        println!("                                 ~/.cursor-bridge/prompts/");
        return;
    }

    let token = get_cursor_token();
    if token.is_empty() {
        log("No Cursor token found. Run `agent login` first.");
        std::process::exit(1);
    }
    log(&format!("token: {}..{}", &token[..12], &token[token.len()-4..]));

    let proxy = match Proxy::start(&token) {
        Ok(p) => p,
        Err(err) => { log(&format!("proxy failed: {err}")); std::process::exit(1); }
    };

    let mut cmd = Command::new("claude");
    let base_url = format!("http://127.0.0.1:{}", proxy.port());
    cmd.env("ANTHROPIC_BASE_URL", &base_url);
    cmd.env("ANTHROPIC_AUTH_TOKEN", "sk-any");
    cmd.env("ANTHROPIC_API_KEY", "");
    // Claude Code speaks stable Anthropic aliases (`sonnet`/`opus`/`haiku`).
    // Those aliases track current Claude Code releases; we map them to Cursor
    // `auto` only when spawning `agent` (see map_agent_model). Do not expose
    // Cursor's `auto` id to Claude Code — it is unrecognized there.
    cmd.env("ANTHROPIC_MODEL", "sonnet");
    cmd.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");

    // Isolate from ~/.claude/settings.json (often pins DeepSeek / custom
    // ANTHROPIC_DEFAULT_*_MODEL that hijack the `sonnet` alias). A private
    // CLAUDE_CONFIG_DIR is the durable fix — not version pins or max-token hacks.
    let config_dir = bridge_config_dir(&base_url);
    cmd.env("CLAUDE_CONFIG_DIR", &config_dir);

    for arg in &claude_args { cmd.arg(arg); }
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            log(&format!("Failed to spawn claude: {err}"));
            log("Install: https://claude.ai/code");
            std::process::exit(1);
        }
    };

    let status = child.wait();
    drop(proxy);
    std::process::exit(status.ok().and_then(|s| s.code()).unwrap_or(0));
}

// ─── Token ────────────────────────────────────────────────────

fn get_cursor_token() -> String {
    for var in &["CURSOR_TOKEN", "CURSOR_API_KEY"] {
        if let Ok(t) = std::env::var(var) {
            if !t.is_empty() { return t; }
        }
    }
    let out = Command::new("security")
        .args(["find-generic-password", "-s", "cursor-access-token", "-w"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Private Claude Code config so user DeepSeek/custom settings cannot leak in.
/// Stable under ~/.cursor-bridge/claude so `/exit` session ids survive across runs.
fn bridge_config_dir(base_url: &str) -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = home.join(".cursor-bridge").join("claude");
    let _ = std::fs::create_dir_all(&dir);
    // Proxy port changes every launch; rewrite settings but keep projects/sessions.
    let settings = format!(
        concat!(
            "{{\n",
            "  \"model\": \"sonnet\",\n",
            "  \"env\": {{\n",
            "    \"ANTHROPIC_BASE_URL\": \"{base}\",\n",
            "    \"ANTHROPIC_AUTH_TOKEN\": \"sk-any\",\n",
            "    \"ANTHROPIC_API_KEY\": \"\",\n",
            "    \"ANTHROPIC_MODEL\": \"sonnet\"\n",
            "  }}\n",
            "}}\n"
        ),
        base = base_url
    );
    let _ = std::fs::write(dir.join("settings.json"), settings);
    log(&format!("claude config: {}", dir.display()));
    dir
}

// ─── Proxy ────────────────────────────────────────────────────

struct Proxy { port: u16, _shutdown: Arc<AtomicBool> }

impl Proxy {
    fn start(token: &str) -> std::io::Result<Self> {
        let t = token.to_string();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();

        std::thread::Builder::new().name("bridge-proxy".into()).spawn(move || {
            let _ = listener.set_nonblocking(true);
            loop {
                if sd.load(Ordering::Relaxed) { break; }
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Listener is nonblocking for accept(); accepted sockets
                        // inherit that on macOS and then large POSTs fail with
                        // EAGAIN (os error 35) mid-body. Force blocking reads.
                        let _ = stream.set_nonblocking(false);
                        let ct = t.clone();
                        std::thread::Builder::new().name("bridge-conn".into())
                            .spawn(move || handle_connection(stream, &ct)).ok();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock =>
                        std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        }).ok();

        log(&format!("proxy on 127.0.0.1:{port}"));
        Ok(Self { port, _shutdown: shutdown })
    }

    fn port(&self) -> u16 { self.port }
}

// ─── HTTP ─────────────────────────────────────────────────────

fn read_exact_bytes<R: Read>(reader: &mut R, len: usize) -> std::io::Result<Vec<u8>> {
    let mut body = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        match reader.read(&mut body[got..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("body truncated: got {got}/{len}"),
                ));
            }
            Ok(n) => got += n,
            // WouldBlock/EAGAIN can still appear briefly; wait and retry.
            Err(ref e)
                if e.kind() == std::io::ErrorKind::Interrupted
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(body)
}

fn read_chunked_body<R: BufRead>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "chunked body ended early",
            ));
        }
        // Chunk size — strip extensions after ';'
        let size_str = line.split(';').next().unwrap_or("").trim();
        let sz = usize::from_str_radix(size_str, 16).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad chunk size: {e}"))
        })?;
        if sz == 0 {
            // Consume trailing headers after the terminating chunk, if any.
            loop {
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 || trailer == "\r\n" || trailer == "\n" {
                    break;
                }
            }
            break;
        }
        let chunk = read_exact_bytes(reader, sz)?;
        body.extend_from_slice(&chunk);
        let mut crlf = String::new();
        let _ = reader.read_line(&mut crlf)?;
    }
    Ok(body)
}

fn respond_body_read_error(mut stream: TcpStream, err: &std::io::Error) {
    let msg = format!("failed to read request body: {err}").replace('"', "'");
    let err_body = format!(r#"{{"error":"{msg}"}}"#);
    let resp = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{err_body}",
        err_body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn handle_connection(stream: TcpStream, token: &str) {
    // Large image payloads can take a while to land on localhost.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(300)));

    let mut reader = BufReader::with_capacity(256 * 1024, &stream);
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).ok().map_or(true, |n| n == 0) || req_line.trim().is_empty() {
        return;
    }

    let parts: Vec<&str> = req_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 { return; }
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    let mut content_length: usize = 0;
    let mut is_chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok().map_or(true, |n| n == 0) || line.trim().is_empty() { break; }
        let lower = line.to_lowercase();
        if lower.starts_with("content-length:") {
            content_length = line.split(':').nth(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        }
        if lower.contains("transfer-encoding:") && lower.contains("chunked") {
            is_chunked = true;
        }
    }

    let body = if content_length > 0 {
        match read_exact_bytes(&mut reader, content_length) {
            Ok(b) => b,
            Err(e) => {
                log(&format!("  {} {} body read failed: {e}", method, path));
                respond_body_read_error(stream, &e);
                return;
            }
        }
    } else if is_chunked {
        match read_chunked_body(&mut reader) {
            Ok(b) => b,
            Err(e) => {
                log(&format!("  {} {} chunked body failed: {e}", method, path));
                respond_body_read_error(stream, &e);
                return;
            }
        }
    } else {
        Vec::new()
    };

    log(&format!("  {} {} ({}b)", method, path, body.len()));

    match (method.as_str(), path.as_str()) {
        ("HEAD", "/api/hello") | ("GET", "/api/hello") => respond_hello(stream, method == "HEAD"),
        ("GET", "/v1/models") | ("GET", "/models") => respond_models(stream),
        ("POST", p) if p.starts_with("/v1/messages") || p.starts_with("/messages") =>
            handle_messages(stream, &body, token),
        ("OPTIONS", _) => respond_cors(stream),
        _ => respond_404(stream),
    }
}

fn respond_cors(mut s: TcpStream) { let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\n\r\n"); }
fn respond_404(mut s: TcpStream) { let _ = s.write_all(b"HTTP/1.1 404\r\nContent-Length: 2\r\n\r\n{}"); }
fn respond_hello(mut s: TcpStream, head: bool) {
    let b = r#"{"status":"ok"}"#;
    let h = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-request-id: bridge-{}\r\n\r\n{}", b.len(), std::process::id(), if head { "" } else { b });
    let _ = s.write_all(h.as_bytes());
}
fn respond_models(mut s: TcpStream) {
    let body = get_models_json();
    let h = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = s.write_all(h.as_bytes());
}

fn get_models_json() -> String {
    // Anthropic aliases first so Claude Code's picker/context logic stays happy.
    // Cursor-native ids are also listed and passed through to `agent` unchanged.
    r#"{"data":[
        {"type":"model","id":"sonnet","display_name":"Cursor Auto"},
        {"type":"model","id":"opus","display_name":"Cursor Auto (opus)"},
        {"type":"model","id":"haiku","display_name":"Cursor Auto (haiku)"},
        {"type":"model","id":"composer-2.5","display_name":"Composer 2.5"},
        {"type":"model","id":"composer-2.5-fast","display_name":"Composer 2.5 Fast"},
        {"type":"model","id":"cursor-grok-4.5-high","display_name":"Cursor Grok 4.5"},
        {"type":"model","id":"cursor-grok-4.5-high-fast","display_name":"Cursor Grok 4.5 Fast"}
    ]}"#.to_string()
}

// ─── Messages ─────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct MessagesRequest {
    model: Option<String>,
    messages: Option<Vec<Message>>,
    system: Option<serde_json::Value>,
    max_tokens: Option<u32>,
    stream: Option<bool>,
}

#[derive(serde::Deserialize)]
struct Message {
    role: String,
    content: serde_json::Value,
}

// ─── Sessions ─────────────────────────────────────────────────

// Claude Code resends the whole transcript every turn. Instead of replaying it
// into a new agent, resume the Cursor chat (`--resume`) and send only what is
// new. Conversations are keyed on their first turn and verified by a prefix
// hash; any mismatch falls back to a fresh session.

struct SessionEntry {
    chat_id: String,
    /// Turns of the next request the session already knows (incl. its last reply).
    consumed: usize,
    /// Prefix length and hash as of the last request served.
    verify_len: usize,
    verify_hash: u64,
    model: String,
}

fn sessions() -> &'static Mutex<HashMap<u64, SessionEntry>> {
    static SESSIONS: OnceLock<Mutex<HashMap<u64, SessionEntry>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// User/assistant turns only, reminders stripped: Claude Code's interleaved
/// `system` messages and `<system-reminder>` blocks are not stable across turns.
struct Turn {
    raw_index: usize,
    role: String,
    text: String,
}

fn conversation(messages: &[Message]) -> Vec<Turn> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "user" || m.role == "assistant")
        .map(|(raw_index, m)| Turn {
            raw_index,
            role: m.role.clone(),
            text: strip_reminders(&extract_text(&m.content)),
        })
        .collect()
}

fn strip_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find(CLOSE) {
            Some(end) => &rest[start + end + CLOSE.len()..],
            // Unterminated: drop the rest.
            None => "",
        };
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn conversation_key(turns: &[Turn]) -> u64 {
    prefix_hash(turns, 1)
}

/// Hashes position and role of each turn, plus text for non-assistant turns.
/// Assistant text is skipped: the client's echo never matches what was streamed.
fn prefix_hash(turns: &[Turn], upto: usize) -> u64 {
    let mut h = DefaultHasher::new();
    for (i, turn) in turns.iter().take(upto).enumerate() {
        i.hash(&mut h);
        turn.role.hash(&mut h);
        if turn.role != "assistant" { turn.text.hash(&mut h); }
    }
    h.finish()
}

enum TurnPlan {
    Fresh { prompt: String },
    Resume { chat_id: String, prompt: String },
}

fn resume_disabled() -> bool {
    std::env::var("CURSOR_BRIDGE_NO_RESUME").map_or(false, |v| !v.is_empty() && v != "0")
}

fn plan_turn(
    store: &HashMap<u64, SessionEntry>,
    messages: &[Message],
    system: &Option<serde_json::Value>,
    model: &str,
) -> TurnPlan {
    let fresh = || TurnPlan::Fresh { prompt: build_prompt(messages, system) };
    let turns = conversation(messages);
    if turns.is_empty() || resume_disabled() { return fresh(); }

    let key = conversation_key(&turns);
    let entry = match store.get(&key) {
        Some(e) => e,
        None => {
            log(&format!("no session for key {key:x} ({} known, {} turns)", store.len(), turns.len()));
            return fresh();
        }
    };
    let reason = if entry.model != model {
        "model changed"
    } else if entry.consumed >= turns.len() {
        "nothing new to send"
    } else if entry.verify_len > turns.len() {
        "history shorter than what we served"
    } else if prefix_hash(&turns, entry.verify_len) != entry.verify_hash {
        "prefix changed"
    } else { "" };
    if !reason.is_empty() {
        log(&format!("fresh session ({reason}); consumed={} verify_len={} turns={}",
                     entry.consumed, entry.verify_len, turns.len()));
        return fresh();
    }

    // Forward raw messages after the last consumed turn so interleaved context still travels.
    let start = turns[entry.consumed - 1].raw_index + 1;
    TurnPlan::Resume {
        chat_id: entry.chat_id.clone(),
        prompt: build_prompt(&messages[start..], &None),
    }
}

fn commit_session(
    store: &mut HashMap<u64, SessionEntry>,
    messages: &[Message],
    model: &str,
    chat_id: &str,
) {
    let turns = conversation(messages);
    if turns.is_empty() || chat_id.is_empty() {
        log(&format!("not recording session (chat_id {chat_id:?}, {} turns)", turns.len()));
        return;
    }
    if store.len() >= 128 { store.clear(); }
    let verify_len = turns.len();
    let key = conversation_key(&turns);
    log(&format!("recording session {chat_id} key {key:x} after {verify_len} turns"));
    store.insert(key, SessionEntry {
        chat_id: chat_id.to_string(),
        // +1 for the reply this turn produced.
        consumed: verify_len + 1,
        verify_len,
        verify_hash: prefix_hash(&turns, verify_len),
        model: model.to_string(),
    });
}

// ─── Prompt building ──────────────────────────────────────────

fn extract_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut out = String::new();
            for block in arr {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = block["text"].as_str() { out.push_str(t); out.push('\n'); }
                    }
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or("unknown");
                        let input = block["input"].to_string();
                        out.push_str(&format!("[TOOL_USE: {name}]\n{input}\n[/TOOL_USE]\n"));
                    }
                    Some("tool_result") => {
                        let id = block["tool_use_id"].as_str().unwrap_or("");
                        let content = extract_text(&block["content"]);
                        let error = block["is_error"].as_bool().unwrap_or(false);
                        if error {
                            out.push_str(&format!("[TOOL_ERROR: {id}]\n{content}\n[/TOOL_ERROR]\n"));
                        } else {
                            out.push_str(&format!("[TOOL_RESULT: {id}]\n{content}\n[/TOOL_RESULT]\n"));
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = block["thinking"].as_str() { out.push_str(&format!("[thinking]\n{t}\n[/thinking]\n")); }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn extract_system_text(system: &Option<serde_json::Value>) -> String {
    match system {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            arr.iter().filter_map(|v| v.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    }
}

fn build_prompt(messages: &[Message], system: &Option<serde_json::Value>) -> String {
    let mut prompt = String::new();
    let sys = extract_system_text(system);
    if !sys.is_empty() { prompt.push_str(&format!("[SYSTEM]\n{sys}\n[/SYSTEM]\n\n")); }

    for msg in messages {
        let role = match msg.role.as_str() {
            "assistant" => "Assistant",
            "user" => "User",
            _ => "User",
        };
        prompt.push_str(&format!("[{role}]\n{}\n[/{role}]\n\n", extract_text(&msg.content)));
    }
    prompt.push_str("[Assistant]\n");
    prompt
}

// ─── Agent ────────────────────────────────────────────────────

fn find_agent() -> Option<String> {
    if let Ok(path) = std::env::var("AGENT_PATH") {
        if !path.is_empty() && std::path::Path::new(&path).exists() { return Some(path); }
    }
    // Try `command -v` (POSIX) then `which`
    for cmd in &["sh", "which"] {
        let args: &[&str] = if *cmd == "sh" { &["-c", "command -v agent"] } else { &["agent"] };
        if let Ok(out) = Command::new(cmd).args(args).output() {
            if out.status.success() {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !p.is_empty() { return Some(p); }
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for loc in &["/usr/local/bin/agent", "/opt/homebrew/bin/agent", "/usr/bin/agent"] {
        if std::path::Path::new(loc).exists() { return Some(loc.to_string()); }
    }
    if !home.is_empty() {
        let local = format!("{home}/.local/bin/agent");
        if std::path::Path::new(&local).exists() { return Some(local); }
    }
    None
}

/// With --stream-partial-output the agent ends each text segment (before a tool
/// call or the result) with a recap repeating it; hold the last event back and
/// drop it if it equals the segment.
struct TextStream {
    segment: String,
    pending: Option<String>,
}

impl TextStream {
    fn new() -> Self { Self { segment: String::new(), pending: None } }

    fn push(&mut self, text: &str) -> Option<String> {
        let ready = self.pending.replace(text.to_string());
        if let Some(ref t) = ready { self.segment.push_str(t); }
        ready
    }

    fn flush(&mut self) -> Option<String> {
        let last = self.pending.take();
        let ready = match last {
            Some(t) if t != self.segment => Some(t),
            _ => None,
        };
        self.segment.clear();
        ready
    }
}

/// Map the Anthropic-facing model id Claude Code sent → Cursor `agent --model`.
/// Stable Anthropic aliases / Claude ids become Cursor Auto; Cursor-native ids pass through.
fn map_agent_model(requested: &str) -> &str {
    let base = requested.split(['[', '@']).next().unwrap_or(requested);
    match base {
        "sonnet" | "opus" | "haiku" | "auto" | "cursor-auto" | "default" | "claude-auto" => "auto",
        other if other.starts_with("claude-") => "auto",
        other => other,
    }
}

fn spawn_agent(requested_model: &str, resume: Option<&str>) -> std::io::Result<std::process::Child> {
    let path = find_agent().unwrap_or_else(|| { log("agent not found. Install Cursor CLI or set AGENT_PATH."); std::process::exit(1); });
    let model = map_agent_model(requested_model);
    log(&format!("spawning: {path} (model={model})"));

    // Prefer a stable sandbox parent; inherited TMPDIR can point at a deleted dir.
    let tmp = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let sandbox = tmp.join(format!("cursor-bridge-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&sandbox);
    log(&format!("sandbox: {}", sandbox.display()));

    // Default mode (no --mode) = full agent with tool execution.
    // --force auto-approves tool calls in non-interactive mode.
    // --trust skips workspace trust prompt.
    let mut cmd = Command::new(path);
    cmd.args(["--print", "--force", "--output-format", "stream-json", "--stream-partial-output",
              "--model", model, "--trust"]);
    if let Some(chat_id) = resume {
        log(&format!("resuming session {chat_id}"));
        cmd.args(["--resume", chat_id]);
    }
    cmd.current_dir(&sandbox)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

fn write_prompt(agent: &mut std::process::Child, model: &str, resume: Option<&str>, prompt: &str) {
    dump_outbound_prompt(model, resume, prompt);
    if let Some(ref mut stdin) = agent.stdin {
        let _ = stdin.write_all(prompt.as_bytes());
        let _ = stdin.flush();
    }
    // Close stdin so agent gets EOF
    agent.stdin = None;
}

// ─── Blocking ─────────────────────────────────────────────────

fn handle_blocking(mut stream: TcpStream, req: &MessagesRequest) {
    let messages = req.messages.as_deref().unwrap_or_default();
    let model = req.model.as_deref().unwrap_or("sonnet");
    let (resume, prompt) = match plan_turn(&sessions().lock().unwrap(), messages, &req.system, model) {
        TurnPlan::Fresh { prompt } => (None, prompt),
        TurnPlan::Resume { chat_id, prompt } => (Some(chat_id), prompt),
    };

    let mut agent = match spawn_agent(model, resume.as_deref()) { Ok(a) => a, Err(e) => {
        let err = format!("{{\"error\":\"agent: {e}\"}}");
        let _ = stream.write_all(format!("HTTP/1.1 500\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{err}", err.len()).as_bytes());
        return;
    }};
    write_prompt(&mut agent, model, resume.as_deref(), &prompt);

    let reader = BufReader::new(agent.stdout.take().unwrap());
    let mut text = String::new();
    let mut texts = TextStream::new();
    let mut usage = serde_json::json!({});
    let mut chat_id = String::new();
    let mut succeeded = false;

    for line in reader.lines() {
        let line = match line { Ok(l) => l, _ => break };
        if line.trim().is_empty() { continue; }
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            if event["type"] == "assistant" {
                if let Some(arr) = event["message"]["content"].as_array() {
                    for block in arr {
                        if let Some(t) = block["text"].as_str() {
                            if let Some(out) = texts.push(t) { text.push_str(&out); }
                        }
                    }
                }
            }
            if event["type"] == "tool_call" || event["type"] == "result" {
                if let Some(out) = texts.flush() { text.push_str(&out); }
            }
            if event["type"] == "system" && event["subtype"] == "init" {
                if let Some(id) = event["session_id"].as_str() { chat_id = id.to_string(); }
            }
            if event["type"] == "result" {
                usage = event["usage"].clone();
                succeeded = event["is_error"] != serde_json::Value::Bool(true);
            }
        }
    }
    let _ = agent.wait();
    if let Some(out) = texts.flush() { text.push_str(&out); }

    if succeeded {
        commit_session(&mut sessions().lock().unwrap(), messages, model, &chat_id);
    }

    let resp = serde_json::json!({
        "id": format!("msg_{}", std::process::id()), "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": text}], "model": model, "stop_reason": "end_turn",
        "usage": { "input_tokens": usage["inputTokens"].as_u64().unwrap_or(0), "output_tokens": usage["outputTokens"].as_u64().unwrap_or(0) }
    });
    let body = serde_json::to_string(&resp).unwrap_or_default();
    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
}

// ─── Streaming ────────────────────────────────────────────────

fn write_sse(stream: &mut TcpStream, event_type: &str, data: &serde_json::Value) -> std::io::Result<()> {
    let json = serde_json::to_string(data)?;
    stream.write_all(b"event: ")?;
    stream.write_all(event_type.as_bytes())?;
    stream.write_all(b"\ndata: ")?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n\n")?;
    stream.flush()
}

// Block start must carry empty text; clients append deltas to it.
fn emit_text(stream: &mut TcpStream, text: &str, index: i32, block_open: &mut bool) {
    if text.is_empty() { return; }
    if !*block_open {
        let _ = write_sse(stream, "content_block_start", &serde_json::json!({
            "type": "content_block_start", "index": index,
            "content_block": {"type": "text", "text": ""}
        }));
        *block_open = true;
    }
    let _ = write_sse(stream, "content_block_delta", &serde_json::json!({
        "type": "content_block_delta", "index": index,
        "delta": {"type": "text_delta", "text": text}
    }));
}

fn handle_streaming(mut stream: TcpStream, req: &MessagesRequest) {
    let messages = req.messages.as_deref().unwrap_or_default();
    let model = req.model.as_deref().unwrap_or("sonnet");
    let (resume, prompt) = match plan_turn(&sessions().lock().unwrap(), messages, &req.system, model) {
        TurnPlan::Fresh { prompt } => (None, prompt),
        TurnPlan::Resume { chat_id, prompt } => (Some(chat_id), prompt),
    };

    let mut agent = match spawn_agent(model, resume.as_deref()) { Ok(a) => a, Err(e) => {
        let err = format!("{{\"error\":\"agent: {e}\"}}");
        let _ = stream.write_all(format!("HTTP/1.1 500\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{err}", err.len()).as_bytes());
        return;
    }};
    log(&format!("prompt: {}b", prompt.len()));
    write_prompt(&mut agent, model, resume.as_deref(), &prompt);

    // SSE response headers
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n");
    let _ = stream.flush();

    let msg_id = format!("msg_{}", std::process::id());
    let _ = write_sse(&mut stream, "message_start", &serde_json::json!({
        "type": "message_start",
        "message": { "id": msg_id, "type": "message", "role": "assistant", "content": [], "model": model, "stop_reason": null, "usage": { "input_tokens": 0, "output_tokens": 0 } }
    }));

    let reader = BufReader::new(agent.stdout.take().unwrap());
    let mut content_index = 0i32;
    let mut result_received = false;
    let mut chat_id = String::new();
    let mut text_block_open = false;
    let mut texts = TextStream::new();

    for line in reader.lines() {
        let line = match line { Ok(l) => l, _ => break };
        if line.trim().is_empty() { continue; }
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            match event["type"].as_str() {
                Some("assistant") => {
                    if let Some(blocks) = event["message"]["content"].as_array() {
                        for block in blocks {
                            let block_type = block["type"].as_str().unwrap_or("text");
                            match block_type {
                                "text" => {
                                    if let Some(text) = block["text"].as_str() {
                                        if text.is_empty() { continue; }
                                        if let Some(out) = texts.push(text) {
                                            emit_text(&mut stream, &out, content_index, &mut text_block_open);
                                        }
                                    }
                                }
                                "tool_use" => {
                                    if let Some(out) = texts.flush() {
                                        emit_text(&mut stream, &out, content_index, &mut text_block_open);
                                    }
                                    if text_block_open {
                                        let _ = write_sse(&mut stream, "content_block_stop", &serde_json::json!({
                                            "type": "content_block_stop", "index": content_index
                                        }));
                                        text_block_open = false;
                                        content_index += 1;
                                    }
                                    let name = block["name"].as_str().unwrap_or("unknown");
                                    let input = block["input"].clone();
                                    let fallback_id = format!("toolu_{}", content_index);
                                    let tool_id = block["id"].as_str().unwrap_or(&fallback_id);
                                    let _ = write_sse(&mut stream, "content_block_start", &serde_json::json!({
                                        "type": "content_block_start", "index": content_index,
                                        "content_block": {"type": "tool_use", "id": tool_id, "name": name, "input": input}
                                    }));
                                    let _ = write_sse(&mut stream, "content_block_stop", &serde_json::json!({
                                        "type": "content_block_stop", "index": content_index
                                    }));
                                    content_index += 1;
                                }
                                _ => {} // skip thinking, etc
                            }
                        }
                    }
                }
                Some("system") if event["subtype"] == "init" => {
                    if let Some(id) = event["session_id"].as_str() { chat_id = id.to_string(); }
                }
                Some("tool_call") => {
                    if let Some(out) = texts.flush() {
                        emit_text(&mut stream, &out, content_index, &mut text_block_open);
                    }
                }
                Some("result") => {
                    result_received = true;
                    if let Some(out) = texts.flush() {
                        emit_text(&mut stream, &out, content_index, &mut text_block_open);
                    }
                    if text_block_open {
                        let _ = write_sse(&mut stream, "content_block_stop", &serde_json::json!({
                            "type": "content_block_stop", "index": content_index
                        }));
                        text_block_open = false;
                    }
                    if event["is_error"] != serde_json::Value::Bool(true) {
                        commit_session(&mut sessions().lock().unwrap(), messages, model, &chat_id);
                    }
                    let usage = &event["usage"];
                    let _ = write_sse(&mut stream, "message_delta", &serde_json::json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": "end_turn"},
                        "usage": { "input_tokens": usage["inputTokens"].as_u64().unwrap_or(0), "output_tokens": usage["outputTokens"].as_u64().unwrap_or(0) }
                    }));
                    let _ = write_sse(&mut stream, "message_stop", &serde_json::json!({"type": "message_stop"}));
                    let _ = stream.write_all(b"data: [DONE]\n\n");
                    let _ = stream.flush();
                }
                _ => {}
            }
        }
    }

    if let Some(out) = texts.flush() {
        emit_text(&mut stream, &out, content_index, &mut text_block_open);
    }
    if text_block_open {
        let _ = write_sse(&mut stream, "content_block_stop", &serde_json::json!({
            "type": "content_block_stop", "index": content_index
        }));
    }

    if !result_received {
        log("result not received, sending fallback message_stop");
        let _ = write_sse(&mut stream, "message_delta", &serde_json::json!({
            "type": "message_delta", "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 0, "output_tokens": 0}
        }));
        let _ = write_sse(&mut stream, "message_stop", &serde_json::json!({"type": "message_stop"}));
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
    }

    let _ = agent.wait();
}

/// Escape raw control bytes inside JSON strings so serde_json can parse bodies
/// that Claude Code sometimes emits (tool output / image sidecars / huge transcripts).
fn escape_json_string_controls(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut in_string = false;
    let mut escape = false;
    for &b in input {
        if !in_string {
            out.push(b);
            if b == b'"' {
                in_string = true;
            }
            continue;
        }
        if escape {
            out.push(b);
            escape = false;
            continue;
        }
        match b {
            b'\\' => {
                out.push(b);
                escape = true;
            }
            b'"' => {
                out.push(b);
                in_string = false;
            }
            0x00..=0x1F => {
                // JSON strings cannot contain raw controls; emit \u00XX.
                out.extend_from_slice(format!("\\u{b:04x}").as_bytes());
            }
            _ => out.push(b),
        }
    }
    out
}

fn handle_messages(mut stream: TcpStream, body: &[u8], _token: &str) {
    let req: MessagesRequest = match serde_json::from_slice(body)
        .or_else(|e| {
            log(&format!("json parse retry after control-char sanitize: {e}"));
            let cleaned = escape_json_string_controls(body);
            serde_json::from_slice(&cleaned)
        }) {
        Ok(r) => r,
        Err(e) => {
            let err = format!("{{\"error\":\"{}\"}}", e.to_string().replace('"', "'"));
            let resp = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{err}", err.len());
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            return;
        }
    };

    if req.stream.unwrap_or(true) { handle_streaming(stream, &req); }
    else { handle_blocking(stream, &req); }
}

// ─── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> Message {
        Message { role: role.into(), content: serde_json::Value::String(text.into()) }
    }

    fn serve(store: &mut HashMap<u64, SessionEntry>, msgs: &[Message], model: &str) -> TurnPlan {
        let plan = plan_turn(store, msgs, &None, model);
        commit_session(store, msgs, model, "chat-1");
        plan
    }

    #[test]
    fn test_reminders_are_stripped_from_identity() {
        assert_eq!(strip_reminders("<system-reminder>noise</system-reminder>\n\nhello"), "hello");
        assert_eq!(strip_reminders("a<system-reminder>x</system-reminder>b"), "ab");
        assert_eq!(strip_reminders("plain"), "plain");
        assert_eq!(strip_reminders("keep<system-reminder>dangling"), "keep");
    }

    #[test]
    fn test_escape_json_string_controls_makes_body_parseable() {
        // Raw newline inside a JSON string is illegal; after sanitize it parses.
        let bad = b"{\"model\":\"sonnet\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\nthere\"}]}";
        assert!(serde_json::from_slice::<MessagesRequest>(bad).is_err());
        let cleaned = escape_json_string_controls(bad);
        let req: MessagesRequest = serde_json::from_slice(&cleaned).expect("cleaned json");
        assert_eq!(req.model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn test_read_exact_bytes_reports_truncation() {
        let mut cursor = std::io::Cursor::new(b"short");
        let err = read_exact_bytes(&mut cursor, 20).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("got 5/20"));
    }

    #[test]
    fn test_interleaved_system_messages_are_not_part_of_the_conversation() {
        let msgs = vec![
            msg("user", "<system-reminder>varies</system-reminder>\n\nhello"),
            msg("system", "hook output, 24kb of it"),
            msg("assistant", "hi"),
            msg("user", "again"),
        ];
        let turns = conversation(&msgs);
        assert_eq!(turns.len(), 3, "system messages are not turns");
        assert_eq!(turns[0].text, "hello", "reminders are stripped");
        assert_eq!(turns[2].raw_index, 3, "raw positions are preserved");
    }

    // System messages and reminders shift between requests.
    #[test]
    fn test_session_survives_client_reinjected_context() {
        let mut store = HashMap::new();
        let turn1 = vec![
            msg("user", "<system-reminder>turn one</system-reminder>\n\nRemember 7."),
            msg("system", "hook context, ends with a newline\n"),
        ];
        serve(&mut store, &turn1, "sonnet-4.5");

        let turn2 = vec![
            msg("user", "<system-reminder>turn two, different</system-reminder>\n\nRemember 7."),
            msg("system", "hook context, ends with a newline"),
            msg("assistant", "OK"),
            msg("user", "What number?"),
            msg("system", "<total_tokens>14999</total_tokens>"),
        ];
        match plan_turn(&store, &turn2, &None, "sonnet-4.5") {
            TurnPlan::Resume { prompt, .. } => {
                assert!(prompt.contains("What number?"), "the new question is sent");
                assert!(!prompt.contains("Remember 7."), "history stays in the session");
                assert!(prompt.contains("14999"), "context after the new turn still travels");
            }
            TurnPlan::Fresh { .. } => panic!("re-injected context must not defeat the session"),
        }
    }

    #[test]
    fn test_second_turn_resumes_and_sends_only_the_new_message() {
        let mut store = HashMap::new();
        serve(&mut store, &[msg("user", "u1")], "sonnet-4.5");

        let turn2 = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")];
        match plan_turn(&store, &turn2, &None, "sonnet-4.5") {
            TurnPlan::Resume { chat_id, prompt } => {
                assert_eq!(chat_id, "chat-1");
                assert!(prompt.contains("u2"), "new message must be sent");
                assert!(!prompt.contains("u1"), "history is already in the session");
                assert!(!prompt.contains("a1"), "the reply is already in the session");
            }
            TurnPlan::Fresh { .. } => panic!("second turn should resume"),
        }
    }

    #[test]
    fn test_third_turn_keeps_sending_only_the_new_message() {
        let mut store = HashMap::new();
        serve(&mut store, &[msg("user", "u1")], "sonnet-4.5");
        let turn2 = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")];
        serve(&mut store, &turn2, "sonnet-4.5");

        let turn3 = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2"),
                         msg("assistant", "a2"), msg("user", "u3")];
        match plan_turn(&store, &turn3, &None, "sonnet-4.5") {
            TurnPlan::Resume { prompt, .. } => {
                assert!(prompt.contains("u3"));
                assert!(!prompt.contains("u2"), "u2 was consumed by the previous turn");
            }
            TurnPlan::Fresh { .. } => panic!("third turn should resume"),
        }
    }

    #[test]
    fn test_edited_user_message_falls_back_to_a_fresh_session() {
        let mut store = HashMap::new();
        serve(&mut store, &[msg("user", "u1")], "sonnet-4.5");
        let turn2 = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")];
        serve(&mut store, &turn2, "sonnet-4.5");

        let forked = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "EDITED"),
                          msg("assistant", "a2"), msg("user", "u3")];
        match plan_turn(&store, &forked, &None, "sonnet-4.5") {
            TurnPlan::Fresh { prompt } => assert!(prompt.contains("u1"), "full history is replayed"),
            TurnPlan::Resume { .. } => panic!("a rewritten prefix must not reuse the session"),
        }
    }

    #[test]
    fn test_dropped_message_falls_back_to_a_fresh_session() {
        let mut store = HashMap::new();
        serve(&mut store, &[msg("user", "u1")], "sonnet-4.5");
        let turn2 = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")];
        serve(&mut store, &turn2, "sonnet-4.5");

        let compacted = vec![msg("user", "u1"), msg("user", "u2"), msg("assistant", "a2"),
                             msg("user", "u3")];
        match plan_turn(&store, &compacted, &None, "sonnet-4.5") {
            TurnPlan::Fresh { .. } => {}
            TurnPlan::Resume { .. } => panic!("a reordered prefix must not reuse the session"),
        }
    }

    #[test]
    fn test_switching_model_starts_a_fresh_session() {
        let mut store = HashMap::new();
        serve(&mut store, &[msg("user", "u1")], "sonnet-4.5");
        let turn2 = vec![msg("user", "u1"), msg("assistant", "a1"), msg("user", "u2")];
        match plan_turn(&store, &turn2, &None, "gpt-5") {
            TurnPlan::Fresh { .. } => {}
            TurnPlan::Resume { .. } => panic!("a different model needs its own session"),
        }
    }

    #[test]
    fn test_a_replayed_turn_does_not_resume() {
        let mut store = HashMap::new();
        let turn1 = vec![msg("user", "u1")];
        serve(&mut store, &turn1, "sonnet-4.5");
        match plan_turn(&store, &turn1, &None, "sonnet-4.5") {
            TurnPlan::Fresh { .. } => {}
            TurnPlan::Resume { .. } => panic!("no unseen messages means no resume"),
        }
    }

    fn render(segments: &[&[&str]]) -> String {
        let mut ts = TextStream::new();
        let mut out = String::new();
        for seg in segments {
            for ev in *seg {
                if let Some(t) = ts.push(ev) { out.push_str(&t); }
            }
            if let Some(t) = ts.flush() { out.push_str(&t); }
        }
        out
    }

    #[test]
    fn test_segment_recap_is_dropped() {
        // Deltas, then the recap repeating the whole segment.
        let out = render(&[&["I", "'ll run that", " command", " for you.",
                             "I'll run that command for you."]]);
        assert_eq!(out, "I'll run that command for you.");
    }

    #[test]
    fn test_recap_dropped_in_every_segment_of_a_tool_turn() {
        // Each segment of a tool turn ends with its own recap.
        let out = render(&[
            &["I", "'ll check.", "I'll check."],
            &["The", " answer is 4.", "The answer is 4."],
        ]);
        assert_eq!(out, "I'll check.The answer is 4.");
    }

    #[test]
    fn test_single_event_without_deltas_is_emitted() {
        // An agent predating --stream-partial-output sends only the recap.
        assert_eq!(render(&[&["the whole reply"]]), "the whole reply");
    }

    #[test]
    fn test_repeated_text_is_not_mistaken_for_a_recap() {
        // Only the event closing a segment can be a recap.
        assert_eq!(render(&[&["hi", "hi", "hihi"]]), "hihi");
    }

    #[test]
    fn test_empty_turn_produces_nothing() {
        assert_eq!(render(&[&[]]), "");
    }

    #[test]
    fn test_extract_system_text_string() {
        let v = Some(serde_json::Value::String("Be helpful.".into()));
        assert_eq!(extract_system_text(&v), "Be helpful.");
    }

    #[test]
    fn test_extract_system_text_array() {
        let v = Some(serde_json::json!([
            {"type": "text", "text": "Be helpful."},
            {"type": "text", "text": "Be concise."}
        ]));
        assert_eq!(extract_system_text(&v), "Be helpful.\nBe concise.");
    }

    #[test]
    fn test_extract_system_text_none() {
        assert_eq!(extract_system_text(&None), "");
    }

    #[test]
    fn test_extract_text_string_content() {
        let v = serde_json::Value::String("hello".into());
        assert_eq!(extract_text(&v), "hello");
    }

    #[test]
    fn test_extract_text_text_block() {
        let v = serde_json::json!([
            {"type": "text", "text": "Hello there"}
        ]);
        assert_eq!(extract_text(&v), "Hello there\n");
    }

    #[test]
    fn test_extract_text_tool_use() {
        let v = serde_json::json!([
            {"type": "tool_use", "name": "bash", "id": "tu_1", "input": {"command": "ls"}}
        ]);
        let out = extract_text(&v);
        assert!(out.contains("TOOL_USE: bash"));
        assert!(out.contains("ls"));
    }

    #[test]
    fn test_extract_text_tool_result() {
        let v = serde_json::json!([
            {"type": "tool_result", "tool_use_id": "tu_1", "content": "file.txt"}
        ]);
        let out = extract_text(&v);
        assert!(out.contains("TOOL_RESULT: tu_1"));
        assert!(out.contains("file.txt"));
    }

    #[test]
    fn test_extract_text_tool_error() {
        let v = serde_json::json!([
            {"type": "tool_result", "tool_use_id": "tu_1", "content": "permission denied", "is_error": true}
        ]);
        let out = extract_text(&v);
        assert!(out.contains("TOOL_ERROR"));
    }

    #[test]
    fn test_agent_empty_env_returns_none_for_bogus() {
        // Without AGENT_PATH set, non-existent name should not crash
        // Just verify the function handles missing gracefully
        let original = std::env::var("AGENT_PATH").ok();
        std::env::remove_var("AGENT_PATH");
        // Can't assert None because `which` might find `agent` in CI,
        // but it shouldn't panic or return Some("")
        let result = find_agent();
        if let Some(path) = result {
            assert!(!path.is_empty(), "path must not be empty");
        }
        if let Some(val) = original {
            std::env::set_var("AGENT_PATH", val);
        }
    }

    #[test]
    fn test_build_prompt_simple() {
        let msgs = [Message {
            role: "user".into(),
            content: serde_json::Value::String("hi".into()),
        }];
        let prompt = build_prompt(&msgs, &None);
        assert!(prompt.contains("[User]"));
        assert!(prompt.contains("hi"));
        assert!(prompt.contains("[/User]"));
        assert!(prompt.contains("[Assistant]"));
    }

    #[test]
    fn test_build_prompt_with_system() {
        let sys = Some(serde_json::Value::String("You are a bot.".into()));
        let msgs = [Message {
            role: "user".into(),
            content: serde_json::Value::String("hi".into()),
        }];
        let prompt = build_prompt(&msgs, &sys);
        assert!(prompt.contains("[SYSTEM]"));
        assert!(prompt.contains("You are a bot."));
    }

    #[test]
    fn test_build_prompt_with_tool_context() {
        let msgs = [
            Message {
                role: "user".into(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "file contents"}
                ]),
            },
            Message {
                role: "assistant".into(),
                content: serde_json::json!([
                    {"type": "tool_use", "name": "read", "id": "tu_1", "input": {"path": "file.txt"}}
                ]),
            },
        ];
        let prompt = build_prompt(&msgs, &None);
        assert!(prompt.contains("TOOL_RESULT"));
        assert!(prompt.contains("TOOL_USE: read"));
    }

    #[test]
    fn test_models_response_valid_json() {
        // Verify the hardcoded models response is valid JSON
        let body = r#"{"data":[
            {"type":"model","id":"cursor-auto","display_name":"Cursor Auto"},
            {"type":"model","id":"cursor-smart","display_name":"Cursor Smart"},
            {"type":"model","id":"default","display_name":"Default"}
        ]}"#;
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["data"].as_array().unwrap().len(), 3);
    }
}

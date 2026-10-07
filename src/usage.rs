//! Local usage metadata only. Never persist prompts, completions or API keys.
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn database_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("无法确定统计目录")?)
            .join("Agent-Switch")
            .join("usage.sqlite3"),
    )
}

/// Stable per URL + Key identifier. The plaintext key is never written to the database.
pub fn configuration_id(gateway_url: &str, api_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"Agent-Switch-usage-v1\0");
    let normalized = url::Url::parse(gateway_url.trim())
        .map(|mut url| {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|_| gateway_url.trim().to_owned());
    digest.update(normalized.trim_end_matches('/').as_bytes());
    digest.update([0]);
    digest.update(api_key.trim().as_bytes());
    format!("{:x}", digest.finalize())
}

fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS configurations (
             id TEXT PRIMARY KEY
         );
         CREATE TABLE IF NOT EXISTS requests (
             id TEXT PRIMARY KEY,
             configuration_id TEXT NOT NULL REFERENCES configurations(id),
             started_ms INTEGER NOT NULL,
             outcome TEXT NOT NULL DEFAULT 'pending',
             input_tokens INTEGER, output_tokens INTEGER,
             cache_read INTEGER, cache_write INTEGER, cache_input INTEGER
         );
         CREATE INDEX IF NOT EXISTS requests_configuration ON requests(configuration_id);",
    )?;
    // The first releases only stored aggregate counters. Keep the database
    // backward compatible and add the per-request fields lazily for existing
    // installations.
    for (name, definition) in [
        ("client", "TEXT NOT NULL DEFAULT ''"),
        ("model", "TEXT"),
        ("request_kind", "TEXT NOT NULL DEFAULT 'inference'"),
        ("status_code", "INTEGER"),
        ("endpoint", "TEXT NOT NULL DEFAULT ''"),
        ("conversion", "TEXT"),
        ("streaming", "INTEGER NOT NULL DEFAULT 0"),
        ("total_ms", "INTEGER"),
        ("first_byte_ms", "INTEGER"),
    ] {
        let present = connection
            .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name=?1")?
            .query_row([name], |_| Ok(()))
            .is_ok();
        if !present {
            connection.execute(
                &format!("ALTER TABLE requests ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    connection.pragma_update(None, "user_version", 2i64)?;
    Ok(connection)
}

#[derive(Clone, Debug)]
pub struct Recorder(Sender<Event>);

enum Event {
    Start {
        id: String,
        configuration: String,
        metadata: RequestMetadata,
    },
    Finish {
        id: String,
        outcome: String,
        status_code: Option<u16>,
        total_ms: Option<u64>,
        first_byte_ms: Option<u64>,
        usage: Usage,
    },
    Flush(Sender<()>),
}

impl Recorder {
    pub fn flush(&self) {
        let (sender, receiver) = mpsc::channel();
        if self.0.send(Event::Flush(sender)).is_ok() {
            let _ = receiver.recv_timeout(Duration::from_secs(5));
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let connection = connect(path)?;
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("usage-sqlite".into())
            .spawn(move || {
                for event in receiver {
                    if let Err(error) = write_event(&connection, event) {
                        tracing::warn!(
                            "{}: {}",
                            crate::i18n::tr("无法写入用量统计", "Unable to write usage statistics"),
                            crate::i18n::runtime_error(&error)
                        );
                    }
                }
            })?;
        Ok(Self(sender))
    }

    pub fn start(&self, configuration: String, protocol: Protocol) -> RequestUsage {
        self.start_with_metadata(configuration, protocol, RequestMetadata::default())
    }

    pub fn start_with_metadata(
        &self,
        configuration: String,
        protocol: Protocol,
        metadata: RequestMetadata,
    ) -> RequestUsage {
        let id = format!("{:032x}", rand::random::<u128>());
        let _ = self.0.send(Event::Start {
            id: id.clone(),
            configuration,
            metadata: metadata.clone(),
        });
        RequestUsage {
            recorder: self.clone(),
            id: Some(id),
            parser: UsageParser::new(protocol),
            http_success: false,
            started: Instant::now(),
            first_byte: None,
            status_code: None,
        }
    }
}

fn write_event(connection: &Connection, event: Event) -> Result<()> {
    match event {
        Event::Flush(sender) => {
            let _ = sender.send(());
        }
        Event::Start {
            id,
            configuration,
            metadata,
        } => {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute(
                "INSERT OR IGNORE INTO configurations(id) VALUES (?1)",
                [&configuration],
            )?;
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(i64::MAX as u128) as i64;
            transaction.execute(
                "INSERT INTO requests(id,configuration_id,started_ms,client,model,request_kind,endpoint,conversion,streaming) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    id,
                    configuration,
                    time,
                    metadata.client,
                    metadata.model,
                    metadata.request_kind,
                    metadata.endpoint,
                    metadata.conversion,
                    metadata.streaming as i64,
                ],
            )?;
            transaction.commit()?;
        }
        Event::Finish {
            id,
            outcome,
            status_code,
            total_ms,
            first_byte_ms,
            usage,
        } => {
            connection.execute(
                "UPDATE requests SET outcome=?2,status_code=?3,total_ms=?4,first_byte_ms=?5,input_tokens=?6,output_tokens=?7,cache_read=?8,cache_write=?9,cache_input=?10 WHERE id=?1 AND outcome='pending'",
                params![
                    id,
                    outcome,
                    status_code.map(i64::from),
                    total_ms.map(|v| v.min(i64::MAX as u64) as i64),
                    first_byte_ms.map(|v| v.min(i64::MAX as u64) as i64),
                    usage.input,
                    usage.output,
                    usage.cache_read,
                    usage.cache_write,
                    usage.cache_input,
                ],
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub total: i64,
    pub success: i64,
    pub failed: i64,
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub ratio_read: Option<i64>,
    pub ratio_input: Option<i64>,
    pub input_known: i64,
    pub output_known: i64,
    pub read_known: i64,
    pub write_known: i64,
}

#[cfg(test)]
pub fn load_summaries(path: &Path) -> Result<HashMap<String, Summary>> {
    let connection = connect(path)?;
    read_summaries(&connection)
}

fn read_summaries(connection: &Connection) -> Result<HashMap<String, Summary>> {
    let mut query = connection.prepare(
        "SELECT configuration_id,COUNT(*),SUM(outcome='success'),SUM(outcome IN ('failed','cancelled')),
         SUM(input_tokens),SUM(output_tokens),SUM(cache_read),SUM(cache_write),
         SUM(CASE WHEN cache_input IS NOT NULL THEN cache_read END),SUM(cache_input),
         COUNT(input_tokens),COUNT(output_tokens),COUNT(cache_read),COUNT(cache_write)
         FROM requests GROUP BY configuration_id",
    )?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get(0)?,
            Summary {
                total: row.get(1)?,
                success: row.get(2)?,
                failed: row.get(3)?,
                input: row.get(4)?,
                output: row.get(5)?,
                cache_read: row.get(6)?,
                cache_write: row.get(7)?,
                ratio_read: row.get(8)?,
                ratio_input: row.get(9)?,
                input_known: row.get(10)?,
                output_known: row.get(11)?,
                read_known: row.get(12)?,
                write_known: row.get(13)?,
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

/// SQLite reads run outside the GUI thread, with at most one queued refresh.
pub struct SummaryReader {
    request: mpsc::SyncSender<()>,
    result: mpsc::Receiver<Result<HashMap<String, Summary>>>,
    pending: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RequestMetadata {
    pub client: String,
    pub model: Option<String>,
    pub request_kind: String,
    pub endpoint: String,
    pub conversion: Option<String>,
    pub streaming: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ModelLogEntry {
    pub started_ms: i64,
    pub configuration_id: String,
    pub client: String,
    pub model: Option<String>,
    pub outcome: String,
    pub status_code: Option<i64>,
    pub request_kind: String,
    pub input: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub output: Option<i64>,
    pub total_ms: Option<i64>,
    pub first_byte_ms: Option<i64>,
    pub endpoint: String,
    pub conversion: Option<String>,
    pub streaming: bool,
}

fn read_model_logs(connection: &Connection, limit: usize) -> Result<Vec<ModelLogEntry>> {
    let mut query = connection.prepare(
        "SELECT started_ms,configuration_id,client,model,outcome,status_code,request_kind,
                input_tokens,cache_read,cache_write,output_tokens,total_ms,first_byte_ms,
                endpoint,conversion,streaming
         FROM requests
         WHERE client <> ''
           AND request_kind <> 'probe'
           AND NOT (client = 'Codex Desktop' AND model IS NULL
                    AND endpoint = '/responses' AND status_code IN (404,405))
         ORDER BY started_ms DESC LIMIT ?1",
    )?;
    let rows = query.query_map([limit as i64], |row| {
        Ok(ModelLogEntry {
            started_ms: row.get(0)?,
            configuration_id: row.get(1)?,
            client: row.get(2)?,
            model: row.get(3)?,
            outcome: row.get(4)?,
            status_code: row.get(5)?,
            request_kind: row.get(6)?,
            input: row.get(7)?,
            cache_read: row.get(8)?,
            cache_write: row.get(9)?,
            output: row.get(10)?,
            total_ms: row.get(11)?,
            first_byte_ms: row.get(12)?,
            endpoint: row.get(13)?,
            conversion: row.get(14)?,
            streaming: row.get::<_, i64>(15)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub struct ModelLogReader {
    request: mpsc::SyncSender<usize>,
    result: mpsc::Receiver<Result<Vec<ModelLogEntry>>>,
    pending: bool,
}

impl ModelLogReader {
    pub fn new() -> Self {
        let (request, requests) = mpsc::sync_channel(1);
        let (results, result) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("model-log-reader".into())
            .spawn(move || {
                let mut connection = None;
                while let Ok(limit) = requests.recv() {
                    let logs = (|| {
                        if connection.is_none() {
                            connection = Some(connect(&database_path()?)?);
                        }
                        read_model_logs(connection.as_ref().expect("opened connection"), limit)
                    })();
                    if logs.is_err() {
                        connection = None;
                    }
                    if results.send(logs).is_err() {
                        break;
                    }
                }
            });
        Self {
            request,
            result,
            pending: false,
        }
    }

    pub fn refresh(&mut self, limit: usize) {
        if !self.pending && self.request.try_send(limit).is_ok() {
            self.pending = true;
        }
    }

    pub fn poll(&mut self) -> Option<Result<Vec<ModelLogEntry>>> {
        match self.result.try_recv() {
            Ok(value) => {
                self.pending = false;
                Some(value)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = false;
                Some(Err(anyhow::anyhow!("模型日志读取线程不可用")))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

impl SummaryReader {
    pub fn new() -> Self {
        let (request, requests) = mpsc::sync_channel(1);
        let (results, result) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("usage-reader".into())
            .spawn(move || {
                let mut connection = None;
                while requests.recv().is_ok() {
                    let summary = (|| {
                        if connection.is_none() {
                            connection = Some(connect(&database_path()?)?);
                        }
                        read_summaries(connection.as_ref().expect("opened connection"))
                    })();
                    if summary.is_err() {
                        connection = None;
                    }
                    if results.send(summary).is_err() {
                        break;
                    }
                }
            });
        Self {
            request,
            result,
            pending: false,
        }
    }

    pub fn refresh(&mut self) {
        if !self.pending && self.request.try_send(()).is_ok() {
            self.pending = true;
        }
    }

    pub fn poll(&mut self) -> Option<Result<HashMap<String, Summary>>> {
        match self.result.try_recv() {
            Ok(value) => {
                self.pending = false;
                Some(value)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = false;
                Some(Err(anyhow::anyhow!("统计读取线程不可用")))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

pub fn format_tokens(value: Option<i64>) -> String {
    let Some(value) = value else {
        return "-".into();
    };
    if value <= 1000 {
        return value.to_string();
    }
    let (divisor, suffix) = if value >= 1_000_000 {
        (1_000_000.0, "M")
    } else {
        (1000.0, "K")
    };
    let number = format!("{:.1}", value as f64 / divisor);
    format!(
        "{}{suffix}",
        number.trim_end_matches('0').trim_end_matches('.')
    )
}

impl Summary {
    pub fn total_token_text(&self) -> String {
        if self.input.is_none() && self.output.is_none() {
            return "-".into();
        }
        let total = self
            .input
            .unwrap_or(0)
            .saturating_add(self.output.unwrap_or(0));
        self.token_text(Some(total), self.input_known.min(self.output_known))
    }

    pub fn token_text(&self, value: Option<i64>, known: i64) -> String {
        let _ = known;
        format_tokens(value)
    }
    pub fn hit_ratio(&self) -> String {
        match (self.ratio_read, self.ratio_input) {
            (Some(read), Some(input)) if input > 0 => {
                let number = format!("{:.1}", read as f64 * 100.0 / input as f64);
                format!("{}%", number.trim_end_matches('0').trim_end_matches('.'))
            }
            _ => "-".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Responses,
    Chat,
    Messages,
    Gemini,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Responses => "Responses",
            Self::Chat => "Chat Completions",
            Self::Messages => "Messages",
            Self::Gemini => "Gemini",
        }
    }
}

pub fn inference_protocol(method: &http::Method, path: &str) -> Option<Protocol> {
    if method != http::Method::POST {
        return None;
    }
    match path.trim_end_matches('/') {
        "/responses" | "/v1/responses" => Some(Protocol::Responses),
        "/v1/chat/completions" => Some(Protocol::Chat),
        "/messages" | "/v1/messages" => Some(Protocol::Messages),
        path if path.ends_with("generateContent") || path.ends_with("streamGenerateContent") => {
            Some(Protocol::Gemini)
        }
        _ => None,
    }
}

pub fn protocol_for_wire(protocol: crate::protocol::WireProtocol) -> Protocol {
    match protocol {
        crate::protocol::WireProtocol::Responses => Protocol::Responses,
        crate::protocol::WireProtocol::ChatCompletions => Protocol::Chat,
        crate::protocol::WireProtocol::Messages => Protocol::Messages,
        crate::protocol::WireProtocol::Gemini => Protocol::Gemini,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    input: Option<i64>,
    output: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    cache_input: Option<i64>,
}

/// Incremental, bounded observer. All original response bytes are forwarded unchanged.
pub struct UsageParser {
    protocol: Protocol,
    sse: bool,
    buffer: Vec<u8>,
    event: Vec<u8>,
    dropping_line: bool,
    dropping_event: bool,
    pub usage: Usage,
    terminal: bool,
    error: bool,
}

const MAX_CAPTURE: usize = 2 * 1024 * 1024;

impl UsageParser {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            sse: false,
            buffer: Vec::new(),
            event: Vec::new(),
            dropping_line: false,
            dropping_event: false,
            usage: Usage::default(),
            terminal: false,
            error: false,
        }
    }
    pub fn set_content_type(&mut self, content_type: &str) {
        self.sse = content_type
            .split(';')
            .next()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"));
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        if !self.sse {
            if !self.dropping_line && self.buffer.len() + bytes.len() <= MAX_CAPTURE {
                self.buffer.extend_from_slice(bytes);
            } else {
                self.buffer.clear();
                self.dropping_line = true;
            }
            return;
        }
        for &byte in bytes {
            if byte == b'\n' {
                if self.dropping_line {
                    self.dropping_line = false;
                    self.dropping_event = true;
                    self.buffer.clear();
                } else {
                    let mut line = std::mem::take(&mut self.buffer);
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    self.line(&line);
                }
            } else if !self.dropping_line {
                if self.buffer.len() < MAX_CAPTURE {
                    self.buffer.push(byte);
                } else {
                    self.buffer.clear();
                    self.dropping_line = true;
                }
            }
        }
    }
    fn line(&mut self, line: &[u8]) {
        if line.is_empty() {
            if !self.dropping_event {
                let event = std::mem::take(&mut self.event);
                if event == b"[DONE]" && self.protocol == Protocol::Chat {
                    self.terminal = true;
                } else if let Ok(value) = serde_json::from_slice::<Value>(&event) {
                    self.observe(&value);
                }
            }
            self.event.clear();
            self.dropping_event = false;
        } else if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if self.event.len() + data.len() < MAX_CAPTURE {
                if !self.event.is_empty() {
                    self.event.push(b'\n');
                }
                self.event.extend_from_slice(data);
            } else {
                self.event.clear();
                self.dropping_event = true;
            }
        } else if line == b"event: error" {
            self.error = true;
        }
    }
    fn observe(&mut self, value: &Value) {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if value.get("error").is_some_and(|v| !v.is_null())
            || matches!(kind, "error" | "response.failed" | "response.incomplete")
            || value
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| matches!(s, "failed" | "incomplete" | "cancelled"))
        {
            self.error = true;
        }
        if matches!(kind, "response.completed" | "message_stop")
            || (self.protocol == Protocol::Gemini
                && (value.pointer("/candidates/0/finishReason").is_some()
                    || value.get("usageMetadata").is_some()))
        {
            self.terminal = true;
        }
        let usage = value
            .get("usage")
            .or_else(|| value.get("usageMetadata"))
            .or_else(|| value.pointer("/response/usage"))
            .or_else(|| value.pointer("/message/usage"));
        let Some(usage) = usage.filter(|v| v.is_object()) else {
            return;
        };
        let number = |name: &str| {
            usage
                .pointer(name)
                .and_then(Value::as_i64)
                .filter(|v| *v >= 0)
        };
        // Streaming usage values are cumulative snapshots, never deltas to add.
        if let Some(v) = number("/input_tokens")
            .or_else(|| number("/prompt_tokens"))
            .or_else(|| number("/promptTokenCount"))
        {
            self.usage.input = Some(v);
        }
        if let Some(v) = number("/output_tokens")
            .or_else(|| number("/completion_tokens"))
            .or_else(|| number("/candidatesTokenCount"))
        {
            self.usage.output = Some(v);
        }
        if let Some(v) = number("/cache_read_input_tokens")
            .or_else(|| number("/input_tokens_details/cached_tokens"))
            .or_else(|| number("/prompt_tokens_details/cached_tokens"))
            .or_else(|| number("/cachedContentTokenCount"))
        {
            self.usage.cache_read = Some(v);
        }
        if let Some(v) = number("/cache_creation_input_tokens") {
            self.usage.cache_write = Some(v);
        }
    }
    pub fn finish(&mut self) -> bool {
        if self.sse {
            // Require a dispatched completion marker, not simply a clean TCP EOF.
            if !self.buffer.is_empty() || !self.event.is_empty() {
                self.error = true;
            }
        } else if !self.dropping_line {
            match serde_json::from_slice::<Value>(&self.buffer) {
                Ok(value) => self.observe(&value),
                Err(_) => self.error = true,
            }
        }
        self.normalize();
        !self.error && (!self.sse || self.terminal)
    }
    fn normalize(&mut self) {
        if self.protocol == Protocol::Messages {
            // Anthropic input_tokens excludes cache reads and writes.
            // If a cache field is omitted, the complete input total is unknown.
            self.usage.input = self
                .usage
                .input
                .zip(self.usage.cache_read)
                .zip(self.usage.cache_write)
                .and_then(|((input, read), write)| input.checked_add(read)?.checked_add(write));
        }
        self.usage.cache_input = self
            .usage
            .input
            .zip(self.usage.cache_read)
            .filter(|(input, read)| read <= input)
            .map(|(input, _)| input);
    }
}

pub struct RequestUsage {
    recorder: Recorder,
    id: Option<String>,
    pub parser: UsageParser,
    pub http_success: bool,
    started: Instant,
    first_byte: Option<Instant>,
    status_code: Option<u16>,
}

impl RequestUsage {
    pub fn set_status_code(&mut self, status_code: u16) {
        self.status_code = Some(status_code);
        self.http_success = (200..300).contains(&status_code);
    }

    pub fn mark_first_byte(&mut self) {
        if self.first_byte.is_none() {
            self.first_byte = Some(Instant::now());
        }
    }

    pub fn finish(&mut self, complete: bool) {
        if let Some(id) = self.id.take() {
            let valid = self.parser.finish();
            let outcome = if complete && self.http_success && valid {
                "success"
            } else if !complete
                && self
                    .status_code
                    .is_some_and(|code| (200..300).contains(&code))
            {
                "cancelled"
            } else {
                "failed"
            };
            let _ = self.recorder.0.send(Event::Finish {
                id,
                outcome: outcome.to_owned(),
                status_code: self.status_code,
                total_ms: Some(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64),
                first_byte_ms: self.first_byte.map(|time| {
                    time.duration_since(self.started)
                        .as_millis()
                        .min(u64::MAX as u128) as u64
                }),
                usage: self.parser.usage,
            });
        }
    }
}

impl Drop for RequestUsage {
    fn drop(&mut self) {
        self.finish(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parsed(protocol: Protocol, value: Value) -> Usage {
        let mut parser = UsageParser::new(protocol);
        parser.feed(&serde_json::to_vec(&value).unwrap());
        assert!(parser.finish());
        parser.usage
    }

    #[test]
    fn identifiers_normalize_urls_but_separate_keys_and_paths() {
        let id = configuration_id(" https://API.example.com:443/v1/ ", " sk-private ");
        assert_eq!(
            id,
            configuration_id("https://api.example.com/v1", "sk-private")
        );
        assert_ne!(
            id,
            configuration_id("https://api.example.com/v1", "another-key")
        );
        assert_ne!(
            id,
            configuration_id("https://api.example.com", "sk-private")
        );
        assert_eq!(id.len(), 64);
        assert!(!id.contains("sk-private"));
    }

    #[test]
    fn only_model_inference_counts() {
        for path in [
            "/responses",
            "/v1/responses",
            "/messages",
            "/v1/messages",
            "/v1/chat/completions",
            "/v1beta/models/gemini-2.5-pro:generateContent",
        ] {
            assert!(inference_protocol(&http::Method::POST, path).is_some());
            assert!(inference_protocol(&http::Method::GET, path).is_none());
        }
        for path in ["/v1/models", "/health", "/admin/update", "/v1/responses/id"] {
            assert!(inference_protocol(&http::Method::POST, path).is_none());
        }
    }

    #[test]
    fn json_usage_and_missing_zero_are_distinct() {
        let responses = parsed(
            Protocol::Responses,
            json!({"usage":{
                "input_tokens":1200,"output_tokens":40,"input_tokens_details":{"cached_tokens":1000}
            }}),
        );
        assert_eq!(responses.input, Some(1200));
        assert_eq!(responses.cache_read, Some(1000));
        assert_eq!(responses.cache_input, Some(1200));
        assert_eq!(responses.cache_write, None);
        let chat = parsed(
            Protocol::Chat,
            json!({"usage":{
                "prompt_tokens":100,"completion_tokens":0,"prompt_tokens_details":{"cached_tokens":0}
            }}),
        );
        assert_eq!(chat.output, Some(0));
        assert_eq!(chat.cache_read, Some(0));
        let missing = parsed(Protocol::Chat, json!({"choices":[]}));
        assert_eq!(missing, Usage::default());

        let gemini = parsed(
            Protocol::Gemini,
            json!({"usageMetadata":{
                "promptTokenCount":1200,"candidatesTokenCount":40,"cachedContentTokenCount":1000
            }}),
        );
        assert_eq!(gemini.input, Some(1200));
        assert_eq!(gemini.output, Some(40));
        assert_eq!(gemini.cache_read, Some(1000));
    }

    #[test]
    fn messages_sse_chunk_boundaries_and_cumulative_usage() {
        let wire = concat!(
            "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":1,\"cache_read_input_tokens\":800,\"cache_creation_input_tokens\":100}}}\r\n\r\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":10}}\n\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        for chunk in [1, 2, 17, wire.len()] {
            let mut parser = UsageParser::new(Protocol::Messages);
            parser.set_content_type("text/event-stream; charset=utf-8");
            for bytes in wire.as_bytes().chunks(chunk) {
                parser.feed(bytes);
            }
            assert!(parser.finish());
            assert_eq!(
                parser.usage,
                Usage {
                    input: Some(1000),
                    output: Some(20),
                    cache_read: Some(800),
                    cache_write: Some(100),
                    cache_input: Some(1000)
                }
            );
        }
    }

    #[test]
    fn responses_and_chat_streams_need_completion_not_just_eof() {
        let mut parser = UsageParser::new(Protocol::Responses);
        parser.set_content_type("text/event-stream");
        parser.feed(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":8,\"output_tokens\":2}}}\n\n");
        assert!(parser.finish());
        assert_eq!(parser.usage.input, Some(8));
        let mut chat = UsageParser::new(Protocol::Chat);
        chat.set_content_type("text/event-stream");
        chat.feed(b"data: {\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2}}\n\n");
        assert!(!chat.finish());
        let mut chat = UsageParser::new(Protocol::Chat);
        chat.set_content_type("text/event-stream");
        chat.feed(b"data: [DONE]\n\n");
        assert!(chat.finish());
        let mut failed = UsageParser::new(Protocol::Responses);
        failed.set_content_type("text/event-stream");
        failed.feed(b"data: {\"type\":\"response.failed\",\"error\":{\"message\":\"fail\"}}\n\n");
        assert!(!failed.finish());
        let mut invalid = UsageParser::new(Protocol::Responses);
        invalid.feed(b"not-json");
        assert!(!invalid.finish());
    }

    #[test]
    fn capture_memory_is_bounded_and_negative_usage_is_ignored() {
        let mut parser = UsageParser::new(Protocol::Responses);
        parser.feed(&vec![b'x'; MAX_CAPTURE + 1]);
        assert!(parser.buffer.is_empty());
        assert!(parser.dropping_line);
        let usage = parsed(
            Protocol::Chat,
            json!({"usage":{"prompt_tokens":-1,"completion_tokens":0}}),
        );
        assert_eq!(usage.input, None);
        assert_eq!(usage.output, Some(0));
    }

    #[test]
    fn compact_numbers_and_weighted_cache_ratio() {
        for (input, expected) in [
            (None, "-"),
            (Some(0), "0"),
            (Some(1000), "1000"),
            (Some(1001), "1K"),
            (Some(1234), "1.2K"),
            (Some(10000), "10K"),
            (Some(1234567), "1.2M"),
        ] {
            assert_eq!(format_tokens(input), expected);
        }
        let summary = Summary {
            total: 2,
            ratio_read: Some(800),
            ratio_input: Some(1000),
            ..Default::default()
        };
        assert_eq!(summary.hit_ratio(), "80%");
        assert_eq!(summary.token_text(Some(10), 1), "10");
        assert_eq!(Summary::default().hit_ratio(), "-");
        assert_eq!(Summary::default().total_token_text(), "-");
        let mut tokens = Summary {
            total: 2,
            input: Some(1200),
            output: Some(345),
            input_known: 2,
            output_known: 2,
            ..Default::default()
        };
        assert_eq!(tokens.total_token_text(), "1.5K");
        tokens.output_known = 1;
        assert_eq!(tokens.total_token_text(), "1.5K");
        tokens.output = None;
        tokens.output_known = 0;
        assert_eq!(tokens.total_token_text(), "1.2K");
    }

    #[test]
    fn sqlite_aggregates_isolate_configurations_and_keep_unknowns_null() {
        let connection = connect(std::path::Path::new(":memory:")).unwrap();
        let a = configuration_id("https://example.test", "first-key");
        let b = configuration_id("https://example.test", "second-key");
        for (id, config) in [("1", &a), ("2", &a), ("3", &a), ("4", &b)] {
            write_event(
                &connection,
                Event::Start {
                    id: id.into(),
                    configuration: config.clone(),
                    metadata: RequestMetadata::default(),
                },
            )
            .unwrap();
        }
        write_event(
            &connection,
            Event::Finish {
                id: "1".into(),
                outcome: "success".into(),
                status_code: Some(200),
                total_ms: Some(10),
                first_byte_ms: Some(5),
                usage: Usage {
                    input: Some(1000),
                    output: Some(10),
                    cache_read: Some(800),
                    cache_input: Some(1000),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        write_event(
            &connection,
            Event::Finish {
                id: "2".into(),
                outcome: "failed".into(),
                status_code: Some(500),
                total_ms: Some(10),
                first_byte_ms: None,
                usage: Usage::default(),
            },
        )
        .unwrap();
        // Finishing twice cannot double count or overwrite the first result.
        write_event(
            &connection,
            Event::Finish {
                id: "1".into(),
                outcome: "failed".into(),
                status_code: Some(500),
                total_ms: Some(10),
                first_byte_ms: None,
                usage: Usage::default(),
            },
        )
        .unwrap();
        let summaries = read_summaries(&connection).unwrap();
        let first = &summaries[&a];
        assert_eq!((first.total, first.success, first.failed), (3, 1, 1));
        assert_eq!(first.input, Some(1000));
        assert_eq!(first.input_known, 1);
        assert_eq!(first.cache_write, None);
        assert_eq!(first.hit_ratio(), "80%");
        assert_eq!((summaries[&b].total, summaries[&b].input), (1, None));
    }

    #[test]
    fn model_logs_hide_codex_probe_405_without_hiding_real_failures() {
        let connection = connect(std::path::Path::new(":memory:")).unwrap();
        let configuration = configuration_id("https://example.test", "key");
        write_event(
            &connection,
            Event::Start {
                id: "probe".into(),
                configuration: configuration.clone(),
                metadata: RequestMetadata {
                    client: "Codex Desktop".into(),
                    endpoint: "/responses".into(),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        write_event(
            &connection,
            Event::Finish {
                id: "probe".into(),
                outcome: "failed".into(),
                status_code: Some(405),
                total_ms: Some(250),
                first_byte_ms: None,
                usage: Usage::default(),
            },
        )
        .unwrap();
        write_event(
            &connection,
            Event::Start {
                id: "real".into(),
                configuration: configuration.clone(),
                metadata: RequestMetadata {
                    client: "Codex Desktop".into(),
                    model: Some("gpt-6.1-sol".into()),
                    endpoint: "/responses".into(),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        write_event(
            &connection,
            Event::Finish {
                id: "real".into(),
                outcome: "failed".into(),
                status_code: Some(405),
                total_ms: Some(250),
                first_byte_ms: None,
                usage: Usage::default(),
            },
        )
        .unwrap();

        let entries = read_model_logs(&connection, 20).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model.as_deref(), Some("gpt-6.1-sol"));
    }

    #[test]
    fn model_logs_hide_codex_probe_404_without_hiding_modelless_other_clients() {
        let connection = connect(std::path::Path::new(":memory:")).unwrap();
        let configuration = configuration_id("https://example.test", "key");
        for (id, client, endpoint, status) in [
            ("codex-probe", "Codex Desktop", "/responses", 404),
            ("claude-real", "Claude Code", "/v1/messages", 404),
        ] {
            write_event(
                &connection,
                Event::Start {
                    id: id.into(),
                    configuration: configuration.clone(),
                    metadata: RequestMetadata {
                        client: client.into(),
                        endpoint: endpoint.into(),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
            write_event(
                &connection,
                Event::Finish {
                    id: id.into(),
                    outcome: "failed".into(),
                    status_code: Some(status),
                    total_ms: Some(200),
                    first_byte_ms: None,
                    usage: Usage::default(),
                },
            )
            .unwrap();
        }
        let entries = read_model_logs(&connection, 20).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].client, "Claude Code");
    }

    #[test]
    fn dropped_request_records_failure_and_database_survives_reopen() {
        let path = std::env::temp_dir().join(format!(
            "agent-switch-usage-{:032x}.sqlite",
            rand::random::<u128>()
        ));
        let recorder = Recorder::open(&path).unwrap();
        let config = configuration_id("https://example.test", "private-key");
        {
            let _cancelled = recorder.start(config.clone(), Protocol::Responses);
        }
        let mut successful = recorder.start(config.clone(), Protocol::Responses);
        successful.http_success = true;
        successful
            .parser
            .feed(br#"{"usage":{"input_tokens":5,"output_tokens":2}}"#);
        successful.finish(true);
        drop(successful);
        recorder.flush();
        let result = load_summaries(&path).unwrap();
        assert_eq!(
            (
                result[&config].total,
                result[&config].success,
                result[&config].failed
            ),
            (2, 1, 1)
        );
        drop(recorder);
        for _ in 0..20 {
            if std::fs::remove_file(&path).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

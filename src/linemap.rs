//! Line maps: byte offset, length and kind of every complete line, no content
//! (see the archive format). fetch uses them to read only the
//! lines it needs. Classifying needs a little knowledge of each agent's
//! format; when it can't tell, a line is `meta`, and the original is never
//! touched.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::agents::AgentKind;
use crate::timefmt::parse_rfc3339_ms;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// What the person said.
    User,
    /// The agent's reply text.
    Assistant,
    Reasoning,
    /// Tool calls and results: the agent's intermediate steps.
    Tool,
    Meta,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Assistant => "assistant",
            Kind::Reasoning => "reasoning",
            Kind::Tool => "tool",
            Kind::Meta => "meta",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "user" => Some(Kind::User),
            "assistant" => Some(Kind::Assistant),
            "reasoning" => Some(Kind::Reasoning),
            "tool" => Some(Kind::Tool),
            "meta" => Some(Kind::Meta),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Line {
    pub offset: u64,
    /// Without the line break.
    pub len: u64,
    pub kind: Kind,
    /// Index into `LineMap::raw_types`.
    pub raw_type: u16,
    pub ts_ms: i64,
}

#[derive(Clone, Debug)]
pub struct LineMap {
    pub agent: AgentKind,
    pub lines: Vec<Line>,
    /// The agent's own record types, e.g. "assistant" or "event_msg/item_completed/UserMessage".
    pub raw_types: Vec<String>,
    /// Bytes covered by complete lines; a trailing partial line is left out.
    pub indexed: u64,
    /// Codex: this thread is a sub-agent, so its "user" turns were written by another agent.
    subagent: bool,
}

impl LineMap {
    pub fn new(agent: AgentKind) -> Self {
        Self {
            agent,
            lines: Vec::new(),
            raw_types: Vec::new(),
            indexed: 0,
            subagent: false,
        }
    }

    /// Indexes the complete lines `reader` yields; it must start at byte `self.indexed`.
    pub fn extend_from<R: Read>(&mut self, reader: R) -> io::Result<()> {
        let mut reader = BufReader::with_capacity(1 << 20, reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let read = reader.read_until(b'\n', &mut buf)?;
            if read == 0 || buf.last() != Some(&b'\n') {
                return Ok(()); // end of file, or a line still being written
            }
            let body = trim_line_break(&buf);
            let (kind, raw, ts_ms) = match self.agent {
                AgentKind::ClaudeCode => classify_claude(body),
                AgentKind::Codex => classify_codex(body, &mut self.subagent),
            };
            let raw_type = self.intern(raw);
            self.lines.push(Line {
                offset: self.indexed,
                len: body.len() as u64,
                kind,
                raw_type,
                ts_ms,
            });
            self.indexed += read as u64;
        }
    }

    /// Builds the map for a file on disk, or extends it if the file grew.
    pub fn update_from_file(&mut self, path: &Path) -> io::Result<()> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.indexed))?;
        self.extend_from(file)
    }

    fn intern(&mut self, raw: String) -> u16 {
        if let Some(i) = self.raw_types.iter().position(|t| *t == raw) {
            return i as u16;
        }
        if self.raw_types.len() >= u16::MAX as usize {
            return 0;
        }
        self.raw_types.push(raw);
        (self.raw_types.len() - 1) as u16
    }
}

fn trim_line_break(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

#[derive(Deserialize)]
struct TypeOnly<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
}

// ---- Claude Code ----

#[derive(Deserialize)]
struct ClaudeRecord<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
    #[serde(rename = "isCompactSummary", default)]
    is_compact_summary: Option<bool>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: Option<bool>,
    #[serde(borrow, default)]
    message: Option<ClaudeMessage<'a>>,
}

#[derive(Deserialize)]
struct ClaudeMessage<'a> {
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
}

fn classify_claude(line: &[u8]) -> (Kind, String, i64) {
    let Ok(record) = serde_json::from_slice::<ClaudeRecord>(line) else {
        return (Kind::Meta, "invalid".to_string(), 0);
    };
    let raw = record.kind.as_deref().unwrap_or_default().to_string();
    let ts_ms = record.timestamp.as_deref().and_then(parse_rfc3339_ms).unwrap_or(0);
    let blocks = record
        .message
        .as_ref()
        .and_then(|m| m.content)
        .map(block_types)
        .unwrap_or_default();
    let has = |wanted: &dyn Fn(&str) -> bool| blocks.iter().any(|b| wanted(b));
    let kind = match raw.as_str() {
        "user" if has(&|b| b == "tool_result") => Kind::Tool,
        // Sub-agent transcripts start with the main agent's instructions, not the person's words.
        "user"
            if record.is_meta == Some(true)
                || record.is_compact_summary == Some(true)
                || record.is_sidechain == Some(true) =>
        {
            Kind::Meta
        }
        "user" => Kind::User,
        "assistant" if has(&|b| b.ends_with("tool_use")) => Kind::Tool,
        "assistant" if has(&|b| b == "text") => Kind::Assistant,
        "assistant" if has(&|b| b.contains("thinking")) => Kind::Reasoning,
        _ => Kind::Meta,
    };
    (kind, raw, ts_ms)
}

/// Block types of a message's content; a plain string counts as one "text" block.
fn block_types(content: &RawValue) -> Vec<String> {
    let text = content.get().trim_start();
    if text.starts_with('"') {
        return vec!["text".to_string()];
    }
    serde_json::from_str::<Vec<TypeOnly>>(text)
        .map(|blocks| blocks.into_iter().filter_map(|b| b.kind.map(Cow::into_owned)).collect())
        .unwrap_or_default()
}

// ---- Codex ----

#[derive(Deserialize)]
struct CodexRecord<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    payload: Option<CodexPayload<'a>>,
}

#[derive(Deserialize)]
struct CodexPayload<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    item: Option<TypeOnly<'a>>,
    #[serde(default)]
    parent_thread_id: Option<serde::de::IgnoredAny>,
    #[serde(borrow, default)]
    source: Option<&'a RawValue>,
}

/// Codex writes each turn twice: `response_item` for the model and `event_msg`
/// for the UI. Kinds come from the UI side so a sentence isn't returned twice;
/// every `response_item` is `meta`.
fn classify_codex(line: &[u8], subagent: &mut bool) -> (Kind, String, i64) {
    let Ok(record) = serde_json::from_slice::<CodexRecord>(line) else {
        return (Kind::Meta, "invalid".to_string(), 0);
    };
    let ts_ms = record.timestamp.as_deref().and_then(parse_rfc3339_ms).unwrap_or(0);
    let top = record.kind.as_deref().unwrap_or_default();
    let payload = record.payload.as_ref();
    let payload_kind = payload.and_then(|p| p.kind.as_deref()).unwrap_or_default();
    let item_kind = payload
        .and_then(|p| p.item.as_ref())
        .and_then(|i| i.kind.as_deref())
        .unwrap_or_default();
    let person = if *subagent { Kind::Meta } else { Kind::User };
    let (kind, raw) = match (top, payload_kind) {
        ("session_meta", _) => {
            if let Some(p) = payload {
                let spawned = p.source.is_some_and(|s| s.get().contains("\"subagent\""));
                *subagent = p.parent_thread_id.is_some() || spawned;
            }
            (Kind::Meta, "session_meta".to_string())
        }
        ("event_msg", "item_completed") => {
            let kind = match item_kind {
                "UserMessage" => person,
                "AgentMessage" => Kind::Assistant,
                "Reasoning" => Kind::Reasoning,
                "" | "ContextCompaction" => Kind::Meta,
                _ => Kind::Tool,
            };
            (kind, format!("event_msg/item_completed/{item_kind}"))
        }
        ("event_msg", _) => {
            // Older ("legacy") rollouts.
            let kind = match payload_kind {
                "user_message" => person,
                "agent_message" => Kind::Assistant,
                "agent_reasoning" | "agent_reasoning_raw_content" => Kind::Reasoning,
                "exec_command_end" | "patch_apply_end" | "mcp_tool_call_end" | "web_search_end" => Kind::Tool,
                _ => Kind::Meta,
            };
            (kind, format!("event_msg/{payload_kind}"))
        }
        ("response_item", _) => (Kind::Meta, format!("response_item/{payload_kind}")),
        (other, _) => (Kind::Meta, other.to_string()),
    };
    (kind, raw, ts_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(agent: AgentKind, text: &str) -> LineMap {
        let mut map = LineMap::new(agent);
        map.extend_from(text.as_bytes()).unwrap();
        map
    }

    fn kinds(map: &LineMap) -> Vec<&'static str> {
        map.lines.iter().map(|l| l.kind.as_str()).collect()
    }

    const CLAUDE: &str = concat!(
        r#"{"type":"user","timestamp":"2026-11-15T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
        "\n",
        r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
        "\n",
        r#"{"type":"user","isMeta":true,"message":{"content":"<command-name>"}}"#,
        "\n",
        r#"{"type":"user","isSidechain":true,"message":{"content":"task for a sub-agent"}}"#,
        "\r\n",
        r#"{"type":"custom-title","customTitle":"x"}"#,
        "\n",
        "not json\n",
        r#"{"type":"user","message":{"content":"still being writ"#,
    );

    #[test]
    fn classifies_claude_code_lines() {
        let map = build(AgentKind::ClaudeCode, CLAUDE);
        assert_eq!(
            kinds(&map),
            [
                "user",
                "reasoning",
                "tool",
                "tool",
                "assistant",
                "meta",
                "meta",
                "meta",
                "meta"
            ]
        );
        assert_eq!(map.lines[0].ts_ms, 1_794_700_800_000);
        assert_eq!(map.raw_types[map.lines[8].raw_type as usize], "invalid");
    }

    #[test]
    fn offsets_skip_line_breaks_and_partial_lines() {
        let map = build(AgentKind::ClaudeCode, CLAUDE);
        let complete: usize = CLAUDE.rfind('\n').unwrap() + 1;
        assert_eq!(map.indexed, complete as u64);
        for line in &map.lines {
            let bytes = &CLAUDE.as_bytes()[line.offset as usize..(line.offset + line.len) as usize];
            assert!(!bytes.ends_with(b"\n") && !bytes.ends_with(b"\r"));
        }
        let sidechain = &map.lines[6];
        assert!(CLAUDE[sidechain.offset as usize..].starts_with(r#"{"type":"user","isSidechain""#));
    }

    #[test]
    fn extending_matches_building_at_once() {
        let whole = build(AgentKind::ClaudeCode, CLAUDE);
        let split = CLAUDE.find("\n{\"type\":\"user\",\"message\":{\"content\":[").unwrap() + 1;
        let mut map = build(AgentKind::ClaudeCode, &CLAUDE[..split]);
        map.extend_from(&CLAUDE.as_bytes()[map.indexed as usize..]).unwrap();
        assert_eq!(map.indexed, whole.indexed);
        assert_eq!(map.lines.len(), whole.lines.len());
        assert_eq!(map.lines[5].offset, whole.lines[5].offset);
    }

    const CODEX: &str = concat!(
        r#"{"timestamp":"2026-11-15T00:00:00.000Z","ordinal":0,"type":"session_meta","payload":{"id":"t","source":"vscode"}}"#,
        "\n",
        r#"{"timestamp":"2026-11-15T00:00:01.000Z","ordinal":1,"type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#,
        "\n",
        r#"{"timestamp":"2026-11-15T00:00:01.000Z","ordinal":2,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[]}}}"#,
        "\n",
        r#"{"timestamp":"2026-11-15T00:00:02.000Z","ordinal":3,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning"}}}"#,
        "\n",
        r#"{"timestamp":"2026-11-15T00:00:03.000Z","ordinal":4,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution"}}}"#,
        "\n",
        r#"{"timestamp":"2026-11-15T00:00:04.000Z","ordinal":5,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","phase":"final_answer"}}}"#,
        "\n",
        r#"{"timestamp":"2026-11-15T00:00:05.000Z","ordinal":6,"type":"event_msg","payload":{"type":"token_count"}}"#,
        "\n",
        r#"{"timestamp":"2026-11-15T00:00:06.000Z","ordinal":7,"type":"event_msg","payload":{"type":"user_message","message":"legacy"}}"#,
        "\n",
    );

    #[test]
    fn classifies_codex_lines() {
        let map = build(AgentKind::Codex, CODEX);
        assert_eq!(
            kinds(&map),
            ["meta", "meta", "user", "reasoning", "tool", "assistant", "meta", "user"]
        );
        assert_eq!(
            map.raw_types[map.lines[2].raw_type as usize],
            "event_msg/item_completed/UserMessage"
        );
        assert_eq!(map.lines[5].ts_ms, 1_794_700_804_000);
    }

    #[test]
    fn codex_sub_agent_turns_are_not_the_person() {
        let sub = CODEX.replacen(
            r#""source":"vscode""#,
            r#""source":{"subagent":{"thread_spawn":{}}}"#,
            1,
        );
        assert_eq!(kinds(&build(AgentKind::Codex, &sub))[2], "meta");
        let with_parent = CODEX.replacen(r#""id":"t","#, r#""id":"t","parent_thread_id":"p","#, 1);
        assert_eq!(kinds(&build(AgentKind::Codex, &with_parent))[2], "meta");
    }
}

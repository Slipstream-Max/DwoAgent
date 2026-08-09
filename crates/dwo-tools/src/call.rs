use std::path::PathBuf;

use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::terminal::TerminalId;

const DEFAULT_YIELD_MS: u64 = 60_000;
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
pub(crate) const DEFAULT_READ_FILE_LINES: usize = 500;
pub(crate) const MAX_READ_FILE_LINES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToolCall {
    pub id: String,
    pub call: ToolCall,
    pub raw_arguments: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCall {
    Terminal(TerminalArgs),
    FileEdit(FileEditArgs),
    ReadFile(ReadFileArgs),
    Handoff(HandoffArgs),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffArgs {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFileArgs {
    pub path: PathBuf,
    pub cursor: usize,
    pub line_count: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalArgs {
    Run {
        command: String,
        cwd: Option<PathBuf>,
        yield_ms: u64,
        timeout_ms: u64,
    },
    Input {
        terminal_id: TerminalId,
        data: String,
        yield_ms: u64,
    },
    Kill {
        terminal_id: TerminalId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditArgs {
    pub patch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolIntent {
    TerminalRun {
        command: String,
    },
    TerminalInput {
        terminal_id: TerminalId,
        data: String,
    },
    TerminalKill {
        terminal_id: TerminalId,
    },
    FileEdit,
    ReadFile,
    Handoff,
}

impl ToolCall {
    pub fn intent(&self) -> ToolIntent {
        match self {
            Self::Terminal(TerminalArgs::Run { command, .. }) => ToolIntent::TerminalRun {
                command: command.clone(),
            },
            Self::Terminal(TerminalArgs::Input {
                terminal_id, data, ..
            }) => ToolIntent::TerminalInput {
                terminal_id: terminal_id.clone(),
                data: data.clone(),
            },
            Self::Terminal(TerminalArgs::Kill { terminal_id }) => ToolIntent::TerminalKill {
                terminal_id: terminal_id.clone(),
            },
            Self::FileEdit(_) => ToolIntent::FileEdit,
            Self::ReadFile(_) => ToolIntent::ReadFile,
            Self::Handoff(_) => ToolIntent::Handoff,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Terminal(_) => "terminal",
            Self::FileEdit(_) => "file_edit",
            Self::ReadFile(_) => "read_file",
            Self::Handoff(_) => "handoff",
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize)]
#[error("{message}")]
pub struct ToolCallParseError {
    pub id: String,
    pub name: String,
    pub message: String,
}

impl ParsedToolCall {
    pub fn parse(raw: Value) -> Result<Self, ToolCallParseError> {
        let object = raw
            .as_object()
            .ok_or_else(|| parse_error("", "", "Tool call must be an object."))?;
        let id = string_field(object, &["tool_call_id", "id"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let name = string_field(object, &["name"])
            .unwrap_or_default()
            .trim()
            .to_string();
        if id.is_empty() {
            return Err(parse_error(&id, &name, "Tool call id is required."));
        }
        if name.is_empty() {
            return Err(parse_error(&id, &name, "Tool name is required."));
        }
        let arguments = parse_arguments(object.get("arguments"))
            .map_err(|message| parse_error(&id, &name, message))?;
        let call = match name.as_str() {
            "terminal" => ToolCall::Terminal(
                parse_terminal(&arguments).map_err(|message| parse_error(&id, &name, message))?,
            ),
            "file_edit" => ToolCall::FileEdit(
                parse_file_edit(&arguments).map_err(|message| parse_error(&id, &name, message))?,
            ),
            "read_file" => ToolCall::ReadFile(
                parse_read_file(&arguments).map_err(|message| parse_error(&id, &name, message))?,
            ),
            "handoff" => ToolCall::Handoff(
                parse_handoff(&arguments).map_err(|message| parse_error(&id, &name, message))?,
            ),
            _ => return Err(parse_error(&id, &name, format!("Unknown tool: {name}"))),
        };
        Ok(Self {
            id,
            call,
            raw_arguments: arguments,
        })
    }
}

fn parse_handoff(args: &Map<String, Value>) -> Result<HandoffArgs, String> {
    let text = required_string(args, "handoff_text")?;
    if text.trim().is_empty() {
        return Err("handoff_text must not be empty.".to_string());
    }
    if text.len() > 32_000 {
        return Err("handoff_text must not exceed 32000 UTF-8 bytes.".to_string());
    }
    Ok(HandoffArgs { text })
}

fn parse_arguments(value: Option<&Value>) -> Result<Map<String, Value>, String> {
    match value {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(Value::String(raw)) => serde_json::from_str::<Map<String, Value>>(raw)
            .map_err(|error| format!("Tool arguments are not valid JSON: {error}")),
        Some(_) => {
            Err("Tool arguments must be a JSON object or an encoded JSON object.".to_string())
        }
    }
}

fn parse_terminal(args: &Map<String, Value>) -> Result<TerminalArgs, String> {
    let terminal_id =
        optional_string(args, "terminal_id")?.filter(|terminal_id| !terminal_id.trim().is_empty());
    let kill = args.get("kill").and_then(Value::as_bool).unwrap_or(false);
    if kill {
        let terminal_id =
            terminal_id.ok_or_else(|| "terminal_id is required when kill is true.".to_string())?;
        return Ok(TerminalArgs::Kill {
            terminal_id: TerminalId::parse(&terminal_id)?,
        });
    }
    if let Some(terminal_id) = terminal_id {
        return Ok(TerminalArgs::Input {
            terminal_id: TerminalId::parse(&terminal_id)?,
            data: optional_string(args, "command")?.unwrap_or_default(),
            yield_ms: duration_field(args, "yield_ms", DEFAULT_YIELD_MS)?,
        });
    }
    let command = required_string(args, "command")?;
    if command.trim().is_empty() {
        return Err("terminal command must not be empty.".to_string());
    }
    Ok(TerminalArgs::Run {
        command,
        cwd: optional_string(args, "cwd")?.map(PathBuf::from),
        yield_ms: duration_field(args, "yield_ms", DEFAULT_YIELD_MS)?,
        timeout_ms: duration_field(args, "timeout_ms", DEFAULT_TIMEOUT_MS)?,
    })
}

fn parse_file_edit(args: &Map<String, Value>) -> Result<FileEditArgs, String> {
    let patch = required_string(args, "patch")?;
    if patch.trim().is_empty() {
        return Err("file_edit patch must not be empty.".to_string());
    }
    Ok(FileEditArgs { patch })
}

fn parse_read_file(args: &Map<String, Value>) -> Result<ReadFileArgs, String> {
    let path = required_string(args, "path")?;
    if path.trim().is_empty() {
        return Err("read_file path must not be empty.".to_string());
    }
    let cursor = match args.get("cursor") {
        None | Some(Value::Null) => 1,
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| "Argument cursor must be a positive integer.".to_string())?,
        Some(_) => return Err("Argument cursor must be a positive integer.".to_string()),
    };
    let line_count = match args.get("line_count") {
        None | Some(Value::Null) => DEFAULT_READ_FILE_LINES,
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| (1..=MAX_READ_FILE_LINES).contains(value))
            .ok_or_else(|| {
                format!(
                    "Argument line_count must be an integer between 1 and {MAX_READ_FILE_LINES}."
                )
            })?,
        Some(_) => {
            return Err(format!(
                "Argument line_count must be an integer between 1 and {MAX_READ_FILE_LINES}."
            ));
        }
    };
    let offset = match args.get("offset") {
        None | Some(Value::Null) => 0,
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| "Argument offset must be a non-negative integer.".to_string())?,
        Some(_) => return Err("Argument offset must be a non-negative integer.".to_string()),
    };
    Ok(ReadFileArgs {
        path: PathBuf::from(path),
        cursor,
        line_count,
        offset,
    })
}

fn required_string(args: &Map<String, Value>, key: &str) -> Result<String, String> {
    optional_string(args, key)?.ok_or_else(|| format!("Missing argument: {key}"))
}

fn optional_string(args: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("Argument {key} must be a string.")),
    }
}

fn duration_field(args: &Map<String, Value>, key: &str, default: u64) -> Result<u64, String> {
    Ok(optional_duration_field(args, key)?.unwrap_or(default))
}

fn optional_duration_field(args: &Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| format!("Argument {key} must be a positive integer.")),
        Some(_) => Err(format!("Argument {key} must be a positive integer.")),
    }
}

fn string_field<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
}

fn parse_error(id: &str, name: &str, message: impl Into<String>) -> ToolCallParseError {
    ToolCallParseError {
        id: id.to_string(),
        name: name.to_string(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn accepts_string_and_object_arguments() {
        for arguments in [
            json!({"command":"rg foo"}),
            json!(r#"{"command":"rg foo"}"#),
        ] {
            let parsed = ParsedToolCall::parse(json!({
                "id": "call-1",
                "name": "terminal",
                "arguments": arguments,
            }))
            .unwrap();
            assert!(matches!(
                parsed.call,
                ToolCall::Terminal(TerminalArgs::Run { .. })
            ));
        }
    }

    #[test]
    fn terminal_id_without_command_is_a_poll() {
        let parsed = ParsedToolCall::parse(json!({
            "id": "call-1",
            "name": "terminal",
            "arguments": {"terminal_id":"term-1"},
        }))
        .unwrap();
        assert!(matches!(
            parsed.call,
            ToolCall::Terminal(TerminalArgs::Input { data, .. }) if data.is_empty()
        ));
    }

    #[test]
    fn empty_terminal_id_creates_a_new_terminal() {
        let parsed = ParsedToolCall::parse(json!({
            "id": "call-1",
            "name": "terminal",
            "arguments": {"terminal_id":"", "command":"rg foo"},
        }))
        .unwrap();
        assert!(matches!(
            parsed.call,
            ToolCall::Terminal(TerminalArgs::Run { .. })
        ));
    }

    #[test]
    fn command_with_terminal_id_is_sent_as_input() {
        let parsed = ParsedToolCall::parse(json!({
            "id": "call-1",
            "name": "terminal",
            "arguments": {"terminal_id":"term-1", "command":"ls\n"},
        }))
        .unwrap();
        assert!(matches!(
            parsed.call,
            ToolCall::Terminal(TerminalArgs::Input { data, .. }) if data == "ls\n"
        ));
    }

    #[test]
    fn kill_takes_precedence_and_requires_terminal_id() {
        let parsed = ParsedToolCall::parse(json!({
            "id": "call-1",
            "name": "terminal",
            "arguments": {"terminal_id":"term-1", "kill":true, "command":"ls"},
        }))
        .unwrap();
        assert!(matches!(
            parsed.call,
            ToolCall::Terminal(TerminalArgs::Kill { .. })
        ));

        let error = ParsedToolCall::parse(json!({
            "id": "call-2",
            "name": "terminal",
            "arguments": {"kill": true},
        }))
        .unwrap_err();
        assert!(
            error
                .message
                .contains("terminal_id is required when kill is true")
        );
    }

    #[test]
    fn new_terminal_requires_a_command() {
        let error = ParsedToolCall::parse(json!({
            "id": "call-1",
            "name": "terminal",
            "arguments": {},
        }))
        .unwrap_err();
        assert!(error.message.contains("Missing argument: command"));
    }

    #[test]
    fn read_file_accepts_bounded_line_count() {
        let parsed = ParsedToolCall::parse(json!({
            "id": "call-1",
            "name": "read_file",
            "arguments": {"path":"src/main.rs", "cursor":20, "line_count":40},
        }))
        .unwrap();
        assert!(matches!(
            parsed.call,
            ToolCall::ReadFile(ReadFileArgs {
                cursor: 20,
                line_count: 40,
                ..
            })
        ));

        let error = ParsedToolCall::parse(json!({
            "id": "call-2",
            "name": "read_file",
            "arguments": {"path":"src/main.rs", "line_count":501},
        }))
        .unwrap_err();
        assert!(error.message.contains("between 1 and 500"));
    }

    #[test]
    fn handoff_requires_bounded_nonempty_text() {
        let parsed = ParsedToolCall::parse(json!({
            "id": "handoff-1",
            "name": "handoff",
            "arguments": {"handoff_text": "done; next: test"}
        }))
        .unwrap();
        assert!(
            matches!(parsed.call, ToolCall::Handoff(HandoffArgs { ref text }) if text == "done; next: test")
        );

        let error = ParsedToolCall::parse(json!({
            "id": "handoff-2",
            "name": "handoff",
            "arguments": {"handoff_text": "  "}
        }))
        .unwrap_err();
        assert!(error.message.contains("must not be empty"));
    }

    #[test]
    fn read_file_accepts_a_unicode_character_offset() {
        let parsed = ParsedToolCall::parse(json!({
            "id": "call-offset",
            "name": "read_file",
            "arguments": {"path":"big.html", "cursor":8, "line_count":3, "offset":12000},
        }))
        .unwrap();
        assert!(matches!(
            parsed.call,
            ToolCall::ReadFile(ReadFileArgs {
                cursor: 8,
                line_count: 3,
                offset: 12000,
                ..
            })
        ));
    }
}

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::terminal::TerminalId;

const DEFAULT_YIELD_MS: u64 = 10_000;
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
        tty: bool,
        yield_ms: u64,
        timeout_ms: Option<u64>,
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
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Terminal(_) => "terminal",
            Self::FileEdit(_) => "file_edit",
            Self::ReadFile(_) => "read_file",
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
            _ => return Err(parse_error(&id, &name, format!("Unknown tool: {name}"))),
        };
        Ok(Self {
            id,
            call,
            raw_arguments: arguments,
        })
    }
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
    let action = required_string(args, "action")?;
    match action.as_str() {
        "run" => {
            let command = required_string(args, "command")?;
            if command.trim().is_empty() {
                return Err("terminal.run command must not be empty.".to_string());
            }
            Ok(TerminalArgs::Run {
                command,
                cwd: optional_string(args, "cwd")?.map(PathBuf::from),
                tty: args.get("tty").and_then(Value::as_bool).unwrap_or(false),
                yield_ms: duration_field(args, "yield_ms", DEFAULT_YIELD_MS)?,
                timeout_ms: optional_duration_field(args, "timeout_ms")?,
            })
        }
        "input" => Ok(TerminalArgs::Input {
            terminal_id: TerminalId::parse(&required_string(args, "terminal_id")?)?,
            data: optional_string(args, "data")?.unwrap_or_default(),
            yield_ms: duration_field(args, "yield_ms", DEFAULT_YIELD_MS)?,
        }),
        "kill" => Ok(TerminalArgs::Kill {
            terminal_id: TerminalId::parse(&required_string(args, "terminal_id")?)?,
        }),
        other => Err(format!("Unknown terminal action: {other}")),
    }
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
            json!({"action":"run", "command":"rg foo"}),
            json!(r#"{"action":"run","command":"rg foo"}"#),
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
    fn empty_input_is_a_valid_poll() {
        let parsed = ParsedToolCall::parse(json!({
            "id": "call-1",
            "name": "terminal",
            "arguments": {"action":"input", "terminal_id":"term-1", "data":""},
        }))
        .unwrap();
        assert!(matches!(
            parsed.call,
            ToolCall::Terminal(TerminalArgs::Input { data, .. }) if data.is_empty()
        ));
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
}

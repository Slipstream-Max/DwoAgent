use serde_json::{Value, json};

use crate::call::{DEFAULT_READ_FILE_LINES, MAX_READ_FILE_LINES};

pub fn tool_schemas() -> Vec<Value> {
    vec![terminal_schema(), read_file_schema(), file_edit_schema()]
}

pub fn read_file_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read UTF-8 text or add a PNG, JPEG, GIF, or WebP image to model context. Text output is contiguous and capped at 20000 UTF-8 bytes across the entire result. Start at a 1-based line with cursor and an optional 0-based Unicode character offset. If more text remains, pass the returned next_cursor and next_offset unchanged to continue.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path. Relative paths are resolved from the session working directory."
                    },
                    "cursor": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 1,
                        "description": "Optional 1-based start line. Defaults to 1. For continuation, use the returned next_cursor."
                    },
                    "line_count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_READ_FILE_LINES,
                        "default": DEFAULT_READ_FILE_LINES,
                        "description": "Optional number of text lines to return. Defaults to 500 and cannot exceed 500."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Optional 0-based Unicode character offset within cursor. Defaults to 0. For continuation, use the returned next_offset."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }
    })
}

pub fn terminal_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "terminal",
            "description": "Run a command, write or poll an interactive terminal, or kill it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {"type":"string", "enum":["run","input","kill"]},
                    "command": {"type":"string"},
                    "cwd": {"type":"string"},
                    "tty": {"type":"boolean", "default":false},
                    "yield_ms": {"type":"integer", "minimum":1, "default":10000},
                    "timeout_ms": {"type":"integer", "minimum":1},
                    "terminal_id": {"type":"string"},
                    "data": {"type":"string", "description":"Empty data polls without writing."}
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }
    })
}

pub fn file_edit_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "file_edit",
            "description": "Apply a structured patch to files.",
            "parameters": {
                "type":"object",
                "properties":{"patch":{"type":"string"}},
                "required":["patch"],
                "additionalProperties":false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file_schema_describes_line_and_offset_paging() {
        let schema = read_file_schema();
        let parameters = &schema["function"]["parameters"];
        assert_eq!(parameters["required"], json!(["path"]));
        assert_eq!(parameters["additionalProperties"], false);
        assert!(parameters["properties"].get("path").is_some());
        assert!(parameters["properties"].get("cursor").is_some());
        assert_eq!(parameters["properties"]["cursor"]["default"], 1);
        assert_eq!(parameters["properties"]["line_count"]["minimum"], 1);
        assert_eq!(parameters["properties"]["line_count"]["maximum"], 500);
        assert_eq!(parameters["properties"]["line_count"]["default"], 500);
        assert_eq!(parameters["properties"]["offset"]["minimum"], 0);
        assert_eq!(parameters["properties"]["offset"]["default"], 0);
    }
}

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
            "description": "Read a selected range from a UTF-8 text file, or add a PNG, JPEG, GIF, or WebP image directly to model context.",
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
                        "description": "Optional 1-based start line. Continue with the previous end_line + 1."
                    },
                    "line_count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_READ_FILE_LINES,
                        "default": DEFAULT_READ_FILE_LINES,
                        "description": "Optional number of text lines to return. Defaults to 500 and cannot exceed 500."
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
    fn read_file_schema_accepts_optional_line_count() {
        let schema = read_file_schema();
        let parameters = &schema["function"]["parameters"];
        assert_eq!(parameters["required"], json!(["path"]));
        assert_eq!(parameters["additionalProperties"], false);
        assert!(parameters["properties"].get("path").is_some());
        assert!(parameters["properties"].get("cursor").is_some());
        assert_eq!(parameters["properties"]["line_count"]["minimum"], 1);
        assert_eq!(parameters["properties"]["line_count"]["maximum"], 500);
        assert_eq!(parameters["properties"]["line_count"]["default"], 500);
        assert!(parameters["properties"].get("next_cursor").is_none());
    }
}

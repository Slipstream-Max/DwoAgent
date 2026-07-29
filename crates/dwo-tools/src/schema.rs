use serde_json::{Value, json};

pub fn tool_schemas() -> Vec<Value> {
    vec![terminal_schema(), read_file_schema(), file_edit_schema()]
}

pub fn read_file_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a UTF-8 text file in pages of up to 500 lines, or add a PNG, JPEG, GIF, or WebP image directly to model context.",
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
    fn read_file_schema_only_accepts_path_and_cursor() {
        let schema = read_file_schema();
        let parameters = &schema["function"]["parameters"];
        assert_eq!(parameters["required"], json!(["path"]));
        assert_eq!(parameters["additionalProperties"], false);
        assert!(parameters["properties"].get("path").is_some());
        assert!(parameters["properties"].get("cursor").is_some());
        assert!(parameters["properties"].get("line_count").is_none());
        assert!(parameters["properties"].get("next_cursor").is_none());
    }
}

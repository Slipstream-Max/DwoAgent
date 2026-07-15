use serde_json::{Value, json};

pub fn tool_schemas() -> Vec<Value> {
    vec![terminal_schema(), file_edit_schema()]
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

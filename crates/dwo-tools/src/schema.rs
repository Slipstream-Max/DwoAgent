use serde_json::{Value, json};

use crate::call::{DEFAULT_READ_FILE_LINES, MAX_READ_FILE_LINES};

pub fn tool_schemas() -> Vec<Value> {
    vec![
        terminal_schema(),
        read_file_schema(),
        file_edit_schema(),
        handoff_schema(),
    ]
}

pub fn handoff_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "handoff",
            "description": "Write a precise handoff summary, then end this turn and rebuild the model context around it. Include the goal, completed work, decisions, unresolved issues, and next steps.",
            "parameters": {
                "type": "object",
                "properties": { "handoff_text": { "type": "string", "minLength": 1 } },
                "required": ["handoff_text"],
                "additionalProperties": false
            }
        }
    })
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
            "description": "Run a command in a new terminal, interact with or poll an existing terminal, or kill one. Every terminal is interactive. Without terminal_id a new terminal is created and command is executed; with terminal_id the command (include a trailing newline) is sent as input to that terminal, or omitted to poll for new output; kill true terminates the terminal.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command to run in a new terminal, or input to send to the terminal identified by terminal_id (include a trailing newline). Omit to poll for incremental output."
                    },
                    "terminal_id": {
                        "type": "string",
                        "description": "ID of an existing terminal returned by a previous call. Omit to create a new terminal. An unknown ID is reported as an error and is never recreated."
                    },
                    "kill": {
                        "type": "boolean",
                        "default": false,
                        "description": "Set true to terminate the terminal identified by terminal_id. Trailing output is drained before returning."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the new terminal. Relative paths are resolved from the session workspace. Ignored when terminal_id is provided."
                    },
                    "yield_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 60000,
                        "description": "Maximum milliseconds to wait for output. Returns as soon as the command exits; it does not terminate the command. Default 60000."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 600000,
                        "description": "Total runtime limit in milliseconds for a new terminal. Reaching it terminates the process tree. Ignored when terminal_id is provided. Default 600000."
                    }
                },
                "required": [],
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

    #[test]
    fn handoff_schema_requires_handoff_text() {
        let parameters = &handoff_schema()["function"]["parameters"];
        assert_eq!(parameters["required"], json!(["handoff_text"]));
        assert_eq!(parameters["additionalProperties"], false);
    }
}

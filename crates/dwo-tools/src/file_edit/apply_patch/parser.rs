use std::path::PathBuf;

use anyhow::{Result, bail};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF: &str = "*** End of File";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Hunk {
    Add {
        path: PathBuf,
        contents: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpdateChunk {
    pub context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub end_of_file: bool,
}

pub(super) fn parse_patch(patch: &str) -> Result<Vec<Hunk>> {
    let normalized = patch.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.lines().collect();
    if lines.first().map(|line| line.trim()) != Some(BEGIN) {
        bail!("The first line of the patch must be '{BEGIN}'");
    }
    if lines.last().map(|line| line.trim()) != Some(END) {
        bail!("The last line of the patch must be '{END}'");
    }
    let mut hunks = Vec::new();
    let mut index = 1;
    while index < lines.len() - 1 {
        let line = lines[index];
        if let Some(path) = line.strip_prefix(ADD) {
            index += 1;
            let mut contents = Vec::new();
            while index < lines.len() - 1 && !is_header(lines[index]) {
                let Some(content) = lines[index].strip_prefix('+') else {
                    bail!(
                        "Invalid Add File line {}: every line must start with '+'",
                        index + 1
                    );
                };
                contents.push(content);
                index += 1;
            }
            if contents.is_empty() {
                bail!("Add File requires at least one content line");
            }
            hunks.push(Hunk::Add {
                path: parse_path(path)?,
                contents: format!("{}\n", contents.join("\n")),
            });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            hunks.push(Hunk::Delete {
                path: parse_path(path)?,
            });
            index += 1;
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            index += 1;
            let move_path = if index < lines.len() - 1 {
                lines[index]
                    .strip_prefix(MOVE)
                    .map(parse_path)
                    .transpose()?
            } else {
                None
            };
            if move_path.is_some() {
                index += 1;
            }
            let mut chunks = Vec::new();
            while index < lines.len() - 1 && !is_header(lines[index]) {
                let context = if lines[index] == "@@" {
                    index += 1;
                    None
                } else if let Some(context) = lines[index].strip_prefix("@@ ") {
                    index += 1;
                    Some(context.to_string())
                } else if chunks.is_empty() {
                    None
                } else {
                    bail!("Invalid update hunk at line {}", index + 1);
                };
                let mut old_lines = Vec::new();
                let mut new_lines = Vec::new();
                let mut end_of_file = false;
                while index < lines.len() - 1
                    && !is_header(lines[index])
                    && !lines[index].starts_with("@@")
                {
                    let change = lines[index];
                    if change == EOF {
                        end_of_file = true;
                        index += 1;
                        break;
                    }
                    let Some(prefix) = change.chars().next() else {
                        bail!("Invalid empty update line at {}", index + 1);
                    };
                    let content = &change[prefix.len_utf8()..];
                    match prefix {
                        ' ' => {
                            old_lines.push(content.to_string());
                            new_lines.push(content.to_string());
                        }
                        '-' => old_lines.push(content.to_string()),
                        '+' => new_lines.push(content.to_string()),
                        _ => bail!(
                            "Invalid update line {}: expected ' ', '+' or '-'",
                            index + 1
                        ),
                    }
                    index += 1;
                }
                if old_lines.is_empty() && new_lines.is_empty() {
                    bail!("Empty update hunk");
                }
                chunks.push(UpdateChunk {
                    context,
                    old_lines,
                    new_lines,
                    end_of_file,
                });
            }
            if chunks.is_empty() {
                bail!("Update File requires at least one hunk");
            }
            hunks.push(Hunk::Update {
                path: parse_path(path)?,
                move_path,
                chunks,
            });
        } else {
            bail!("Unknown patch header at line {}: {line}", index + 1);
        }
    }
    if hunks.is_empty() {
        bail!("Patch must contain at least one file operation");
    }
    Ok(hunks)
}

fn parse_path(path: &str) -> Result<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        bail!("Patch path must not be empty");
    }
    Ok(PathBuf::from(path))
}

fn is_header(line: &str) -> bool {
    line.starts_with(ADD) || line.starts_with(DELETE) || line.starts_with(UPDATE) || line == END
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_operations() {
        let hunks =
            parse_patch("*** Begin Patch\n*** Add File: a\n+x\n*** Delete File: b\n*** End Patch")
                .unwrap();
        assert_eq!(hunks.len(), 2);
    }

    #[test]
    fn rejects_missing_boundaries() {
        assert!(parse_patch("*** Add File: a\n+x").is_err());
    }
}

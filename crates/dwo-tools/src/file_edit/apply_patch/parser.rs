use std::path::PathBuf;

use anyhow::{Result, bail};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ENVIRONMENT_ID: &str = "*** Environment ID:";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const REPLACE: &str = "*** Replace File: ";
const EXPECTED: &str = "*** Expected: ";
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
    Replace {
        path: PathBuf,
        expected: usize,
        old: String,
        new: String,
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
    let trimmed = patch.trim();
    let original_lines = trimmed.lines().collect::<Vec<_>>();
    let lines = patch_lines(&original_lines)?;
    let mut hunks = Vec::new();
    let mut index = 1;
    let mut environment_id_seen = false;

    while index < lines.len() - 1 {
        let header = lines[index].trim();
        if let Some(environment_id) = header.strip_prefix(ENVIRONMENT_ID) {
            if !hunks.is_empty() || environment_id_seen {
                bail!("apply_patch environment_id cannot be specified more than once");
            }
            if environment_id.trim().is_empty() {
                bail!("apply_patch environment_id cannot be empty");
            }
            environment_id_seen = true;
            index += 1;
            continue;
        }

        if let Some(path) = header.strip_prefix(ADD) {
            index += 1;
            let mut contents = String::new();
            while index < lines.len() - 1 && !is_trimmed_header(lines[index]) {
                let line = lines[index];
                let Some(content) = line.strip_prefix('+') else {
                    bail!(
                        "Invalid Add File line {}: every line must start with '+'",
                        index + 1
                    );
                };
                contents.push_str(content);
                contents.push('\n');
                index += 1;
            }
            hunks.push(Hunk::Add {
                path: PathBuf::from(path),
                contents,
            });
            continue;
        }

        if let Some(path) = header.strip_prefix(DELETE) {
            hunks.push(Hunk::Delete {
                path: PathBuf::from(path),
            });
            index += 1;
            continue;
        }

        if let Some(path) = header.strip_prefix(REPLACE) {
            let hunk_line = index + 1;
            index += 1;
            let Some(expected_line) = lines.get(index) else {
                bail!("Replace file hunk at line {hunk_line} is missing '{EXPECTED}<count>'");
            };
            let Some(expected) = expected_line.trim().strip_prefix(EXPECTED) else {
                bail!("Replace file hunk at line {hunk_line} is missing '{EXPECTED}<count>'");
            };
            let expected = expected.trim().parse::<usize>().map_err(|_| {
                anyhow::anyhow!("Invalid Replace File expected count at line {}", index + 1)
            })?;
            if expected == 0 {
                bail!(
                    "Replace File expected count must be at least 1 at line {}",
                    index + 1
                );
            }
            index += 1;
            if lines.get(index).map(|line| line.trim_end()) != Some("@@") {
                bail!(
                    "Expected Replace File body to start with '@@' at line {}",
                    index + 1
                );
            }
            index += 1;

            let mut old_lines = Vec::new();
            let mut new_lines = Vec::new();
            let mut reading_new = false;
            while index < lines.len() - 1 && !is_trimmed_header(lines[index]) {
                let line = lines[index];
                if let Some(content) = line.strip_prefix('-') {
                    if reading_new {
                        bail!(
                            "Invalid Replace File line {}: removed lines must precede added lines",
                            index + 1
                        );
                    }
                    old_lines.push(content);
                } else if let Some(content) = line.strip_prefix('+') {
                    reading_new = true;
                    new_lines.push(content);
                } else {
                    bail!(
                        "Invalid Replace File line {}: expected '-' or '+'",
                        index + 1
                    );
                }
                index += 1;
            }
            if old_lines.is_empty() {
                bail!("Replace file hunk for path '{path}' has no content to match");
            }
            let old = old_lines.join("\n");
            let new = new_lines.join("\n");
            if old == new {
                bail!("Replace file hunk for path '{path}' does not change the content");
            }
            hunks.push(Hunk::Replace {
                path: PathBuf::from(path),
                expected,
                old,
                new,
            });
            continue;
        }

        if let Some(path) = header.strip_prefix(UPDATE) {
            let hunk_line = index + 1;
            index += 1;
            let mut move_path = None;
            let mut chunks = Vec::new();

            while index < lines.len() - 1 {
                let line = lines[index];
                let update_line = line.trim_end();
                if is_update_header(update_line) {
                    break;
                }

                if chunks
                    .last()
                    .is_some_and(|chunk: &UpdateChunk| chunk.end_of_file)
                {
                    if update_line.is_empty() {
                        index += 1;
                        continue;
                    }
                    if update_line != "@@" && !update_line.starts_with("@@ ") {
                        bail!(
                            "Expected update hunk to start with a @@ context marker at line {}",
                            index + 1
                        );
                    }
                }

                if chunks.is_empty()
                    && move_path.is_none()
                    && let Some(destination) = update_line.strip_prefix(MOVE)
                {
                    move_path = Some(PathBuf::from(destination));
                    index += 1;
                    continue;
                }

                if update_line == "@@" || update_line.starts_with("@@ ") {
                    ensure_last_chunk_not_empty(&chunks, index + 1)?;
                    chunks.push(UpdateChunk {
                        context: update_line.strip_prefix("@@ ").map(str::to_string),
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        end_of_file: false,
                    });
                    index += 1;
                    continue;
                }

                if update_line == EOF {
                    ensure_last_chunk_not_empty(&chunks, index + 1)?;
                    let Some(chunk) = chunks.last_mut() else {
                        bail!("Update hunk does not contain any lines");
                    };
                    chunk.end_of_file = true;
                    index += 1;
                    continue;
                }

                if chunks.is_empty() {
                    chunks.push(UpdateChunk {
                        context: None,
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        end_of_file: false,
                    });
                }
                let chunk = chunks.last_mut().expect("chunk was inserted above");
                if line.is_empty() {
                    chunk.old_lines.push(String::new());
                    chunk.new_lines.push(String::new());
                } else if let Some(content) = line.strip_prefix(' ') {
                    chunk.old_lines.push(content.to_string());
                    chunk.new_lines.push(content.to_string());
                } else if let Some(content) = line.strip_prefix('+') {
                    chunk.new_lines.push(content.to_string());
                } else if let Some(content) = line.strip_prefix('-') {
                    chunk.old_lines.push(content.to_string());
                } else {
                    bail!(
                        "Invalid update line {}: expected ' ', '+' or '-'",
                        index + 1
                    );
                }
                index += 1;
            }

            if chunks.is_empty() {
                bail!(
                    "Update file hunk for path '{}' is empty at line {hunk_line}",
                    path
                );
            }
            ensure_last_chunk_not_empty(&chunks, index + 1)?;
            hunks.push(Hunk::Update {
                path: PathBuf::from(path),
                move_path,
                chunks,
            });
            continue;
        }

        bail!("Unknown patch header at line {}: {header}", index + 1);
    }

    Ok(hunks)
}

fn patch_lines<'a>(lines: &'a [&'a str]) -> Result<&'a [&'a str]> {
    if has_boundaries(lines) {
        return Ok(lines);
    }
    if let [first, .., last] = lines
        && matches!(*first, "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
        && last.ends_with("EOF")
        && lines.len() >= 4
    {
        let inner = &lines[1..lines.len() - 1];
        if has_boundaries(inner) {
            return Ok(inner);
        }
    }
    if lines.first().map(|line| line.trim()) != Some(BEGIN) {
        bail!("The first line of the patch must be '{BEGIN}'");
    }
    bail!("The last line of the patch must be '{END}'")
}

fn has_boundaries(lines: &[&str]) -> bool {
    lines.first().is_some_and(|line| line.trim() == BEGIN)
        && lines.last().is_some_and(|line| line.trim() == END)
}

fn is_trimmed_header(line: &str) -> bool {
    let line = line.trim();
    line == END
        || line.starts_with(ADD)
        || line.starts_with(DELETE)
        || line.starts_with(UPDATE)
        || line.starts_with(REPLACE)
}

fn is_update_header(line: &str) -> bool {
    line == END
        || line.starts_with(ADD)
        || line.starts_with(DELETE)
        || line.starts_with(UPDATE)
        || line.starts_with(REPLACE)
}

fn ensure_last_chunk_not_empty(chunks: &[UpdateChunk], line_number: usize) -> Result<()> {
    if chunks
        .last()
        .is_some_and(|chunk| chunk.old_lines.is_empty() && chunk.new_lines.is_empty())
    {
        bail!("Update hunk does not contain any lines at line {line_number}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_empty_add_file_and_padded_headers() {
        let hunks = parse_patch(
            "  *** Begin Patch\n  *** Add File: empty.txt\n  *** Delete File: old.txt\n*** End Patch  ",
        )
        .unwrap();
        assert_eq!(hunks.len(), 2);
        assert!(matches!(&hunks[0], Hunk::Add { contents, .. } if contents.is_empty()));
    }

    #[test]
    fn bare_empty_update_line_is_context() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Update File: a\n@@\n one\n\n-two\n+TWO\n*** End Patch",
        )
        .unwrap();
        let Hunk::Update { chunks, .. } = &hunks[0] else {
            panic!("expected update hunk");
        };
        assert_eq!(chunks[0].old_lines, vec!["one", "", "two"]);
        assert_eq!(chunks[0].new_lines, vec!["one", "", "TWO"]);
    }

    #[test]
    fn accepts_lenient_heredoc_wrapper() {
        let hunks =
            parse_patch("<<'EOF'\n*** Begin Patch\n*** Add File: a\n+x\n*** End Patch\nEOF")
                .unwrap();
        assert_eq!(hunks.len(), 1);
    }

    #[test]
    fn parses_replace_file_with_expected_count() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Replace File: a.txt\n*** Expected: 2\n@@\n-old\n-line\n+new\n+line\n*** End Patch",
        )
        .unwrap();
        assert_eq!(
            hunks,
            vec![Hunk::Replace {
                path: PathBuf::from("a.txt"),
                expected: 2,
                old: "old\nline".to_string(),
                new: "new\nline".to_string(),
            }]
        );
    }

    #[test]
    fn replace_file_requires_a_positive_expected_count() {
        let error = parse_patch(
            "*** Begin Patch\n*** Replace File: a.txt\n*** Expected: 0\n@@\n-old\n+new\n*** End Patch",
        )
        .unwrap_err();
        assert!(error.to_string().contains("must be at least 1"));
    }

    #[test]
    fn update_context_that_looks_like_replace_header_remains_context() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n *** Replace File: literal text\n-old\n+new\n*** End Patch",
        )
        .unwrap();
        let Hunk::Update { chunks, .. } = &hunks[0] else {
            panic!("expected update hunk");
        };
        assert_eq!(
            chunks[0].old_lines,
            vec!["*** Replace File: literal text", "old"]
        );
    }
}

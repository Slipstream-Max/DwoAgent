//! ACP transcript replay.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use agent_client_protocol::{Client, ConnectionTo};
use anyhow::{Context as AnyhowContext, Result};

use super::notifications::emit_session_update;
use crate::config::models::SessionTranscriptEvent;

pub fn replay_transcript_file(
    cx: &ConnectionTo<Client>,
    session_id: &str,
    path: &Path,
) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("open {}", path.display())),
    };

    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let event: SessionTranscriptEvent = serde_json::from_str(text)
            .with_context(|| format!("parse transcript event in {}", path.display()))?;
        emit_session_update(cx, session_id, &event.update);
    }
    Ok(())
}

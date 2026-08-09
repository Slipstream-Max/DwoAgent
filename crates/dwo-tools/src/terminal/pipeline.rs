#[derive(Debug, Clone, Copy, Default)]
enum AnsiState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
}

#[derive(Debug, Default)]
pub(crate) struct OutputPipeline {
    ansi: AnsiStripper,
    utf8_pending: Vec<u8>,
    has_visible_output: bool,
}

impl OutputPipeline {
    pub fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        let clean = self.ansi.strip(bytes);
        self.utf8_pending.extend(clean);
        let complete = complete_utf8_prefix_len(&self.utf8_pending);
        if complete == 0 {
            return Vec::new();
        }
        let output = self.utf8_pending.drain(..complete).collect::<Vec<_>>();
        if !self.has_visible_output && output.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Vec::new();
        }
        if output.iter().any(|byte| !byte.is_ascii_whitespace()) {
            self.has_visible_output = true;
        }
        output
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = self.ansi.flush();
        output.extend(self.utf8_pending.drain(..));
        if output.is_empty() {
            return output;
        }
        if !self.has_visible_output && output.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Vec::new();
        }
        self.has_visible_output = true;
        output
    }
}

#[derive(Debug, Default)]
struct AnsiStripper {
    state: AnsiState,
}

impl AnsiStripper {
    fn strip(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut clean = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            self.state = match self.state {
                AnsiState::Ground if byte == 0x1b => AnsiState::Escape,
                AnsiState::Ground => {
                    clean.push(byte);
                    AnsiState::Ground
                }
                AnsiState::Escape => escape_state(byte),
                AnsiState::EscapeIntermediate if (0x20..=0x2f).contains(&byte) => {
                    AnsiState::EscapeIntermediate
                }
                AnsiState::EscapeIntermediate => AnsiState::Ground,
                AnsiState::Csi if byte == 0x1b => AnsiState::Escape,
                AnsiState::Csi if (0x40..=0x7e).contains(&byte) => AnsiState::Ground,
                AnsiState::Csi => AnsiState::Csi,
                AnsiState::Osc if byte == 0x07 => AnsiState::Ground,
                AnsiState::Osc if byte == 0x1b => AnsiState::OscEscape,
                AnsiState::Osc => AnsiState::Osc,
                AnsiState::OscEscape if byte == b'\\' => AnsiState::Ground,
                AnsiState::OscEscape => escape_state(byte),
            };
        }
        clean
    }

    fn flush(&mut self) -> Vec<u8> {
        self.state = AnsiState::Ground;
        Vec::new()
    }
}

fn escape_state(byte: u8) -> AnsiState {
    match byte {
        b'[' => AnsiState::Csi,
        b']' => AnsiState::Osc,
        0x20..=0x2f => AnsiState::EscapeIntermediate,
        _ => AnsiState::Ground,
    }
}

fn complete_utf8_prefix_len(bytes: &[u8]) -> usize {
    let mut checked = 0;
    while checked < bytes.len() {
        match std::str::from_utf8(&bytes[checked..]) {
            Ok(_) => return bytes.len(),
            Err(error) => {
                let invalid_at = checked + error.valid_up_to();
                let Some(error_len) = error.error_len() else {
                    return invalid_at;
                };
                checked = invalid_at + error_len;
            }
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_utf8_split_across_chunks() {
        let mut pipeline = OutputPipeline::default();
        assert_eq!(pipeline.process(&[0xe4, 0xb8]), b"");
        assert_eq!(pipeline.process(&[0xad]), "中".as_bytes());
    }

    #[test]
    fn strips_ansi_split_across_chunks() {
        let mut pipeline = OutputPipeline::default();
        let mut output = pipeline.process(b"hello\x1b[3");
        output.extend(pipeline.process(b"1mred\x1b[0m"));
        assert_eq!(output, b"hellored");
    }

    #[test]
    fn drops_only_initial_whitespace() {
        let mut pipeline = OutputPipeline::default();
        assert!(pipeline.process(b"\r\n").is_empty());
        assert_eq!(pipeline.process(b"first"), b"first");
        assert_eq!(pipeline.process(b"\r\n"), b"\r\n");
    }
}

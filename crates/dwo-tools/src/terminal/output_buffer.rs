use std::collections::VecDeque;

const DEFAULT_HARD_CAP_BYTES: usize = 1024 * 1024;
const DEFAULT_MODEL_CAP_BYTES: usize = 20_000;
const OMITTED_MARKER: &str = "\n... output omitted ...\n";

#[derive(Debug)]
pub struct OutputBuffer {
    bytes: VecDeque<u8>,
    base_offset: u64,
    read_offset: u64,
    hard_cap_bytes: usize,
    model_cap_bytes: usize,
}

impl Default for OutputBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_HARD_CAP_BYTES, DEFAULT_MODEL_CAP_BYTES)
    }
}

impl OutputBuffer {
    pub fn new(hard_cap_bytes: usize, model_cap_bytes: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            base_offset: 0,
            read_offset: 0,
            hard_cap_bytes: hard_cap_bytes.max(1),
            model_cap_bytes: model_cap_bytes.max(128),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let mut drain = self.bytes.len().saturating_sub(self.hard_cap_bytes);
        while drain < self.bytes.len() && is_utf8_continuation(self.bytes[drain]) {
            drain += 1;
        }
        if drain > 0 {
            self.bytes.drain(..drain);
            self.base_offset += drain as u64;
            self.read_offset = self.read_offset.max(self.base_offset);
        }
    }

    pub fn take_unread(&mut self) -> String {
        self.take_unread_inner(false)
    }

    /// Consume only the complete UTF-8 prefix while the producer is still
    /// running. A split code point remains in the buffer for the next read.
    pub fn take_complete_unread(&mut self) -> String {
        self.take_unread_inner(true)
    }

    fn take_unread_inner(&mut self, preserve_incomplete_utf8: bool) -> String {
        let start_offset = self.read_offset.max(self.base_offset);
        let start = (start_offset - self.base_offset) as usize;
        let unread: Vec<u8> = self.bytes.iter().skip(start).copied().collect();
        let consume = if preserve_incomplete_utf8 {
            complete_utf8_prefix_len(&unread)
        } else {
            unread.len()
        };
        if consume == 0 {
            return String::new();
        }
        self.read_offset = start_offset + consume as u64;
        render_capped(&unread[..consume], self.model_cap_bytes)
    }

    pub fn has_unread(&self) -> bool {
        self.read_offset < self.base_offset + self.bytes.len() as u64
    }

    pub fn has_complete_unread(&self) -> bool {
        let start_offset = self.read_offset.max(self.base_offset);
        let start = (start_offset - self.base_offset) as usize;
        let unread: Vec<u8> = self.bytes.iter().skip(start).copied().collect();
        complete_utf8_prefix_len(&unread) > 0
    }

    pub fn render_all(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        render_capped(&bytes, self.model_cap_bytes)
    }
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// Return the length that can be decoded without waiting for a split trailing
/// UTF-8 code point. Invalid bytes are considered consumable input; only an
/// incomplete valid sequence is retained for the next chunk.
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

fn render_capped(bytes: &[u8], cap: usize) -> String {
    cap_valid_utf8(String::from_utf8_lossy(bytes).into_owned(), cap)
}

fn cap_valid_utf8(rendered: String, cap: usize) -> String {
    if rendered.len() <= cap {
        return rendered;
    }
    let marker = OMITTED_MARKER;
    let available = cap.saturating_sub(marker.len());
    let mut head_end = available / 2;
    while head_end > 0 && !rendered.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = rendered.len().saturating_sub(available - head_end);
    while tail_start < rendered.len() && !rendered.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}{}{}",
        &rendered[..head_end],
        marker,
        &rendered[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_is_incremental() {
        let mut buffer = OutputBuffer::new(100, 100);
        buffer.push(b"one");
        assert_eq!(buffer.take_unread(), "one");
        assert_eq!(buffer.take_unread(), "");
        buffer.push(b"two");
        assert_eq!(buffer.take_unread(), "two");
    }

    #[test]
    fn large_output_keeps_head_and_tail() {
        let mut buffer = OutputBuffer::new(1000, 128);
        buffer.push(format!("HEAD{}TAIL", "x".repeat(500)).as_bytes());
        let output = buffer.take_unread();
        assert!(output.starts_with("HEAD"));
        assert!(output.ends_with("TAIL"));
        assert!(output.contains("output omitted"));
        assert!(output.len() <= 128);
    }

    #[test]
    fn hard_cap_advances_unread_cursor() {
        let mut buffer = OutputBuffer::new(8, 128);
        buffer.push(b"12345678");
        buffer.push(b"90");
        assert_eq!(buffer.take_unread(), "34567890");
    }

    #[test]
    fn complete_reads_preserve_split_utf8() {
        let mut buffer = OutputBuffer::new(100, 128);
        buffer.push(&[0xe4, 0xb8]);
        assert!(!buffer.has_complete_unread());
        assert_eq!(buffer.take_complete_unread(), "");
        buffer.push(&[0xad]);
        assert!(buffer.has_complete_unread());
        assert_eq!(buffer.take_complete_unread(), "中");
    }

    #[test]
    fn invalid_bytes_do_not_consume_a_split_character_after_them() {
        let mut buffer = OutputBuffer::new(100, 128);
        buffer.push(&[0xff, 0xe4, 0xb8]);
        assert_eq!(buffer.take_complete_unread(), "�");
        buffer.push(&[0xad]);
        assert_eq!(buffer.take_complete_unread(), "中");
    }

    #[test]
    fn final_read_flushes_incomplete_utf8() {
        let mut buffer = OutputBuffer::new(100, 128);
        buffer.push(&[0xe4, 0xb8]);
        assert_eq!(buffer.take_complete_unread(), "");
        assert_eq!(buffer.take_unread(), "�");
    }

    #[test]
    fn unicode_truncation_stays_on_character_boundaries() {
        let mut buffer = OutputBuffer::new(40_000, 128);
        buffer.push(format!("开{}结", "中".repeat(100)).as_bytes());
        let output = buffer.take_unread();
        assert!(output.starts_with("开"));
        assert!(output.ends_with("结"));
        assert!(output.contains("output omitted"));
        assert!(!output.contains('�'));
        assert!(output.len() <= 128);
    }

    #[test]
    fn hard_cap_does_not_start_inside_a_utf8_character() {
        let mut buffer = OutputBuffer::new(5, 128);
        buffer.push("ab中cd".as_bytes());
        assert_eq!(buffer.take_unread(), "中cd");
    }

    #[test]
    fn lossy_utf8_stays_inside_model_cap() {
        let mut buffer = OutputBuffer::new(40_000, 20_000);
        buffer.push(&vec![0xFF; 20_000]);
        let output = buffer.take_unread();
        assert!(output.len() <= 20_000);
        assert!(output.contains("output omitted"));
    }
}

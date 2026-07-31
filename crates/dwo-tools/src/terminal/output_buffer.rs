use std::collections::VecDeque;

const DEFAULT_HARD_CAP_BYTES: usize = 1024 * 1024;
pub(crate) const DEFAULT_MODEL_CAP_BYTES: usize = 20_000;
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
        let overflow = self.bytes.len().saturating_sub(self.hard_cap_bytes);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.base_offset += overflow as u64;
            self.read_offset = self.read_offset.max(self.base_offset);
        }
    }

    pub fn take_unread(&mut self) -> String {
        let end_offset = self.base_offset + self.bytes.len() as u64;
        let start_offset = self.read_offset.max(self.base_offset);
        let start = (start_offset - self.base_offset) as usize;
        let unread: Vec<u8> = self.bytes.iter().skip(start).copied().collect();
        self.read_offset = end_offset;
        render_capped(&unread, self.model_cap_bytes)
    }

    pub fn has_unread(&self) -> bool {
        self.read_offset < self.base_offset + self.bytes.len() as u64
    }

    pub fn render_all(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        render_capped(&bytes, self.model_cap_bytes)
    }
}

pub(crate) fn render_capped(bytes: &[u8], cap: usize) -> String {
    if bytes.len() <= cap {
        return cap_valid_utf8(String::from_utf8_lossy(bytes).into_owned(), cap);
    }
    let marker = OMITTED_MARKER.as_bytes();
    let available = cap.saturating_sub(marker.len());
    let head_len = available / 2;
    let tail_len = available.saturating_sub(head_len);
    let mut rendered = Vec::with_capacity(cap);
    rendered.extend_from_slice(&bytes[..head_len]);
    rendered.extend_from_slice(marker);
    rendered.extend_from_slice(&bytes[bytes.len() - tail_len..]);
    cap_valid_utf8(String::from_utf8_lossy(&rendered).into_owned(), cap)
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
    fn lossy_utf8_stays_inside_model_cap() {
        let mut buffer = OutputBuffer::new(40_000, 20_000);
        buffer.push(&vec![0xFF; 20_000]);
        let output = buffer.take_unread();
        assert!(output.len() <= 20_000);
        assert!(output.contains("output omitted"));
    }
}

# Read File

- Use `read_file` to inspect UTF-8 text files and PNG, JPEG, GIF, or WebP images.
- Pass `{"path":"src/main.rs"}` to start at line 1. Text output is contiguous and never exceeds 20000 UTF-8 bytes across the entire result.
- `cursor` is the 1-based starting line, `offset` is the 0-based Unicode character position within that line, and `line_count` limits how many lines are read per call from 1 through 500.
- Text results contain `content`, `start_line`, `start_offset`, `end_line`, `end_offset`, and `total_lines`.
- If more text remains, the result also contains `next_cursor` and `next_offset`. Pass both values back unchanged on the next `read_file` call. This works for ordinary line paging and for a single line longer than the byte limit.
- Do not guess a continuation position from `end_line` or from the content length; only use `next_cursor` and `next_offset` when they are present.
- Image results only report completion. When the selected model supports image input, the tool adds supported image data directly to model context; otherwise the call returns an error. Do not use terminal commands to encode or print it.

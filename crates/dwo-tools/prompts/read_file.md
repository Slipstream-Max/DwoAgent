# Read File

- Use `read_file` to inspect UTF-8 text files and PNG, JPEG, GIF, or WebP images.
- Pass `{"path":"src/main.rs"}` for the first text page. A page contains at most 500 lines.
- When `total_lines` is present and greater than `end_line`, continue with `cursor` set to `end_line + 1`, for example `{"path":"src/main.rs","cursor":501}`.
- Text results contain `content`, `start_line`, and `end_line`. `total_lines` appears only when the file exceeds 500 lines. There is no `line_count` or `next_cursor` field.
- Image results only report completion. When the selected model supports image input, the tool adds supported image data directly to model context; otherwise the call returns an error. Do not use terminal commands to encode or print it.

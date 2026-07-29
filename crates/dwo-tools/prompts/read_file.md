# Read File

- Use `read_file` to inspect UTF-8 text files and PNG, JPEG, GIF, or WebP images.
- Pass `{"path":"src/main.rs"}` for the first text page. It returns up to 500 lines by default.
- Use `line_count` to request a smaller range, for example `{"path":"src/main.rs","cursor":120,"line_count":40}` reads lines 120 through 159. `line_count` must be between 1 and 500.
- When `total_lines` is present and greater than `end_line`, continue with `cursor` set to `end_line + 1`. Keep or change `line_count` as needed.
- Text results contain `content`, `start_line`, and `end_line`. `total_lines` appears when the file exceeds the requested page size. There is no `next_cursor` field.
- Image results only report completion. When the selected model supports image input, the tool adds supported image data directly to model context; otherwise the call returns an error. Do not use terminal commands to encode or print it.

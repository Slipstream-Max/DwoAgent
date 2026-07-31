# Read File

- Use `read_file` to inspect UTF-8 text files and PNG, JPEG, GIF, or WebP images.
- Pass `{"path":"src/main.rs"}` for the first text page. It returns up to 500 lines by default.
- Use `line_count` to request a smaller range, for example `{"path":"src/main.rs","cursor":120,"line_count":40}` reads lines 120 through 159. `line_count` must be between 1 and 500.
- When `total_lines` is present and greater than `end_line`, continue with `cursor` set to `end_line + 1`. Keep or change `line_count` as needed.
- Text results contain `content`, `start_line`, and `end_line`. `total_lines` appears when the file exceeds the requested page size. There is no `next_cursor` field.
- A line longer than 20000 bytes is truncated keeping head and tail, matching terminal output truncation. Truncated lines are listed in `truncated_lines` as `[{"line":3844,"chars":610567}]` so you know how much was omitted.
- Page through a long line with `offset` (0-based character offset) plus `line_count` 1, for example `{"path":"big.html","cursor":3844,"line_count":1,"offset":300000}` reads line 3844 starting at character 300000. The result reports `offset`, `line_chars`, and `remaining_chars`; continue with a larger `offset` until `remaining_chars` is 0.
- Image results only report completion. When the selected model supports image input, the tool adds supported image data directly to model context; otherwise the call returns an error. Do not use terminal commands to encode or print it.

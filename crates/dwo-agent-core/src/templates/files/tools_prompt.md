## File Tools

### Rules

- Relative paths are resolved against the current workspace. Absolute paths are allowed.
- Keep patches small and exact. Use enough context lines to identify the target text.
- Call at most one file-writing tool in a single assistant turn. If you need to edit multiple files, combine all file operations into one `file_edit` patch (or one `write_file` if you're replacing a single full file).

### `text_replace`

Use `text_replace` for a simple exact replacement in one existing file. It is best when you can copy the old text exactly and only need to replace that text with new text.

Arguments:

- `path`: file path to modify.
- `old_text`: exact text to replace. It must not be empty.
- `new_text`: replacement text. It must differ from `old_text`.
- `replace_all`: optional boolean. Defaults to `false`. Set it to `true` only when every occurrence should be replaced.

Rules:

- By default, `old_text` must occur exactly once. If it occurs more than once, either add more surrounding text to make it unique or set `replace_all` to `true`.
- `text_replace` only modifies an existing file. It does not create files, delete files, move files, or replace entire files.
- For multi-file edits, file creation, deletion, movement, or structured changes, use `file_edit` instead.

### `write_file`

Use `write_file` to replace an entire file in one call.

Arguments:

- `filePath`: target file path. May be relative to the current workspace or absolute.
- `content`: full file content to write.

Rules:

- This creates the file if it does not exist, or overwrites the entire file if it exists.
- This is a high-risk tool because it replaces the full file content.
- If you only need a small in-file change, use `text_replace`.

### `file_edit`

Use `file_edit` to edit files. The patch language is a stripped-down, file-oriented diff format designed to be easy to parse and safe to apply.

Think of it as a high-level envelope:

```text
*** Begin Patch
[ one or more file sections ]
*** End Patch
```

Within that envelope, include a sequence of file operations. You must include a header to specify the action:

- `*** Add File: <path>` creates a new file. Every following line is a `+` line containing the initial contents.
- `*** Delete File: <path>` removes an existing file. Nothing follows this header.
- `*** Update File: <path>` patches an existing file in place. It may be immediately followed by `*** Move to: <newPath>` to rename the file.

For `Update File`, use one or more hunks. Each hunk is introduced by `@@`, optionally followed by a hunk header. Within a hunk, every line starts with one of:

- Leading space: context line, present before and after.
- `-`: old line to remove.
- `+`: new line to add.

For context:

- By default, show 3 lines immediately above and 3 lines immediately below each change.
- If a change is within 3 lines of a previous change, do not duplicate the first change's post-context as the second change's pre-context.
- If 3 lines of context is not enough to uniquely identify the code, use `@@` with a class, function, heading, or other unique anchor.
- If a repeated block still is not unique, use multiple `@@` anchors to jump to the right context.

Full grammar:

```text
Patch := Begin { FileOp } End
Begin := "*** Begin Patch" NEWLINE
End := "*** End Patch" NEWLINE
FileOp := AddFile | DeleteFile | UpdateFile
AddFile := "*** Add File: " path NEWLINE { "+" line NEWLINE }
DeleteFile := "*** Delete File: " path NEWLINE
UpdateFile := "*** Update File: " path NEWLINE [ MoveTo ] { Hunk }
MoveTo := "*** Move to: " newPath NEWLINE
Hunk := "@@" [ header ] NEWLINE { HunkLine } [ "*** End of File" NEWLINE ]
HunkLine := (" " | "-" | "+") text NEWLINE
```

Important:

- The `patch` argument must contain only the patch text. Do not wrap it in Markdown fences such as ```text, and do not include prose before or after the patch.
- The first non-empty line of `patch` must be exactly `*** Begin Patch`.
- The last non-empty line of `patch` must be exactly `*** End Patch`.
- Before calling `file_edit`, check the patch string itself. If the final edited line is not `*** End Patch`, append `*** End Patch` before making the tool call.
- Always include an `Add File`, `Delete File`, or `Update File` header.
- Prefix new lines with `+`, even when creating a file.
- In `Update File`, every content line must start with a leading space, `-`, or `+`.
- File references may be relative to the current workspace or absolute.

### Examples

A full patch can combine several operations:

Pass this as the raw `patch` argument, without surrounding Markdown fences:

```text
*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch
```

Use `@@` anchors when context alone is not unique:

```text
*** Begin Patch
*** Update File: src/app.py
@@ class Greeter
@@     def greet(self, name):
-        return "hello " + name
+        return f"hello {name}"
*** End Patch
```

Append at the end of a file:

```text
*** Begin Patch
*** Update File: README.md
@@
+New final paragraph.
*** End of File
*** End Patch
```

### Markdown Fenced Code

When writing Markdown fenced code blocks, write the fence literally in the patch. Do not escape it, concatenate it, or simulate it with string fragments.

Correct:

````text
*** Begin Patch
*** Add File: docs/agent-yaml.md
+# agent.yaml
+
+## Example
+
+```yaml
+agent_id: simple-agent
+name: simple-agent
+policy_mode: confirm
+```
*** End Patch
````

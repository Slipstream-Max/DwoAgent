## File Tools

### Rules

- Relative paths are resolved against the current workspace. Absolute paths are allowed.
- Use `file_edit` for all file writes.
- Call at most one file-writing tool in a single assistant turn. If you need to edit multiple files, combine all file operations into one `file_edit` patch.
- Keep patches small and exact. Use enough context lines to identify update targets.

### `file_edit`

Use `file_edit` to edit files. The patch language is a stripped-down, file-oriented diff format designed to be easy to parse and safe to apply.

Think of it as a high-level envelope:

```text
*** Begin Patch
[ one or more file sections ]
*** End Patch
```

Within that envelope, include a sequence of file operations. You must include a header to specify the action:

- `*** Write File: <path>` creates or overwrites a whole file. Every following content line must start with `+`.
- `*** Replace All: <path>` replaces every exact occurrence of old text in an existing file. Put old text on `-` lines, then replacement text on `+` lines.
- `*** Delete File: <path>` removes an existing file. Nothing follows this header.
- `*** Update File: <path>` patches an existing file in place. It may be immediately followed by `*** Move to: <newPath>` to rename the file.

For `Replace All`:

- It is exact string replacement, not regex.
- It can replace a word, a line fragment, a whole line, or multiple consecutive lines.
- The old text must not be empty and must exist at least once.
- Every occurrence is replaced. Use `Update File` when only one occurrence should change.
- `-` old-text lines must come before `+` replacement-text lines.

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
FileOp := WriteFile | ReplaceAll | DeleteFile | UpdateFile
WriteFile := "*** Write File: " path NEWLINE { "+" line NEWLINE }
ReplaceAll := "*** Replace All: " path NEWLINE { "-" oldLine NEWLINE } { "+" newLine NEWLINE }
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
- Always include a `Write File`, `Replace All`, `Delete File`, or `Update File` header.
- Prefix `Write File` content lines with `+`.
- In `Replace All`, every content line must start with `-` or `+`.
- In `Update File`, every content line must start with a leading space, `-`, or `+`.
- File references may be relative to the current workspace or absolute.

### Examples

A full patch can combine several operations:

Pass this as the raw `patch` argument, without surrounding Markdown fences:

```text
*** Begin Patch
*** Write File: hello.txt
+Hello world
*** Replace All: src/app.py
-old_name
+new_name
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
*** Write File: docs/agent-yaml.md
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

---
name: file_edit
description: Apply structured patches that add, update, move, or delete UTF-8 text files.
---

# Use Cases

Use `file_edit` for deliberate source-code, configuration, documentation, and other text-file changes. A single patch may contain operations for multiple files.

# Input

`file_edit` accepts one `patch` string. Every patch begins with `*** Begin Patch`, ends with `*** End Patch`, and contains one or more file operations.

```text
*** Begin Patch
... file operations ...
*** End Patch
```

# Atomic Operations

## Add File

Write a file, replacing an existing file at the same path. Every content line starts with `+`. An Add File operation with no content lines creates an empty file.

```text
*** Add File: path/to/new.txt
+first line
+second line
```

## Update File

Modify an existing file using one or more ordered hunks.

Complete example without an anchor:

```text
*** Begin Patch
*** Update File: path/to/existing.txt
@@
 fn greet() {
-    println!("hello");
+    println!("hello, world");
 }
*** End Patch
```

Complete example using an anchor:

```text
*** Begin Patch
*** Update File: path/to/existing.txt
@@ impl Greeter {
     fn greet(&self) {
-        println!("hello");
+        println!("hello, world");
     }
*** End Patch
```

`@@ impl Greeter {` finds that anchor and starts matching on the line after it.
Do not repeat the anchor as a context or removed line below the `@@` header. In
the example above, there is intentionally no ` impl Greeter {` line after the
header.

Within an update hunk:

- A space preserves a context line.
- `-` removes a line.
- `+` adds a line.
- `@@` starts a hunk.
- `@@ context` finds an anchor, then matches the hunk after that anchor. Do not repeat the anchor in the hunk body.
- Blank context lines may be completely empty or contain a single leading space. Use `+` or `-` explicitly when adding or removing a blank line.
- `*** End of File` requires the hunk to match at the end of the file.

## Move File

Move an updated file by placing `*** Move to:` immediately after its update header. An existing destination is replaced.

```text
*** Update File: old/path.txt
*** Move to: new/path.txt
@@
-old content
+new content
```

## Delete File

Delete an existing file.

```text
*** Delete File: path/to/obsolete.txt
```

# Results

A successful result lists each changed path and whether it was added, updated, moved, or deleted. A parse error, unmatched hunk, invalid target, or filesystem error returns an error result for this tool call.

# Notes

- Prefer paths relative to the session working directory.
- Add targets replace existing files. Update and delete targets must exist.
- Files being updated must contain valid UTF-8 text.
- Updates follow Codex apply-patch text behavior and normally finish with a final newline.
- Hunk matching tries exact text first, then tolerates trailing whitespace, surrounding whitespace, and common Unicode punctuation differences.
- File operations are applied in patch order. If a later operation fails, earlier successful operations remain applied.
- Keep related multi-file changes in one patch when they form one coherent edit.
- Only one `file_edit` call is allowed per assistant response. Combine coherent multi-file changes into one patch; issue unrelated or follow-up patches in later responses.
- File edits from different sessions are serialized by the shared file-edit service.

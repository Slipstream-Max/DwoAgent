# Tool Rules

- Tool calls in one assistant response are independent work items. Do not make one call depend on the output or side effects of another call in the same response.
- Multiple `terminal` calls may execute concurrently. Operations targeting the same terminal are serialized.
- At most one `file_edit` call is allowed in one assistant response. Put related multi-file changes into that call's single patch. If separate patches are necessary, issue later `file_edit` calls in subsequent assistant responses.
- `terminal` and `file_edit` may execute concurrently. If a terminal command needs files produced by `file_edit`, issue the command in a later assistant response.
- A failure in one tool call does not stop the other calls in the batch. Tool results are returned in the original call order, not completion order.
- Use `file_edit` for deliberate source-code and text-file changes. Use `terminal` for execution, inspection, and workflows where a program legitimately generates files.
- Tool text is returned as UTF-8. Terminal output is retained internally as bytes and decoded with lossy UTF-8 only when rendered for the model.
- Terminal results contain incremental unread output. Large output preserves its beginning and end while omitting the middle.
- Every tool call is subject to the active policy. Do not use another tool or command form to bypass a policy denial.

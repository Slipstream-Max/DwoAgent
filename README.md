# Dwo Agent

Dwo Agent is a native Rust, multi-session agent runtime. One local `dwo` daemon owns the profile, model clients, sessions, channels, MCP connections, automation jobs, and tool runtimes. CLI, ACP, and Weixin clients all attach to that same runtime over local IPC.

## Architecture

```text
dwo daemon
|- AgentService
|  `- session actor -> model context + transcript + session config
|- provider-configured model clients
|- terminal and file-edit tools
|- managed MCP runtime
|- Weixin and Telegram channel runtimes
`- automation scheduler

clients
|- dwo CLI
|- dwo acp
`- channel endpoints
```

The workspace is split into focused crates:

```text
crates/dwo-agent          daemon composition, CLI, ACP, channels, automation
crates/dwo-agent-service  session actors, persistence, model/tool loop
crates/dwo-context        model context, environment watcher, compaction
crates/dwo-model-client   provider catalog and HTTP model transport
crates/dwo-mcp            managed MCP discovery, auth, and calls
crates/dwo-tools          terminal, file editing, policy, execution
crates/dwo-pty            Windows and portable PTY support
```

## Build And Install

Rust 1.95 or newer is required.

```powershell
cargo build --release -p dwo-agent
./target/release/dwo.exe install --start
./target/release/dwo.exe daemon status
```

`install` creates the default profile at `~/.dwoagent/profile.yaml` and registers user-login startup. Use global `--config-path <path>` to operate another profile.

## Commands

The complete command and behavior reference is [docs/commands.md](docs/commands.md). The primary workflows are:

```text
dwo daemon start|stop|status
dwo session list|new|delete|prompt|cancel|watch|model|reasoning
dwo channel weixin status|bind|unbind|send-message|send-file
dwo channel telegram status|bind|unbind|send-message|send-file
dwo mcp search <query>
dwo mcp call <server.tool> --args '<json>'
dwo mcp auth <server> [--logout]
dwo automation list|status|run
dwo acp
```

Human-facing CLI output is readable YAML-style text. `session watch` is a continuous stream of reasoning, tool calls/results, answers, and terminal state; use `Ctrl+C` to stop watching. JSON is retained only where it is an explicit machine payload, such as MCP `--args` or automation `--json`.

## Profile And Persistence

The default layout is:

```text
~/.dwoagent/
|- profile.yaml
|- resource/
|  |- prompts/System.md
|  |- prompts/AGENTS.md
|  |- skills/
|  `- mcp.json
|- runtime/
|  |- sessions/YYYY/MM/DD/<session-id>/
|  |  |- session.json
|  |  |- model_context.json
|  |  `- client_transcript.jsonl
|  |- workspaces/
|  |- attachments/
|  |- mcp/catalog.json
|  |- mcp/oauth/
|  `- logs/
`- channels/
   |- weixin/
   |  |- runtime.yaml
   |  `- secret.yaml
   `- telegram/
      |- runtime.yaml
      `- secret.yaml
```

`model_context.json` is the current provider-facing context. `client_transcript.jsonl` is an append-only, complete replay stream and is not shortened by model compaction. `session.json` stores identity, cwd, title, mode, and model settings.

Usage tracks the current context token count reported by the latest normal model response; it is not a cumulative input/output counter. Compaction resets it to zero until the next response. Model switches immediately publish the existing count against the target model's context window.

When switching an image-bearing session to a text-only model, Dwo first uses the current or last successful visual model to convert the images into a text summary. The target model is committed only after the image-free context is saved. The transcript keeps original images for replay, but switching back to a visual model does not restore compacted images to model context.

## MCP

MCP servers are configured in `resource/mcp.json`. Daemon startup initializes all configured servers concurrently and keeps successful stdio/HTTP sessions alive. New or changed servers are initialized by the config watcher. Runtime states are `starting`, `ready`, `auth_required`, and `failed`.

`dwo mcp search` reads the current in-memory catalog and never starts a server. A server match lists all its tools; a direct tool match additionally expands that tool's schema. `runtime/mcp/catalog.json` is a derived context/diagnostic projection, not evidence of a live connection.

## Development

```powershell
cargo fmt --all
cargo test --workspace
```

On Windows ARM64, run Rust build and test commands from a Visual Studio Developer PowerShell configured for ARM64 so native dependencies can find the C toolchain.

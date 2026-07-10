# subagent

`subagent` is a Linux-first, agent-only CLI for running persistent coding agents in
the background. Every operational response is JSONL; there is no alternate table,
color, or interactive presentation mode.

## Setup

Install the included Linux binary directly:

```sh
install -Dm755 dist/subagent "$HOME/.local/bin/subagent"
```

Or build and install it from source with a current stable Rust toolchain:

```sh
cargo build --release --locked
install -Dm755 target/release/subagent "$HOME/.local/bin/subagent"
```

Ensure `$HOME/.local/bin` is on `PATH`, then configure an OpenAI-compatible
chat-completions endpoint and start the per-user daemon:

```sh
export OPENAI_API_KEY='...'
export OPENAI_BASE_URL='https://example.com/v1'
export OPENAI_MODEL='your-model'
subagent daemon start
```

The API key is retained only by the daemon process and is removed from agent shell
environments. Non-secret settings may also be managed with `subagent config`.
Configuration changes take effect after restarting the daemon.

## Commands

```text
subagent daemon start|status|stop
subagent agents spawn --dir PATH (--message TEXT | --message-file PATH)
subagent agents list|status|logs|context|send|side|btw|time|stop|delete
subagent config list|get|set
```

Run any command with `--help` for its exact input flags and JSON output schema.
Agent identifiers are unique `agt_<ULID>` values. A spawn can use `--mode readonly`
(the default) or `--mode write`, and `--wall-time HOURS` accepts values up to 100.
`SUBAGENT_MAX_AGENTS` or `config set max-agents N` controls concurrent working
agents; zero means unlimited.

Use `agents side` (alias `agents btw`) for a focused branch that inherits a snapshot
of the parent's full model context and workspace. Its only goal is to answer the
question. It can run while its parent is working and may read files, search with
glob or grep, execute non-mutating Bash commands, poll terminals, read stored output,
and view images. Side agents are always readonly even when the parent is in write
mode: they never receive `write`, `edit`, or `apply_patch`, and are instructed not to
change files or state through Bash. Their question, tool calls, answer, and temporary
command-output storage are not added to the parent transcript.

## Model tools

Agents can use `read`, `glob`, `grep`, `exec_command`, `write_stdin`,
`list_terminals`, `terminate_terminal`, `terminate_all_terminals`, `view_image`, and
`read_output`. Write-mode agents additionally receive `write`, `edit`, and
`apply_patch`. Each agent may own at most eight live background terminals. Complete
command output is retained on disk and can be fetched in bounded chunks with
`read_output`.

## State and security

State is stored under the user's XDG state directory as one directory per agent,
including metadata, context, event JSONL, and command outputs. Runtime IPC uses a
user-owned Unix socket.

Agents are intentionally unsandboxed. Readonly mode removes structured mutation
tools and instructs the model not to modify the workspace, but Bash remains
available and the mode is advisory—not a security boundary. Run the daemon as a
user that has access only to the projects and resources agents should reach.

## Verification

Run the unit tests directly with Cargo:

```sh
cargo test --locked
```

The end-to-end suite uses a local mock OpenAI-compatible streaming server:

```sh
cargo build --release --locked
SUBAGENT_BIN="$PWD/target/release/subagent" tests/e2e.sh
```

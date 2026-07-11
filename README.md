# subagent

**Persistent coding agents for other agents.**

`subagent` is a Linux-first CLI and per-user daemon for starting coding work in the
background, checking it later, and continuing it without losing context. Every
operational response is JSONL. There is no table, color, or interactive output mode
to parse around.

- Native host execution with one small static binary
- Persistent agent histories with stable `agt_<ULID>` identifiers
- Concurrent work across independent project directories
- OpenAI-compatible chat-completions API
- Full coding tools, including Bash and eight background terminals per agent
- Durable, readonly Side runs with saved answers and tool traces

## Install

Supported platform: **Linux x86-64** (`x86_64`/`amd64`). The release binary is
statically linked, so the same artifact runs across distributions without a system
runtime dependency.

Install the latest release without root privileges:

```sh
curl -fsSL https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh | sh
```

The installer downloads the release binary and checksum, verifies SHA-256, installs
to `$HOME/.local/bin/subagent`, and runs `subagent --version`.

To inspect the installer before running it:

```sh
curl -fsSLO https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh
less install.sh
sh install.sh
```

To use another user-owned destination:

```sh
curl -fsSL https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh |
  SUBAGENT_INSTALL_DIR="$HOME/bin" sh
```

Alternatively, clone the repository and install its included binary:

```sh
git clone https://github.com/randomvibecoder/subagent.git
cd subagent
install -Dm755 dist/subagent-linux-x86_64 "$HOME/.local/bin/subagent"
```

Ensure `$HOME/.local/bin` is on `PATH`. To build from source instead, use a current
stable Rust toolchain:

```sh
cargo build --release --locked
install -Dm755 target/release/subagent "$HOME/.local/bin/subagent"
```

No root privileges, package manager, or system service installation are required.

## Quick start

Configure an OpenAI-compatible endpoint and start the daemon:

```sh
export OPENAI_API_KEY='...'
export OPENAI_BASE_URL='https://example.com/v1'
export OPENAI_MODEL='your-model'

subagent daemon start
```

Spawn a write-enabled agent:

```sh
subagent agents spawn \
  --dir "$HOME/projects/my-app" \
  --mode write \
  --name "Fix authentication" \
  --message "Find and fix the login regression, then run the relevant tests."
```

The response contains the new ID:

```json
{"type":"agent","id":"agt_01...","status":"working"}
```

Use that ID to inspect or continue the work:

```sh
subagent agents status agt_01...
subagent agents send agt_01... --message "Also check token refresh behavior."
subagent agents logs agt_01... --follow
```

`send` returns immediately after the daemon durably stores the message:

```json
{"type":"message_sent","message_id":"msg_01...","agent_id":"agt_01...","status":"queued","sent_at":"2026-07-10T12:00:00Z"}
```

## Why a daemon?

The CLI is a thin JSONL client. A manually started, user-owned daemon holds active
workers and listens on a private Unix socket. Agent metadata, durable messages,
context, events, and complete command output are persisted on disk, so finished agents
leave memory and can be resumed later.

```text
calling agent -> subagent CLI -> Unix socket -> daemon -> model + tools
                                              |-> agent A
                                              |-> agent B
                                              `-> agent C
```

Stopping the CLI process does not stop the daemon. Use `subagent daemon stop` for an
orderly shutdown.

## Commands

| Command | Purpose |
| --- | --- |
| `daemon start` | Start the detached per-user daemon |
| `daemon status` | Report daemon PID, socket, model, and capacity |
| `daemon stop` | Stop the daemon and its working agents |
| `agents spawn` | Create and immediately start a persistent agent |
| `agents list` | Filter, sort, and paginate stored agents |
| `agents status ID` | Read one agent's current metadata |
| `agents rename ID NAME` | Change the unique display name shown by `agents list` |
| `agents logs ID` | Read the last 20 transcript Events by default, or select/follow other types |
| `agents context ID` | Dump the complete current raw model context as debugging JSONL |
| `agents send ID` | Durably queue input and immediately return a message receipt |
| `agents side ID` | Alias that starts a durable readonly Side run |
| `agents btw ID` | Alias for `agents side` |
| `sides create|list|status|logs|stop|delete` | Manage durable one-shot Side runs |
| `agents time ID MINUTES` | Reset a working agent's deadline from now |
| `agents stop ID` | Stop an agent and all of its terminal process groups |
| `agents delete ID` | Permanently delete a non-working agent history |
| `messages list|status|cancel` | Inspect or cancel durable Agent messages |
| `config list|get|set` | Manage non-secret daemon configuration |

Run any command with `--help` for its exact flags and JSON output schema.

### Spawn input

Inline and file input are mutually exclusive:

```sh
subagent agents spawn --name "Feature build" --dir /path/to/project --message "Build the feature"
subagent agents spawn --name "Repository review" --dir /path/to/project --message-file task.md
printf '%s\n' "Review this repository" | \
  subagent agents spawn --name "Repository review" --dir /path/to/project --message-file -
```

Names are mandatory, case-sensitive, unique across stored agents, and 4–40 Unicode
characters. IDs remain authoritative for every command. Agents start in `readonly`
mode unless `--mode write` is supplied. Optional `--wall-time-minutes MINUTES` values
are integers from 1 through 6000.

### List filters

`agents list` supports repeatable status filters, canonical directory filters,
spawned/finished time ranges, sorting, ordering, limits, and offsets:

```sh
subagent agents list \
  --status working \
  --dir /path/to/project \
  --sort updated_at \
  --order desc \
  --limit 20
```

Agent states are `working`, `finished`, `stopped`, and `failed`.

Every list item includes the display `name`, `spawned_at`, `last_message_at`, and
`updated_at`, so list output
distinguishes creation, latest accepted user instruction, and latest worker activity.

## Optional Web UI

Start the daemon with a localhost-only dashboard when a human wants to monitor it:

```sh
subagent daemon start --web-ui-port 7341
```

The daemon response includes a fresh tokenized `web_ui_url`. Open that exact URL in a
browser. The embedded dark-only UI binds only `127.0.0.1`, uses `#000000` for the
background, `#ffffff` for primary text, and light gray for secondary metadata. It
supports the human-facing equivalents of spawn, rename, list/status, filtered live
logs, send, message inspection/cancellation, side questions, time, stop, and confirmed
delete. The dashboard opens each agent on a dedicated routed page. Tool activity is
rendered as readable collapsed accordions instead of raw JSON; `apply_patch` calls use
a one-pane Git-style diff with red deletions and green additions. Agent pages have
Main, Side, and Controls tabs. Main opens at the newest event and loads history while
scrolling upward, using the full remaining viewport without a conversation card.
Side is a history index; every Side run opens on its own full-screen page with the
same Main/Controls layout and saved tool trace. The UI
intentionally omits config, daemon administration, and raw context.

### Logs and context

Normal logs omit tool payloads and return the newest 20 system, user, and assistant
Events in chronological JSONL order:

```sh
subagent agents logs agt_01...
```

Select exact Event types with repeatable `--type`, or use `--all`:

```sh
subagent agents logs agt_01... --type tool_call --type tool_result --limit 100
subagent agents logs agt_01... --all --follow
```

`agents context` is a raw debugging escape hatch, not normal transcript output. Never
print it unfiltered into model-visible terminal output; redirect it or use a narrow
`jq` selector:

```sh
subagent agents context agt_01... > /tmp/agent-context.jsonl
```

### Durable messages

Messages are FIFO and survive daemon failure. Poll or cancel them using stable IDs:

```sh
subagent messages list agt_01... --status pending
subagent messages status agt_01... msg_01...
subagent messages cancel agt_01... msg_01...
```

After restart, interrupted Agents with pending messages resume automatically as
capacity becomes available.

## Side questions

Side runs answer focused questions without interrupting the parent or adding anything
to its transcript:

```sh
subagent sides create agt_01... \
  --message "Which module validates refresh tokens, and why?"
```

Creation returns a side_<ULID> immediately. The Side receives a valid snapshot of the
parent's model context and working directory. It may read files, search with
`glob` or `grep`, run non-mutating Bash such as `rg`, poll its own terminals, inspect
stored output, and view images.

Side runs are always readonly, even when the parent is in write mode. They never
receive `write`, `edit`, or `apply_patch`. Their question, reasoning, tool calls,
answer, and command outputs persist until sides delete or parent deletion. At most two
may work per parent. Bash restrictions remain instruction-based, not a security
boundary.

## Model tools

All agents can receive these tools:

| Tool | Purpose |
| --- | --- |
| `read` | Read bounded UTF-8 file ranges with line numbers |
| `glob` | Find files while respecting ignore rules |
| `grep` | Search file contents with regular expressions |
| `exec_command` | Run Bash and return a terminal ID when still active |
| `write_stdin` | Write to or poll a live terminal |
| `list_terminals` | List the agent's live background terminals |
| `terminate_terminal` | Stop one terminal process group |
| `terminate_all_terminals` | Stop every terminal owned by the agent |
| `read_output` | Read bounded chunks from complete stored command output |
| `view_image` | Attach a local image to the next model request |

Write-mode agents additionally receive three mutation styles:

| Tool | Purpose |
| --- | --- |
| `write` | Create or replace a complete file |
| `edit` | Replace exact text in an existing file |
| `apply_patch` | Apply an OpenAI-style add/update/delete patch |

Each agent may own at most eight live background terminals. Terminal process groups
are cleaned up when an agent finishes, stops, fails, times out, or the daemon exits.
The API key is removed from agent shell environments.

## Configuration

Environment variables override stored configuration:

| Variable | Meaning |
| --- | --- |
| `OPENAI_API_KEY` | Required daemon credential; never written to configuration |
| `OPENAI_BASE_URL` | OpenAI-compatible API base URL |
| `OPENAI_MODEL` | Model sent to chat completions |
| `SUBAGENT_MAX_AGENTS` | Maximum simultaneously working agents; `0` is unlimited |

Non-secret settings can be persisted with the CLI:

```sh
subagent config set base-url https://example.com/v1
subagent config set model your-model
subagent config set max-agents 8
subagent config list
```

Restart the daemon after changing stored configuration.

## State layout

Defaults follow the XDG base-directory convention:

```text
~/.config/subagent/config.toml
~/.local/state/subagent/
├── daemon.log
└── agents/
    └── agt_<ULID>/
        ├── metadata.json
        ├── context.json
        ├── messages.json
        ├── events.jsonl
        └── outputs/
```

The socket uses `$XDG_RUNTIME_DIR/subagent.sock` when `XDG_RUNTIME_DIR` is set and
falls back to the private state directory. State directories are owner-only and
files are written with owner-only permissions.

## Security model

Agents execute directly with the permissions of the user running the daemon. There
is no built-in sandbox, command approval layer, or network isolation. Readonly mode
withholds structured mutation tools and tells the model not to change state, but
Bash remains available and can technically mutate the host.

Run `subagent` as a user that can access only the projects, credentials, processes,
and network resources agents should reach. Treat repository instructions and other
content an agent reads as untrusted input.

## Development

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
```

The schema contract test requires the Python `jsonschema` package.

The end-to-end suite starts a local mock OpenAI-compatible streaming server:

```sh
cargo build --release --locked
SUBAGENT_BIN="$PWD/target/release/subagent" tests/e2e.sh
```

See [`SKILL.md`](SKILL.md) for the agent-facing workflow,
[`references/protocol.md`](references/protocol.md) for exact behavior, and
[`references/cli.schema.json`](references/cli.schema.json) for JSONL schemas.

## License

Apache-2.0

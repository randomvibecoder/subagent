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
- Ephemeral, readonly `side` questions over an agent's existing context

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
  --title "Fix authentication" \
  --message "Find and fix the login regression, then run the relevant tests."
```

The response contains the new ID:

```json
{"type":"agent","id":"agt_01...","status":"working","title":"Fix authentication"}
```

Use that ID to inspect or continue the work:

```sh
subagent agents status agt_01...
subagent agents logs agt_01... --follow
subagent agents send agt_01... --message "Also check token refresh behavior."
```

## Why a daemon?

The CLI is a thin JSONL client. A manually started, user-owned daemon holds active
workers and listens on a private Unix socket. Agent metadata, context, events, and
complete command output are persisted on disk, so finished agents leave memory and
can be resumed later.

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
| `agents logs ID` | Read or follow lifecycle, model, and tool events |
| `agents context ID` | Emit bounded model-sized conversation context |
| `agents send ID` | Queue input for a working agent or resume a finished one |
| `agents side ID` | Answer a question using inherited context and readonly tools |
| `agents btw ID` | Alias for `agents side` |
| `agents time ID HOURS` | Reset a working agent's deadline from now |
| `agents stop ID` | Stop an agent and all of its terminal process groups |
| `agents delete ID` | Permanently delete a non-working agent history |
| `config list|get|set` | Manage non-secret daemon configuration |

Run any command with `--help` for its exact flags and JSON output schema.

### Spawn input

Inline and file input are mutually exclusive:

```sh
subagent agents spawn --dir /path/to/project --message "Build the feature"
subagent agents spawn --dir /path/to/project --message-file task.md
printf '%s\n' "Review this repository" | \
  subagent agents spawn --dir /path/to/project --message-file -
```

Agents start in `readonly` mode unless `--mode write` is supplied. Optional
`--wall-time HOURS` values must be greater than zero and no more than 100.

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

## Side questions

`side` (alias `btw`) answers a focused question without interrupting the parent or
adding anything to its transcript:

```sh
subagent agents side agt_01... \
  --message "Which module validates refresh tokens, and why?"
```

The side agent receives a valid snapshot of the parent's model context and working
directory. Its only goal is to answer the question. It may read files, search with
`glob` or `grep`, run non-mutating Bash such as `rg`, poll its own terminals, inspect
stored output, and view images.

Side agents are always readonly, even when the parent is in write mode. They never
receive `write`, `edit`, or `apply_patch`. Their question, reasoning, tool calls,
answer, and temporary command outputs are not persisted. Bash restrictions are
instruction-based, not a security boundary.

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

The end-to-end suite starts a local mock OpenAI-compatible streaming server:

```sh
cargo build --release --locked
SUBAGENT_BIN="$PWD/target/release/subagent" tests/e2e.sh
```

## License

Apache-2.0

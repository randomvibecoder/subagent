# subagent

`subagent` is a Linux-first, agent-only CLI for running persistent coding agents in
the background. Every operational response is JSONL; there is no alternate table,
color, or interactive presentation mode.

## Setup

Use the prebuilt static binary at `dist/subagent`, or build it with
`Dockerfile.release`. Configure an OpenAI-compatible chat-completions endpoint:

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
subagent agents list|status|logs|context|send|time|stop|delete
subagent config list|get|set
```

Run any command with `--help` for its exact input flags and JSON output schema.
Agent identifiers are unique `agt_<ULID>` values. A spawn can use `--mode readonly`
(the default) or `--mode write`, and `--wall-time HOURS` accepts values up to 100.
`SUBAGENT_MAX_AGENTS` or `config set max-agents N` controls concurrent working
agents; zero means unlimited.

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

All model-driven tests run inside Docker using a mock OpenAI-compatible streaming
server. Build and run the test image with:

```sh
docker build -f Dockerfile.test -t subagent-test .
docker run --rm subagent-test
```

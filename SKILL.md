---
name: subagent-cli
description: Use the `subagent` CLI to start, monitor, question, continue, stop, and delete persistent background coding agents. Trigger for delegated or parallel coding work and for any request involving `subagent daemon`, `subagent agents`, or `subagent config` commands.
---

# Subagent CLI

`subagent` runs coding agents in the background. A subagent has its own task, working
directory, conversation, tools, status, and stable `agt_<ULID>` ID. A per-user daemon
keeps agents running after the command that spawned them exits and persists their
history for later inspection or continuation.

All operational commands emit JSONL. Help and version output are plain text.

## Install

Supported platform: Linux x86-64 (`x86_64`/`amd64`). Install without `sudo`:

```sh
curl -fsSL https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh | sh
```

The installer verifies the release checksum, installs to
`$HOME/.local/bin/subagent`, and prints the installed version. Optional controls:

```sh
SUBAGENT_VERSION=v0.1.0 sh install.sh
SUBAGENT_INSTALL_DIR="$HOME/bin" sh install.sh
```

## Configure and start

```sh
export OPENAI_API_KEY='...'
export OPENAI_BASE_URL='https://example.com/v1'
export OPENAI_MODEL='your-model'
subagent daemon start
```

Keep API keys out of tasks. The daemon requires `OPENAI_API_KEY` and removes it from
agent shell environments.

## Output schemas

Commands below refer to these exact shared schemas.

### Agent

```json
{
  "type": "agent",
  "id": "agt_<ULID>",
  "title": "string",
  "dir": "/canonical/path",
  "mode": "readonly|write",
  "advisory_readonly": true,
  "model": "string",
  "status": "working|finished|stopped|failed",
  "spawned_at": "RFC3339",
  "run_started_at": "RFC3339",
  "updated_at": "RFC3339",
  "finished_at": "RFC3339|null",
  "stopped_at": "RFC3339|null",
  "failed_at": "RFC3339|null",
  "deadline_at": "RFC3339|null",
  "run_number": 1,
  "stop_reason": "string|null",
  "last_error": "string|null"
}
```

`advisory_readonly` is `true` in readonly mode and `false` in write mode.

### Event

```json
{
  "event_id": "evt_<ULID>",
  "agent_id": "agt_<ULID>",
  "sequence": 1,
  "timestamp": "RFC3339",
  "type": "lifecycle|user_message|assistant_message|reasoning|tool_call|tool_result|error",
  "data": {}
}
```

### Error

Errors go to stderr and return a non-zero exit status:

```json
{"type":"error","code":"string","message":"string"}
```

Codes include `cli_error`, `not_found`, `max_agents_reached`, `conflict`,
`invalid_argument`, and `internal_error`.

## Daemon commands

### `subagent daemon start`

Start the detached daemon. Output: exactly one daemon object after it is ready.

```json
{"type":"daemon","status":"running","pid":1234,"socket":"/path/subagent.sock","working_agents":0,"max_agents":0,"model":"string","base_url":"string"}
```

### `subagent daemon status`

Output: exactly the same running daemon object as `daemon start`.

### `subagent daemon stop`

Stop the daemon and all working agents. Output: exactly one object:

```json
{"type":"daemon","status":"stopping","working_agents":2}
```

## Agent commands

### `subagent agents spawn`

```sh
subagent agents spawn --dir DIR \
  (--message TEXT | --message-file PATH) \
  [--title TITLE] [--mode readonly|write] [--wall-time HOURS]
```

- `--message-file -` reads stdin.
- `--mode` defaults to `readonly`.
- `--wall-time` must satisfy `0 < HOURS <= 100`.

Output: exactly one Agent object with `status:"working"` and `run_number:1`.

Use this for a new independent task. The new agent does not inherit the caller's
conversation, so make the task self-contained. Use write mode only when changes are
intended.

### `subagent agents list`

```sh
subagent agents list \
  [--status working|finished|stopped|failed]... \
  [--dir DIR] \
  [--spawned-after RFC3339] [--spawned-before RFC3339] \
  [--finished-after RFC3339] [--finished-before RFC3339] \
  [--sort spawned_at|updated_at|finished_at] \
  [--order asc|desc] [--limit N] [--offset N]
```

Defaults: `--sort spawned_at --order desc --limit 100 --offset 0`.

Output: zero or more Agent objects, one per line. No match emits no lines.

### `subagent agents status ID`

Output: exactly one Agent object for `ID`.

### `subagent agents logs ID`

```sh
subagent agents logs ID [--type TYPE]... [--after EVENT_ID] [--limit N] [--follow]
```

Default limit: newest 100 events. `--type` is repeatable. `--after` uses an event ID
as a cursor.

Output without `--follow`: zero or more Event objects, one per line.

Output with `--follow`: historical matching Event objects followed by new Event
objects as they are appended; the stream remains open until disconnected.

### `subagent agents context ID`

```sh
subagent agents context ID [--include TYPE]... [--max-tokens N]
```

Defaults: include `user_message` and `assistant_message`; maximum approximately
12,000 tokens.

Output first line:

```json
{"type":"context_meta","agent_id":"agt_<ULID>","estimated_tokens":123,"max_tokens":12000,"truncated":false,"included_types":["user_message","assistant_message"]}
```

Output remaining lines: zero or more Event objects selected for context.

### `subagent agents send ID`

```sh
subagent agents send ID \
  (--message TEXT | --message-file PATH) [--wall-time HOURS]
```

If the agent is working, queue the message for its next safe boundary. Otherwise,
resume it as a new run using its persisted context. Output: exactly one updated Agent
object. A resumed agent has `status:"working"` and an incremented `run_number`.

### `subagent agents side ID`

Alias: `subagent agents btw ID`.

```sh
subagent agents side ID \
  (--message TEXT | --message-file PATH) [--wall-time HOURS]
```

Ask one question using a snapshot of the parent's context and workspace. The side
agent can read, search, run non-mutating Bash, poll terminals, read command output,
and view images. It never receives `write`, `edit`, or `apply_patch`. Nothing from
the side run is added to the parent transcript.

Output: exactly one side-answer object:

```json
{
  "type": "side_answer",
  "side_id": "side_<ULID>",
  "agent_id": "agt_<ULID>",
  "answer": "string",
  "model": "string",
  "mode": "readonly",
  "parent_mode": "readonly|write",
  "ephemeral": true,
  "inherited_context_messages": 12,
  "tool_calls": 3,
  "usage": null
}
```

`usage` is either `null` or the usage object returned by the API.

### `subagent agents time ID HOURS`

Require a working agent and `0 < HOURS <= 100`. Reset its deadline to `HOURS` from
now. Output: exactly one updated Agent object with the new `deadline_at`.

### `subagent agents stop ID`

Require a working agent. Stop it and its terminal process groups. Output: exactly one
updated Agent object with `status:"stopped"`, `stop_reason:"user_request"`, and a
non-null `stopped_at`.

### `subagent agents delete ID`

Require a non-working agent. Permanently delete its metadata, context, events, and
outputs. Output: exactly one object:

```json
{"type":"agent_deleted","id":"agt_<ULID>"}
```

## Configuration commands

Supported keys: `base-url`, `model`, `max-agents`, `context-token-budget`, and
`tool-output-preview-bytes`.

### `subagent config list`

Output: exactly one object:

```json
{"type":"config","base-url":"string","model":"string","max-agents":0,"context-token-budget":64000,"tool-output-preview-bytes":16384}
```

### `subagent config get KEY`

Output: exactly one object. `value` is a string or non-negative integer depending on
the key.

```json
{"type":"config_value","key":"model","value":"string"}
```

### `subagent config set KEY VALUE`

Persist a non-secret setting. Output: exactly one object:

```json
{"type":"config_value","key":"max-agents","value":8,"note":"restart daemon for this value to take effect"}
```

Restart the daemon after setting a value. `max-agents:0` means unlimited; a positive
value rejects new work when that many agents are working. API keys cannot be stored
with `config`.

## Use the right command

- New independent task: `agents spawn`.
- Question about an existing agent: `agents side`.
- New durable instruction or continuation: `agents send`.
- Quick state check: `agents status`.
- Detailed or streaming progress: `agents logs`.
- End active work: `agents stop`.
- Remove stored history: `agents delete` only with explicit authorization.

Normal agents run directly with the daemon user's host permissions. Readonly mode
withholds structured mutation tools but Bash remains advisory. `subagent` does not
create worktrees or prevent concurrent agents from editing the same files.

If installed behavior differs from this skill, use
`subagent <group> <command> --help` as the authority.

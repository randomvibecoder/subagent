---
name: subagent-cli
description: Manage persistent background coding agents with the `subagent` JSONL CLI. Use when an agent needs to delegate independent repository work, run multiple coding tasks concurrently, inspect agent status or logs, continue a previous agent, ask an ephemeral readonly question over an agent's context, change a deadline, stop work, or delete stored agent history.
---

# Subagent CLI

Use `subagent` to delegate coding work to independent agents that run through a
per-user daemon. Treat stdout as JSONL, retain every returned agent ID, and use the
noun-verb command structure exactly as documented below.

## Understand subagents

A **subagent** is a persistent background coding worker created by another agent. It
has its own:

- Stable ID in the form `agt_<ULID>`
- Working directory
- Model conversation and persisted context
- Readonly or write mode
- Lifecycle status and optional deadline
- Tool runtime with up to eight live background terminals
- Event log and complete command-output files

The `subagent` process invoked from a shell is only a client. It sends one request to
a user-owned daemon through a private Unix socket, prints JSONL, and exits. The daemon
owns active workers. Finished agents are unloaded from memory but remain on disk and
can be resumed.

Subagents are useful for independent, self-contained work that can proceed in the
background or in parallel. They are not the right choice for a quick question about
an existing agent; use `agents side` for that. They are also not a hard security
boundary: normal agents execute with the daemon user's host permissions.

## Establish readiness

Confirm that the CLI exists:

```sh
command -v subagent
subagent --version
```

Check the daemon before starting it:

```sh
subagent daemon status
```

If it is not running, configure an OpenAI-compatible endpoint in the daemon's
environment and start it:

```sh
export OPENAI_API_KEY='...'
export OPENAI_BASE_URL='https://example.com/v1'
export OPENAI_MODEL='your-model'
subagent daemon start
```

Never put API keys in agent messages. `OPENAI_API_KEY` is required to start the
daemon, is held in daemon memory, and is removed from agent shell environments.

## Choose the correct operation

Use this decision order:

1. Use `agents spawn` for new, independent work.
2. Use `agents side` or `agents btw` for one question that should inherit an existing
   agent's context without changing its transcript.
3. Use `agents send` to add an instruction to an existing agent or resume it.
4. Use `agents status` for current state and `agents logs` for detailed progress.
5. Use `agents stop` when work must end; use `agents delete` only when its persisted
   history is no longer needed.

For new work, choose the mode deliberately:

- `readonly`: investigation, review, explanation, planning, or diagnosis.
- `write`: implementation, file edits, fixes, builds, or tests that may change state.

Readonly mode withholds structured mutation tools, but Bash remains available and
the restriction is advisory. Do not treat readonly mode as a sandbox.

## Handle JSONL

Operational output is one JSON object per line. There is no alternate human/table
mode. Parse each line independently. Errors are JSON objects written to stderr and
produce a non-zero exit status.

When shell parsing is needed, use `jq` if available:

```sh
result=$(subagent agents spawn --dir "$PWD" --message "Inspect this repository")
id=$(printf '%s\n' "$result" | jq -r '.id')
```

Do not guess or reconstruct IDs. Preserve the exact returned `agt_...` value.

## Daemon commands

### Start

```sh
subagent daemon start
```

Start the detached per-user daemon. Fail if another daemon already owns the socket.
Return one daemon JSON object after readiness.

### Status

```sh
subagent daemon status
```

Report daemon status, PID, socket path, active-agent count, capacity, model, and API
base URL.

### Stop

```sh
subagent daemon stop
```

Request an orderly daemon shutdown. This stops working agents and their terminal
process groups. Do not stop the daemon merely because one agent should stop; use
`agents stop ID` instead.

## Agent commands

### Spawn a new agent

```sh
subagent agents spawn [OPTIONS] --dir DIR \
  (--message TEXT | --message-file PATH)
```

Options:

- `--dir DIR`: required existing working directory; stored canonically.
- `--message TEXT`: inline task.
- `--message-file PATH`: read a UTF-8 task from a file; use `-` for stdin.
- `--title TITLE`: stable display title; defaults to the first non-empty task line.
- `--mode readonly|write`: defaults to `readonly`.
- `--wall-time HOURS`: optional deadline, where `0 < HOURS <= 100`.

Use message files for long or quote-heavy tasks:

```sh
subagent agents spawn \
  --dir /home/user/project \
  --mode write \
  --title "Repair authentication" \
  --message-file /tmp/auth-task.md
```

Spawn returns one agent object containing `id`, `title`, `dir`, `mode`, `model`,
`status`, timestamps, deadline, run number, stop reason, and last error. New agents
start in `working` status.

Write self-contained tasks. State the objective, important constraints, expected
verification, and completion condition. Do not assume the subagent sees the calling
agent's conversation.

### List agents

```sh
subagent agents list [OPTIONS]
```

Options:

- `--status working|finished|stopped|failed`: repeatable.
- `--dir DIR`: filter by canonical working directory.
- `--spawned-after RFC3339`
- `--spawned-before RFC3339`
- `--finished-after RFC3339`
- `--finished-before RFC3339`
- `--sort spawned_at|updated_at|finished_at`: default `spawned_at`.
- `--order asc|desc`: default `desc`.
- `--limit N`: default `100`.
- `--offset N`: default `0`.

Each matching agent is emitted as one JSONL line. No match produces no output.

Examples:

```sh
subagent agents list --status working
subagent agents list --dir "$PWD" --sort updated_at --order desc --limit 20
subagent agents list --status failed --status stopped
```

### Read status

```sh
subagent agents status ID
```

Return the current metadata for exactly one agent. Interpret statuses as follows:

- `working`: the daemon currently owns an active run.
- `finished`: the model completed normally without another queued message.
- `stopped`: the user, deadline, daemon shutdown, or daemon interruption stopped it.
- `failed`: an API, tool, storage, or other fatal error ended it.

### Read or follow logs

```sh
subagent agents logs ID [OPTIONS]
```

Options:

- `--type TYPE`: repeatable event filter.
- `--after EVENT_ID`: emit only events after the supplied cursor.
- `--limit N`: newest historical events; default `100`.
- `--follow`: keep the connection open and stream new events.

Event types include `lifecycle`, `user_message`, `assistant_message`, `reasoning`,
`tool_call`, `tool_result`, and `error`. Every event includes `event_id`, `agent_id`,
`sequence`, `timestamp`, `type`, and `data`.

Use `--follow` only when a blocking stream is appropriate. For periodic automation,
prefer `status` or cursor-based `logs --after EVENT_ID`.

### Read bounded context

```sh
subagent agents context ID [OPTIONS]
```

Options:

- `--include TYPE`: repeatable; defaults to `user_message` and
  `assistant_message`.
- `--max-tokens N`: approximate output budget; default `12000`.

The first line is `context_meta`; remaining lines are selected events. Use this for
model-sized handoff context, not for complete diagnostics. Use `logs` when tool and
lifecycle detail matters.

### Send or resume

```sh
subagent agents send ID [OPTIONS] \
  (--message TEXT | --message-file PATH)
```

Optional `--wall-time HOURS` resets a working deadline from now or sets the resumed
run's deadline.

If the agent is `working`, queue the message for its next safe model boundary. If it
is `finished`, `stopped`, or `failed`, start a new run with the persisted context and
increment `run_number`.

Use `send` when the instruction should become part of the parent agent's durable
conversation. Do not use it for an ephemeral question; use `side`.

### Ask a side question

```sh
subagent agents side ID [OPTIONS] \
  (--message TEXT | --message-file PATH)
subagent agents btw ID [OPTIONS] \
  (--message TEXT | --message-file PATH)
```

`btw` is an alias for `side`. Optional `--wall-time HOURS` bounds the side run.

A side agent:

- Inherits a valid snapshot of the parent agent's full model context and workspace.
- Runs independently, including while the parent is working.
- Has the single goal of answering the question.
- Can read, glob, grep, run non-mutating Bash, poll terminals, read command output,
  and view images.
- Is always readonly, even when the parent is write-enabled.
- Never receives `write`, `edit`, or `apply_patch`.
- Does not persist its question, reasoning, tool calls, answer, or temporary outputs.
- Does not alter the parent's transcript.

Use side questions for facts, explanations, locations, rationale, or status that may
require inspecting the workspace. Bash restrictions remain instruction-based.

The response is one `side_answer` object containing `side_id`, parent `agent_id`,
`answer`, `model`, readonly `mode`, `parent_mode`, inherited-message count, tool-call
count, and optional usage.

### Change a deadline

```sh
subagent agents time ID HOURS
```

Require a working agent and `0 < HOURS <= 100`. Reset the deadline to `HOURS` from
now. This command cannot clear a deadline.

### Stop an agent

```sh
subagent agents stop ID
```

Require a working agent. Signal the worker, terminate all of its terminal process
groups, persist `stopped` status, and return updated metadata.

### Delete an agent

```sh
subagent agents delete ID
```

Permanently remove metadata, context, events, and stored command outputs. Refuse to
delete a working agent. Stop it first if deletion is truly intended. Treat deletion
as destructive and do not infer permission from a request to stop or hide work.

## Configuration commands

### List

```sh
subagent config list
```

Return all non-secret settings.

### Get

```sh
subagent config get KEY
```

### Set

```sh
subagent config set KEY VALUE
```

Supported keys:

- `base-url`
- `model`
- `max-agents`
- `context-token-budget`
- `tool-output-preview-bytes`

Stored configuration changes take effect after restarting the daemon. Never store
`OPENAI_API_KEY` through `config`; it is intentionally environment-only.

Environment overrides:

- `OPENAI_API_KEY`
- `OPENAI_BASE_URL`
- `OPENAI_MODEL`
- `SUBAGENT_MAX_AGENTS`

`max-agents = 0` means unlimited. A positive limit is a hard rejection threshold
for simultaneously working agents; there is no queue.

## Work effectively

For a delegated implementation:

1. Spawn with a precise title, canonical project directory, write mode, and explicit
   verification requirements.
2. Save the returned ID.
3. Continue other independent work.
4. Check `status` and inspect filtered logs when needed.
5. Use `send` for corrections or additional durable requirements.
6. Inspect the final status and relevant assistant/tool events before relying on the
   result.

For parallel work, spawn independent agents only when their directories or intended
changes will not collide. `subagent` does not create worktrees or merge concurrent
edits.

For a question about an existing agent, use `side` first. Use `send` only if the
answer or instruction should influence the parent's continuing work.

## Respect safety boundaries

- Assume normal agents are unsandboxed and can access the daemon user's files,
  processes, credentials, and network.
- Use `--mode write` only when mutation is intended.
- Treat readonly and side-agent Bash restrictions as advisory.
- Scope `--dir` to the intended project.
- Do not place secrets in prompts, titles, logs, or message files.
- Do not delete agent history without explicit authorization.
- Stop runaway work and terminal processes promptly.
- Remember that stopping the daemon stops all working agents.

Use `subagent <group> <command> --help` whenever installed behavior and this skill
differ. The installed CLI is authoritative.

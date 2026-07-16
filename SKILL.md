---
name: subagent-cli
description: Use the subagent JSONL CLI to install, configure, start, monitor, message, question, stop, and delete persistent background coding agents. Trigger for delegated or parallel coding work and requests involving subagent daemon, agents, messages, or config commands.
---

# Subagent CLI

Use subagent to run coding agents through a detached per-user daemon. Background work
returns an ID immediately. Every operational response is UTF-8 JSONL: one object per
line, never a top-level array. Help and version output are plain text.

For exact input grammar, output schemas, lifecycle, errors, tools, and edge behavior,
read [references/protocol.md](references/protocol.md). Before implementing a parser or
using an unfamiliar command, also read
[references/cli.schema.json](references/cli.schema.json).

## Install

Linux x86-64 only. This always installs the latest statically linked release:

~~~sh
curl -fsSL https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh | sh
~~~

The installer verifies SHA-256 and writes to $HOME/.local/bin/subagent. To choose
another user-owned destination:

~~~sh
curl -fsSL https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh |
  SUBAGENT_INSTALL_DIR="$HOME/bin" sh
~~~

## Start the daemon

~~~sh
export OPENAI_API_KEY='...'
export OPENAI_BASE_URL='https://example.com/v1'
export OPENAI_MODEL='your-model'
subagent daemon start
~~~

The daemon captures these values at startup. Restart it after config or environment
changes. It removes OPENAI_API_KEY and SUBAGENT_WEB_PASSWORD from agent shell
environments, but this is not a sandbox: agents retain the daemon user's other host,
filesystem, process, network, and credential access.

## Optional Web UI

The Web UI is only needed when a human is in the loop and wants to watch transcripts,
inspect tool calls, send messages, or manage agents interactively. Agents and automated
workflows should use the JSONL CLI directly; they do not need to start or open the Web
UI. To enable the localhost-only interface for a human:

~~~sh
subagent daemon start --web-ui-port 7341
~~~

To require a password even on localhost, set it only in the startup environment:

~~~sh
SUBAGENT_WEB_PASSWORD='choose-a-secret' \
  subagent daemon start --web-ui-port 7341
~~~

Open the plain `web_ui_url` returned by `daemon start` or `daemon status`. HTTP Basic
Auth uses the fixed username `subagent`. If `SUBAGENT_WEB_PASSWORD` is absent, the
localhost UI is unauthenticated. `web_auth` reports `basic`, `none`, or `null` when the
UI is disabled; the password is never returned and is removed from agent shell
environments. The Web UI exposes human-facing views of CLI capabilities; it is not
required for daemon or agent operation.

## Happy path

Start independent background work:

~~~sh
subagent agents spawn \
  --dir /home/me/project \
  --message "Build the website" \
  --name "Website"
~~~

Output is one Agent. Save its stable agt_<ULID> ID:

~~~json
{"type":"agent","id":"agt_...","dir":"/home/me/project","status":"working","spawned_at":"2026-07-10T12:00:00Z","last_message_at":"2026-07-10T12:00:00Z","updated_at":"2026-07-10T12:00:00Z"}
~~~

The actual Agent contains every field defined in the schema reference.

List all working agents:

~~~sh
subagent agents list --status working
~~~

Each match is one compact agent_list_item line. Zero matches emits zero lines:

~~~json
{"type":"agent_list_item","id":"agt_1...","name":"Website","status":"working","dir":"/home/me/project","mode":"readonly","spawned_at":"...","last_message_at":"...","updated_at":"...","run_number":1,"working_sides":0}
{"type":"agent_list_item","id":"agt_2...","name":"Tests","status":"working","dir":"/home/me/tests","mode":"write","spawned_at":"...","last_message_at":"...","updated_at":"...","run_number":1,"working_sides":1}
~~~

Inspect one:

~~~sh
subagent agents status agt_...
~~~

Read its transcript:

~~~sh
subagent agents logs agt_...
~~~

By default this emits the newest 20 system, user, and assistant Events in chronological
order. Tool calls/results, reasoning, lifecycle, and errors are excluded so they do not
waste model context.

Prefer the high-signal inbox when coordinating several agents:

~~~sh
subagent inbox
subagent inbox --agent agt_... --priority 3 --limit 50 --offset 0
~~~

Inbox emits durable Notifications newest-first, one per line. The default is the
newest 20 entries at priority 2 or higher. Agents publish meaningful progress,
milestones, requests for input, and blockers; spawn, resume, finish, stop, and failure
notifications are automatic. This is the recommended master-agent view because it
avoids importing transcripts, code, tool calls, and tool results into the master's
context.

Send durable follow-up work:

~~~sh
subagent agents send agt_... --message "Also add dark mode"
~~~

The daemon stores the message before returning:

~~~json
{"type":"message_sent","message_id":"msg_...","agent_id":"agt_...","status":"queued","sent_at":"2026-07-10T12:05:00Z"}
~~~

Sent means durably accepted, not yet consumed by the model. Poll it:

~~~sh
subagent messages status agt_... msg_...
~~~

~~~json
{"type":"message","id":"msg_...","agent_id":"agt_...","content":"Also add dark mode","status":"delivered","sent_at":"...","delivered_at":"...","cancelled_at":null}
~~~

Start one durable readonly Side question:

~~~sh
subagent sides create agt_... --message "Which framework is it using?"
~~~

Output is a side_created receipt with a stable side_<ULID>. It returns immediately.
Inspect the saved answer and tool trace with sides status and sides logs. agents side
and agents btw are creation aliases.

## Commands

### Daemon

~~~text
subagent daemon start
subagent daemon status
subagent daemon stop
~~~

Start and status emit one daemon object. Stop returns status stopping before full
shutdown. Agent commands never auto-start the daemon.

### Agents

~~~text
subagent agents spawn --name NAME --dir DIR (--message TEXT | --message-file PATH)
    [--mode readonly|write] [--model MODEL] [--wall-time-minutes MINUTES]

subagent agents list
    [--status working|finished|stopped|failed]...
    [--dir DIR]
    [--spawned-after RFC3339] [--spawned-before RFC3339]
    [--finished-after RFC3339] [--finished-before RFC3339]
    [--sort spawned_at|updated_at|finished_at]
    [--order asc|desc] [--limit N] [--offset N]

subagent agents status AGENT_ID
subagent agents rename AGENT_ID NEW_NAME

subagent agents logs AGENT_ID
    [--type EVENT_TYPE]... [--all]
    [--after EVENT_ID] [--limit N] [--follow]

subagent agents context AGENT_ID

subagent agents send AGENT_ID
    (--message TEXT | --message-file PATH) [--wall-time-minutes MINUTES]

subagent agents side AGENT_ID
    (--message TEXT | --message-file PATH) [--model MODEL]
    [--wall-time-minutes MINUTES]

subagent agents time AGENT_ID MINUTES
subagent agents stop AGENT_ID
subagent agents delete AGENT_ID
~~~

Spawn returns immediately with an Agent ID. Relative paths resolve from DIR; absolute
paths, .., and escaping symlinks are permitted because DIR is a working directory, not
a security boundary.

NAME is mandatory, trimmed, 4–40 Unicode scalar values, control-free, case-sensitive,
and unique across all stored agents. It is only a human label in list output; all
commands still require ID. Rename works in every state and returns one agent_renamed
receipt. MINUTES is an integer from 1 through 6000; omission means no deadline.
Each agent_list_item also contains working_sides, from zero through two.
MODEL overrides the daemon default only for the new Agent and remains attached across
resumed runs. Omit it to use the daemon default.

### Sides

~~~text
subagent sides create AGENT_ID
    (--message TEXT | --message-file PATH) [--model MODEL]
    [--wall-time-minutes MINUTES]
subagent sides list AGENT_ID
    [--status working|finished|stopped|failed]... [--limit N] [--offset N]
subagent sides status SIDE_ID
subagent sides logs SIDE_ID
    [--type EVENT_TYPE]... [--all] [--after EVENT_ID] [--limit N] [--follow]
subagent sides stop SIDE_ID
subagent sides delete SIDE_ID
~~~

Side creation is asynchronous and persistent. At most two Side runs may be working
for one parent; a third returns capacity_exceeded. list emits compact Side records,
status includes the full question and nullable answer, and logs exposes the saved
conversation, reasoning, tool calls, and tool results. A daemon interruption marks a
working Side stopped instead of resuming it. Deleting a parent stops its working
Sides and removes all its Side histories. Side never appends to parent context.

Agent timestamps:

- spawned_at: creation time.
- last_message_at: latest daemon acceptance of a user message; initially spawn time.
- updated_at: latest consumed message, model/tool activity, deadline change, or state
transition.

Omitted Side MODEL inherits the parent Agent model; an override applies only to the
new Side.

### Inbox

~~~text
subagent inbox [--limit N] [--offset N] [--priority 1|2|3|4|5]
    [--agent AGENT_ID]
~~~

Output is Notification JSONL, newest first. limit defaults 20 and accepts 1–100;
offset defaults zero. priority is a minimum threshold, defaults 2, and therefore
`--priority 3` includes priorities 3, 4, and 5. agent filters by the main Agent ID and
also includes its Side notifications. The journal exposes its newest 10,000 entries.
There is no follow, wait, read/unread, or acknowledgement state.

Priority meanings are: 1 routine progress, 2 milestone/finish, 3 input required or
stop, 4 blocker/failure, and 5 reserved critical severity. Natural finish summary is
the final Agent message, capped at 5,000 Unicode scalar values.
- run_started_at: start of the current run.

### Log types

Valid types are system_message, user_message, assistant_message, reasoning, tool_call,
tool_result, lifecycle, and error.

- No --type: system, user, and assistant only.
- Repeated --type: only those exact types.
- --all: every type; conflicts with --type.
- --limit: 1 through 10000; default 20.
- --after: exclusive same-agent Event cursor.
- --follow: flush new matches and exit after the agent becomes terminal.

### Raw context debugging

agents context dumps the complete current persisted model context. Its first line is:

~~~json
{"type":"context_meta","agent_id":"agt_...","message_count":42,"compacted_at":"RFC3339|null"}
~~~

Remaining lines are unchanged model messages:

~~~json
{"role":"system","content":"..."}
{"role":"user","content":"..."}
{"role":"assistant","content":null,"tool_calls":[...]}
{"role":"tool","tool_call_id":"call_...","content":"..."}
~~~

Never print raw context directly into model-visible terminal output. It can contain
large tool results, image data, system instructions, and sensitive material. Redirect
it or filter narrowly:

~~~sh
subagent agents context agt_... > /tmp/agent-context.jsonl
subagent agents context agt_... |
  jq -c 'select(.role == "user" or .role == "assistant")'
~~~

This is the complete current context, not lifetime history. It may already be
compacted. Use agents logs --all for persisted Event history.

### Durable messages

~~~text
subagent messages list AGENT_ID [--status pending|delivered|cancelled]...
subagent messages status AGENT_ID MESSAGE_ID
subagent messages cancel AGENT_ID MESSAGE_ID
~~~

List emits one Message per line. Cancel works only while pending. Delivery is FIFO.
Pending messages survive daemon failure. On restart, interrupted agents with pending
messages automatically resume as capacity becomes available. A delivered user_message
Event contains its message_id.

### Configuration

~~~text
subagent config list
subagent config get KEY
subagent config set KEY VALUE
~~~

Keys: base-url, model, max-agents, context-token-budget, and
tool-output-preview-bytes. Base URL/model must be nonempty; context and preview budgets
must be positive. max-agents defaults to 4; set it to 0 only when unlimited
concurrency is intentional. Restart the daemon after setting.

Precedence is compiled defaults, persisted config, then environment overrides.
List/get show effective values; set changes persisted values without copying overrides.

## Modes and safety

Write agents receive write, edit, and apply_patch. Readonly and side agents do not.
They still receive Bash and are instructed never to mutate through it. That instruction
is advisory, not enforced. Bash exists for inspection such as rg, grep, find, cat, and
non-in-place sed.

Deleting an agent stops its working Side runs and removes daemon metadata, context,
Events, Messages, Side histories, and stored terminal output. It never deletes or
reverts the working directory, project files, Git state, commits, or branches.

## Choosing a command

- New independent task: agents spawn.
- List active work: agents list --status working.
- Coordinate many agents without context rot: inbox, optionally filtered by agent or
  priority.
- Inspect normal conversation: agents logs.
- Inspect tools/errors: agents logs with --type or --all.
- Durable instruction: agents send, then messages status.
- Focused readonly question: sides create, then sides status or sides logs.
- Debug exact model input: agents context, redirected or filtered.
- End active work: agents stop.
- Remove daemon history: agents delete only with explicit authorization.

The latest binary, this skill, the protocol reference, and the JSON Schema form one
contract. Backward compatibility with older releases is not promised.

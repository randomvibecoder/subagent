---
name: subagent-cli
description: Use the subagent JSONL CLI to install, configure, start, monitor, message, question, stop, and delete persistent background coding agents. Trigger for delegated or parallel coding work and requests involving subagent daemon, agents, messages, or config commands.
---

# Subagent CLI

Use subagent to run coding agents through a detached per-user daemon. Background work
returns a preferred short ref plus durable ID immediately. Every operational response is UTF-8 JSONL: one object per
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

Output is one Agent. Prefer its short local `ref`; retain `id` for exports and external integrations:

~~~json
{"type":"agent","id":"agt_...","ref":"a_1","name":"Website","dir":"/home/me/project","status":"working","spawned_at":"2026-07-10T12:00:00Z","last_message_at":"2026-07-10T12:00:00Z","updated_at":"2026-07-10T12:00:00Z"}
~~~

The actual Agent contains every field defined in the schema reference.

List all working agents:

~~~sh
subagent agents list --status working
~~~

Each match is one compact agent_list_item line followed by a `list_summary`. Zero matches still emits `{"type":"list_summary","resource":"agents","count":0}`:

~~~json
{"type":"agent_list_item","id":"agt_1...","ref":"a_1","name":"Website","status":"working","model":"gpt-5.4-mini","current_phase":"requesting_model","last_event_at":"...","dir":"/home/me/project","mode":"readonly","spawned_at":"...","last_message_at":"...","updated_at":"...","run_number":1,"working_sides":0}
{"type":"list_summary","resource":"agents","count":1,"next_cursor":null}
~~~

Inspect one:

~~~sh
subagent agents status a_1
~~~

Read its transcript:

~~~sh
subagent agents logs a_1
~~~

By default Agent logs emit the newest 20 system, user, and assistant Events in
chronological order, followed by `logs_summary`. Tool calls/results, reasoning,
lifecycle, and errors are excluded so they do not waste model context.

Prefer the high-signal inbox when coordinating several agents:

~~~sh
subagent inbox list --agent a_1 --priority 3 --limit 50 --offset 0
~~~

Inbox emits unread durable Notifications newest-first, then one `inbox_summary`. The default is
the newest 20 unread entries at priority 2 or higher. Agents publish meaningful progress,
milestones, requests for input, and blockers; spawn, resume, finish, stop, and failure
notifications are automatic. This is the recommended master-agent view because it
avoids importing transcripts, code, tool calls, and tool results into the master's
context.

Send durable follow-up work:

~~~sh
subagent agents send a_1 --message "Also add dark mode"
~~~

The daemon stores the message before returning:

~~~json
{"type":"message_sent","message_id":"msg_...","message_ref":"m_1","agent_id":"agt_...","agent_ref":"a_1","status":"queued","agent_resumed":false,"run_number":1,"agent_status":"working","resume_state":"not_needed","sent_at":"2026-07-10T12:05:00Z"}
~~~

Sent means durably accepted, not yet consumed by the model. Poll it:

~~~sh
subagent messages status a_1 m_1
~~~

~~~json
{"type":"message","id":"msg_...","ref":"m_1","agent_id":"agt_...","agent_ref":"a_1","content":"Also add dark mode","status":"delivered","sent_at":"...","delivered_at":"...","cancelled_at":null}
~~~

Start one durable readonly Side question:

~~~sh
subagent sides create a_1 --message "Which framework is it using?"
~~~

Output is a side_created receipt with a stable `side_<ULID>` and local `s_` reference. It returns immediately.
Inspect the saved answer and tool trace with sides status and sides logs. `sides create`
is the sole Agent-context Side creation command.

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
    [--order asc|desc] [--limit N] [--offset N] [--after-cursor CURSOR] [--verbose]

subagent agents status AGENT
subagent agents wait AGENT [--timeout-seconds SECONDS]
subagent agents rename AGENT NEW_NAME

subagent agents logs AGENT
    [--type EVENT_TYPE]... [--all]
    [--after EVENT_ID] [--limit N] [--follow]

subagent agents context AGENT

subagent agents send AGENT
    (--message TEXT | --message-file PATH) [--wall-time-minutes MINUTES]

subagent sides create AGENT
    (--message TEXT | --message-file PATH) [--model MODEL]
    [--wall-time-minutes MINUTES]

subagent agents time AGENT MINUTES
subagent agents stop AGENT
subagent agents delete AGENT
~~~

Spawn returns immediately with a preferred a_N ref and durable Agent ID. Relative paths resolve from DIR; absolute
paths, .., and escaping symlinks are permitted because DIR is a working directory, not
a security boundary.

NAME is mandatory, trimmed, 4–40 Unicode scalar values, control-free, case-sensitive,
and unique across all stored agents. Canonical system IDs/refs are reserved, while
prefix-like names such as `a_team` remain valid. Commands resolve the full durable ID,
then short local reference, then exact Agent name. Rename works in every state and returns one agent_renamed
receipt. MINUTES is an integer from 1 through 6000; omission means no deadline.
Each agent_list_item also contains working_sides, from zero through two.
Compact list output includes model, current_phase, and last_event_at. `--verbose`
emits the full Agent telemetry plus working_sides and seconds_since_last_event.
Agent and Side list limits default to 100 and accept 1 through 1000. The final Agent
list_summary contains a nullable `next_cursor`; pass a non-null value back through
`--after-cursor` for keyset pagination. Cursor mode permits omitted `--offset` or
explicit `--offset 0` and rejects nonzero offset. Offset-only pagination remains
available for compatibility but is less stable during concurrent updates.
MODEL overrides the daemon default only for the new Agent and remains attached across
resumed runs. Omit it to use the daemon default.

### Sides

~~~text
subagent sides create AGENT
    (--message TEXT | --message-file PATH) [--model MODEL]
    [--wall-time-minutes MINUTES]
subagent sides list AGENT
    [--status working|finished|stopped|failed]... [--limit N] [--offset N]
    [--after-cursor CURSOR]
subagent sides status SIDE
subagent sides logs SIDE
    [--type EVENT_TYPE]... [--all] [--after EVENT_ID] [--limit N] [--follow]
subagent sides stop SIDE
subagent sides delete SIDE
~~~

Side creation is asynchronous and persistent. At most two Side runs may be working
for one parent; a third returns capacity_exceeded. list emits compact Side records
including model/current_phase and a cursor-bearing summary,
status includes the full question and nullable answer, and logs exposes the saved
conversation, reasoning, tool calls, and tool results. A daemon interruption marks a
working Side stopped instead of resuming it. Deleting a parent stops its working
Sides and removes all its Side histories. Side never appends to parent context.

Agent timestamps:

- spawned_at: creation time.
- last_message_at and last_message_sent_at: latest daemon acceptance of user input;
  both begin at spawned_at for the initial task.
- last_message_delivered_at: latest input placed into model context; the initial task
  is direct context and therefore begins at spawned_at even though it is not a durable Message.
- run_started_at: start of the current run.
- updated_at: latest consumed message, model/tool activity, deadline change, or state
transition.

Working status includes `current_phase`, active request telemetry, historical
`last_provider_request_id`, `last_progress_at`, model/tool timestamps, and
`retry_count`. Active `request_started_at` and `provider_request_id` are null outside
requesting_model/retrying_model. A daemon watchdog emits one deduplicated
`possible_stall` notification after 180 seconds without progress by default. Configure
`stall-notification-seconds`; zero disables it. The watchdog diagnoses but never stops
or resumes work.

Omitted Side MODEL inherits the parent Agent model; an override applies only to the
new Side.

### Inbox

~~~text
subagent inbox list [--limit N] [--offset N] [--priority 1|2|3|4]
    [--after-cursor CURSOR] [--agent AGENT] [--all]
subagent inbox ack SEQUENCE_OR_NOTIFICATION_ID
subagent inbox follow [--after SEQUENCE] [--priority 1|2|3|4]
    [--agent AGENT]
~~~

List output is unread Notification JSONL, newest first, followed by exactly one
`inbox_summary` containing the emitted count, global acknowledgement watermark, and
nullable cursor toward older matches.
limit defaults 20 and accepts 1–100;
offset defaults zero. priority is a minimum threshold, defaults 2, and therefore
`--priority 3` includes priorities 3 and 4. agent accepts a ref, durable ID, or
exact name and also includes its Side notifications. `--all` includes acknowledged
history. ack durably marks the selected notification and every older sequence
handled; the watermark never moves backward. follow emits matching unread history
oldest-first and then flushes new JSONL until disconnected. The journal exposes its
newest 10,000 entries.

Priority meanings are: 1 routine progress, 2 milestone/finish, 3 input required or
stop, and 4 blocker/failure. Natural finish summary is
the final Agent message, capped at 5,000 Unicode scalar values.

### Log types

Valid types are system_message, user_message, assistant_message, reasoning, tool_call,
tool_result, lifecycle, and error. Agent logs default to system/user/assistant; Side
logs default to user/assistant. New Side histories do contain a system_message that can
be selected explicitly.

- No --type: the Agent or Side default described above.
- Repeated --type: only those exact types.
- --all: every type; conflicts with --type.
- --limit: 1 through 10000; default 20.
- --after: exclusive same-owner Event cursor.
- --follow: flush new matches and exit after the agent becomes terminal.
- Finite output always ends with logs_summary; follow emits Events only.

### Raw context debugging

agents context dumps the complete current persisted model context. Its first line is:

~~~json
{"type":"context_meta","agent_id":"agt_...","agent_ref":"a_1","agent_name":"Website","message_count":42,"compacted_at":"RFC3339|null"}
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
subagent messages list AGENT [--status pending|delivered|cancelled]...
    [--limit N] [--after-cursor CURSOR]
subagent messages status AGENT MESSAGE_ID
subagent messages cancel AGENT MESSAGE_ID
~~~

List emits newest-first, applies repeated status filters with OR, and ends with an
Agent-scoped cursor-bearing `list_summary`. Limit defaults 100 and accepts 1–1000.
Cancel works only while pending. Delivery is FIFO.
Pending messages survive daemon failure. On restart, interrupted agents with pending
messages automatically resume as capacity becomes available. A delivered user_message
Event contains its message_id.

### Configuration

~~~text
subagent config list
subagent config get KEY
subagent config set KEY VALUE
~~~

Keys: base-url, model, max-agents, context-token-budget,
tool-output-preview-bytes, and stall-notification-seconds. Base URL/model must be
nonempty; context and preview budgets must be positive. max-agents defaults to 8; set it to 0 only when unlimited
concurrency is intentional. Restart the daemon after setting only when the returned
`restart_required` field is true.

Precedence is compiled defaults, persisted config, then environment overrides.
List emits one `config_value` per key; get emits one. Each record separates default,
persisted, caller-local effective, and running-daemon active values and their sources.
`active_differs_from_local` reports any difference between the running daemon and the
calling shell's effective value. `restart_required` is true only for an unmasked
persisted/default change that the running daemon has not loaded; both are null when no
daemon is reachable. Set changes persisted values without copying overrides.

## Modes and safety

Write agents receive write, edit, and apply_patch. Readonly and side agents do not.
They still receive Bash and are instructed never to mutate through it. That instruction
is advisory, not enforced. Bash exists for inspection such as rg, grep, find, cat, and
non-in-place sed.

Deleting an agent stops its working Side runs and removes daemon metadata, context,
Events, Messages, Side histories, and stored terminal output. It never deletes or
reverts the working directory, project files, Git state, commits, or branches.

Every Event has explicit `owner:"agent"|"side"`. Agent Events omit `side_id` and
`side_ref`; Side Events include both, while `agent_id`/`agent_ref` identify their
parent. Per-owner mutations are serialized. Exactly one terminal transition wins,
and terminal metadata has exactly one matching finished/stopped/failed timestamp,
null deadline/request fields, and the matching terminal phase. On upgrade, protocol 5
deletes daemon-owned records that already contradict those invariants, including their
dependent Side histories and notifications; it never touches the project directory.

Graceful daemon shutdown stops accepting mutations, waits for accepted mutations,
and reports cleanup failure instead of writing a clean-stop marker. Terminal process
ownership is memory-only: daemon SIGKILL, host crash, or a descendant that escapes its
process group can leave processes behind. Subagent is not crash-proof OS isolation.

## Choosing a command

- New independent task: agents spawn.
- List active work: agents list --status working.
- Coordinate many agents without context rot: inbox list, optionally filtered by agent or
  priority.
- Inspect normal conversation: agents logs.
- Inspect tools/errors: agents logs with --type or --all.
- Durable instruction: agents send, then messages status.
- Focused readonly question: sides create, then sides status or sides logs.
- Debug exact model input: agents context, redirected or filtered.
- End active work: agents stop.
- Remove daemon history: agents delete only with explicit authorization.

The v0.1.8 contract uses `protocol_version:5`. The binary reports `version` and
`protocol_version` in daemon status. Operational CLI
commands reject an incompatible running daemon with `protocol_mismatch`; restart the
daemon after replacing the binary. The latest binary, this skill, the protocol
reference, and the JSON Schema form one contract.

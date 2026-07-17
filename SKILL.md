---
name: subagent-cli
description: Delegate, monitor, steer, interrupt, and collect final answers from persistent background coding Agents and readonly context-inheriting Sides through the Subagent JSONL CLI. Use for parallel or long-running coding work, keeping a coordinator's context small, supervising multiple models, or operating the subagent daemon, team, inbox, messages, Agents, and Sides.
metadata:
  version: "0.2.2"
---

# Subagent CLI

## What Subagent is

`subagent` is a local CLI and long-lived daemon for delegating work to independent AI workers. The Agent invoking this CLI is the **coordinator** (or master Agent). A worker created with `agents spawn` is a persistent **Agent** with its own model conversation, tools, working directory, lifecycle, logs, and final answer.

The CLI is only the control surface. `agents spawn` returns immediately while the daemon continues running the worker in the background. Closing the CLI command does not end the worker. Agent records, messages, events, and final answers survive coordinator turns and daemon restarts.

Workers operate directly in the directory supplied with `--dir`; Subagent does not create a Git branch, copy the project, or isolate workers from one another. Give parallel Agents separate directories or clearly non-overlapping ownership when concurrent edits could conflict.

~~~text
coordinator context
    |
    | short task, follow-up, status request
    v
subagent CLI -> local daemon -> Agent a_7 context + tools -> project directory
                            -> Agent a_8 context + tools -> project directory
                            -> Side  s_3 context + readonly tools
    ^
    | compact JSONL status, notifications, final answers
    +-----------------------------------------------------
~~~

## How it saves coordinator context

Subagent keeps worker detail outside the coordinator's model context. The coordinator sends a bounded task, remembers a short reference such as `a_7`, and consumes progress notifications plus the authoritative final answer. Raw reasoning, tool calls, terminal output, and the complete worker transcript remain in daemon-managed storage unless explicitly requested.

This lets the coordinator spend its context on decomposition, dependencies, decisions, and next steps instead of accumulating every file read and test log from every worker. For the common path:

1. Spawn a worker with all facts it needs.
2. Save its `ref` from the JSON response.
3. Use `team list --active` or `inbox wait` for bounded supervision.
4. Use `agents followup` only when direction changes.
5. Read `final_answer` when the run finishes.

Do not routinely call `agents context`, `logs --all`, or ingest generated source code into the coordinator conversation. Those commands defeat the context-saving design and are intended for targeted diagnosis. Default logs are bounded and omit tool traffic.

Context is isolated, not magically shared:

- A normal Agent receives the bundled Agent system prompt and the task passed to `spawn`. It does **not** inherit the coordinator's conversation, hidden reasoning, open files, or prior tool results. Put necessary requirements, paths, constraints, decisions, and acceptance tests in the task.
- A follow-up becomes durable input to that same Agent's existing context.
- A Side receives a frozen snapshot of one parent Agent's context plus a new question. It does not inherit the coordinator's separate conversation, and its trace is not added back to the parent.
- Each worker manages its own context budget. When an Agent context grows too large, the daemon semantically summarizes the oldest safe roughly 60% and retains the newest roughly 40% verbatim. This compaction affects the worker context, not the coordinator context.

## Coordinator rules

- Delegate concrete outcomes with explicit scope and verification criteria.
- Prefer several independent Agents for parallelizable work; avoid assigning two writers the same files.
- Treat JSONL receipts as the source of IDs and state. Prefer local refs such as `a_7`, `s_3`, `m_12`, and `e_90` for local commands.
- Supervise through `team list --active` and `inbox wait`; do not repeatedly poll every transcript.
- Trust an Agent's final answer as its report, but independently verify high-risk results with a separate focused Agent or a narrow local check.
- Pull raw logs or context only when a failure cannot be understood from status, notifications, and the final answer.

Operational output is UTF-8 JSONL: one object per line, never a top-level array. Retain full ULIDs for exports, backups, HTTP integrations, or cross-machine data.

For exact schemas, lifecycle invariants, tools, cursors, HTTP endpoints, and error codes, read [references/protocol.md](references/protocol.md) and [references/cli.schema.json](references/cli.schema.json).

## Install and start

Install the latest static Linux x86-64 release:

~~~sh
curl -fsSL https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh | sh
~~~

Check whether a daemon is already running:

~~~sh
subagent daemon status
~~~

If it reports a running daemon, use that daemon. Otherwise configure the OpenAI-compatible Chat Completions endpoint and start one:

~~~sh
export OPENAI_API_KEY='...'
export OPENAI_BASE_URL='https://api.openai.com/v1'
export OPENAI_MODEL='your-model'
subagent daemon start
~~~

The daemon captures environment values at startup; restart it after changing them. Agent shell commands run with the daemon user's host permissions. Readonly mode and Side non-mutation rules are advisory, not an OS sandbox.

## Recommended workflow

### 1. Delegate

~~~sh
subagent agents spawn \
  --name "API tests" \
  --dir /home/me/project \
  --mode write \
  --message "Add regression tests, run them, and summarize the result"
~~~

Spawn returns one complete Agent immediately. Save `ref` for local commands:

~~~json
{"type":"agent","id":"agt_...","ref":"a_7","name":"API tests","status":"working","run_number":1,"final_answer":null}
~~~

Use `--model MODEL` to override the daemon model for one Agent. Normal Agents start with only the Agent system prompt and their assigned task; they do not inherit another Agent's conversation.

### 2. Supervise the team

For one known Agent, wait for it to reach a terminal state and return its complete Agent object:

~~~sh
subagent agents wait a_7 --timeout-seconds 300
~~~

For several concurrent workers, use the bounded active-team view:

~~~sh
subagent team list --active
~~~

This is the preferred coordinator view. `--active` emits working, interrupted, and capacity-waiting Agents plus active Sides and their parents, followed by one `team_summary`. Members include model, task, lifecycle status, derived coordination state, elapsed time, pending-message count, latest progress, and the full authoritative final answer when available. The summary includes working and available Agent slots. Omit `--active` only when complete persisted team history is intentionally needed; it can be very large on a long-lived daemon.

Wait for the first future high-signal update without polling:

~~~sh
subagent inbox wait --timeout-seconds 300 --priority 2 --type FINAL_ANSWER
~~~

The command returns the first matching Notification and exits. With no match before the deadline it exits successfully with:

~~~json
{"type":"wait_summary","resource":"inbox","matched":false,"count":0,"after_sequence":42,"timeout_seconds":300}
~~~

Use `--after SEQUENCE` to include existing records after a known sequence. Omit `--type FINAL_ANSWER` when progress and coordination messages should also wake the coordinator. Filter with `--agent AGENT`, `--priority 1..4`, or repeat `--type TYPE`.

### 3. Steer deliberately

Assign more work and wake or resume the Agent:

~~~sh
subagent agents followup a_7 --message "Also test the error response"
~~~

`subagent agents send` remains a behavior-compatible alias for `agents followup`.
It returns the historical `message_sent` receipt type; `agents followup` returns
`followup_sent`.

Store context without waking an inactive Agent:

~~~sh
subagent messages send a_7 --message "The API contract changed yesterday"
~~~

Both commands return immediately after durable acceptance. A receipt includes the Message ID/ref, intent, Agent status, run number, whether a run resumed, and one of:

- `not_needed`: the Agent was already working.
- `started`: a new working run began.
- `waiting_for_capacity`: the follow-up is pending and no slot is free.
- `not_woken`: a Message was stored for an inactive Agent.

Messages for a working Agent are delivered FIFO at the next safe model boundary. Inspect or cancel pending Messages with:

~~~sh
subagent messages list a_7 --status pending
subagent messages status a_7 m_12
subagent messages cancel a_7 m_12
~~~

### 4. Interrupt or stop

Interrupt only the current turn while preserving the Agent, context, and pending Messages:

~~~sh
subagent agents interrupt a_7
~~~

The Agent becomes `interrupted`. Resume it only with `agents followup` or the `agents send` alias.

Stop creates a terminal stopped lifecycle transition and cancels pending Messages:

~~~sh
subagent agents stop a_7
~~~

Delete removes daemon-managed history only. It never deletes or reverts workspace files:

~~~sh
subagent agents delete a_7
~~~

### 5. Consume the result

A successful run must produce nonempty assistant content. One empty provider turn is retried; a second fails with `empty_completion`.

The authoritative `FINAL_ANSWER` is stored in all three places:

- `final_answer` in `agents status` or `sides status`.
- The terminal `team_member`.
- The completion Notification's typed payload.

Each answer includes content, run number, Event ID/ref, and timestamp. Answers are limited to 1 MiB and are never silently truncated. Starting a new Agent run clears its current `final_answer`; historical Events and Notifications remain durable.

## Sides

Use a Side for one bounded question that needs the parent Agent's conversational context:

~~~sh
subagent sides create a_7 --message "Which migration is still unsafe?"
~~~

A Side:

- Inherits a frozen, compacted snapshot of the parent model context.
- Reads the same live working directory.
- Runs readonly tools under advisory non-mutation instructions.
- Has its own durable transcript and terminal processes.
- Is one-shot and cannot receive follow-ups.
- Does not add its trace to the parent context.
- Publishes the same typed `FINAL_ANSWER` envelope as an Agent.

Inspect it with:

~~~sh
subagent sides list a_7
subagent sides status s_3
subagent sides logs s_3
subagent sides stop s_3
subagent sides delete s_3
~~~

Sides remain a separate resource, not nested child Agents. At most two Sides may work for one parent at once.

## Command reference

### Agents and team

~~~text
subagent team list [--active]

subagent agents spawn --name NAME --dir DIR (--message TEXT | --message-file PATH)
    [--mode readonly|write] [--model MODEL] [--wall-time-minutes MINUTES]
subagent agents list [--status STATUS]... [--limit N] [--after-cursor CURSOR] [--verbose]
subagent agents status AGENT
subagent agents wait AGENT [--timeout-seconds SECONDS]
subagent agents rename AGENT NEW_NAME
subagent agents followup AGENT (--message TEXT | --message-file PATH) [--wall-time-minutes MINUTES]
subagent agents send AGENT (--message TEXT | --message-file PATH) [--wall-time-minutes MINUTES]
subagent agents interrupt AGENT
subagent agents time AGENT MINUTES
subagent agents stop AGENT
subagent agents delete AGENT
~~~

Agent statuses are `working`, `interrupted`, `finished`, `stopped`, or `failed`. List commands emit zero or more item records followed by one summary record, so empty success is never silent.

### Inbox

~~~text
subagent inbox wait [--after SEQUENCE] [--timeout-seconds N]
    [--priority 1|2|3|4] [--agent AGENT] [--type TYPE]...
subagent inbox list [--priority N] [--agent AGENT] [--limit N] [--all]
subagent inbox ack SEQUENCE_OR_NOTIFICATION_ID
subagent inbox follow [--after SEQUENCE] [--priority N] [--agent AGENT]
~~~

Typed envelopes are `NEW_TASK`, `MESSAGE`, `FOLLOWUP`, `PROGRESS`, `INTERRUPTED`, `FINAL_ANSWER`, and `FAILED`. Automatic progress comes from lifecycle transitions; Agents use `notify` for meaningful progress. Do not infer progress from arbitrary tool calls.

### Logs

~~~text
subagent agents logs AGENT [--type EVENT_TYPE]... [--all]
    [--after EVENT_ID] [--limit N] [--follow]
subagent sides logs SIDE [--type EVENT_TYPE]... [--all]
    [--after EVENT_ID] [--limit N] [--follow]
~~~

Agent logs default to the newest 20 `system_message`, `user_message`, and `assistant_message` Events, emitted chronologically, followed by `logs_summary`. Side logs default to user and assistant messages. Tool calls/results, reasoning, lifecycle, and errors are excluded by default to protect coordinator context. Use `--all` only for diagnosis.

### Raw context debugging

~~~sh
subagent agents context a_7 > context.jsonl
subagent agents context a_7 | jq -c 'select(.role == "user")'
~~~

`context` dumps the complete raw model context. Never read an unfiltered context dump into an agent conversation: it can be extremely large and is intended for debugging. Redirect it to a file or filter it narrowly with `jq`.

Agent context is append-only during normal work: user messages, assistant messages, and tool results are appended in order. When the configured approximate token budget is exceeded, the daemon makes a tool-free request to that Agent's selected model. It summarizes the oldest safe roughly 60% of conversation weight into one rolling summary while retaining the newest roughly 40% verbatim. The original Agent system prompt is always preserved, tool-call/result groups are never split, and a later compaction summarizes the previous rolling summary again. If semantic compaction cannot produce a nonempty summary that fits, the run fails explicitly instead of silently clipping history.

### Daemon and configuration

~~~text
subagent daemon start [--web-ui-port PORT]
subagent daemon status
subagent daemon stop
subagent config list
subagent config get KEY
subagent config set KEY VALUE
~~~

The Web UI is optional and only useful when a human is in the loop. Start it with `--web-ui-port`; set `SUBAGENT_WEB_PASSWORD` at daemon startup for localhost HTTP Basic authentication. Automated coordinators should normally use the JSONL CLI. A separate harness may use the localhost HTTP API documented in the protocol reference.

## Selection rules

- Use `agents spawn` for independent new work.
- Use `sides create` for a one-shot question requiring a parent Agent's context.
- Use `team list --active` for a compact live overview; unfiltered `team list` emits complete history.
- Use `inbox wait` to block for any high-signal update.
- Use `messages send` to add context without waking.
- Use `agents followup` to assign work and wake or resume.
- Use `agents interrupt` to cancel one turn but retain resumability.
- Use `agents stop` for a terminal stop.
- Use `agents logs` for readable transcript history.
- Use `agents context` only for filtered debugging.

Always inspect command exit status and stderr. Success JSONL is written to stdout; one structured Error is written to stderr. Streaming commands can emit valid stdout before a later connection error.

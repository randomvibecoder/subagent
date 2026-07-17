# Subagent v0.2.1 / Protocol 6 Reference

This file specifies the current binary. Backward compatibility is not promised. The
release binary, SKILL.md, this reference, and cli.schema.json must change together.

## Contents

1. Framing and errors
2. Primitive inputs
3. Core objects and lifecycle
4. Commands
5. Notifications
6. Durable messages
7. Events, logs, and context
8. Side agents
9. Model API
10. Agent tools
11. Optional localhost HTTP API
12. Storage and security

## 1. Framing and errors

Operational stdout is UTF-8 JSONL. Every line is one compact JSON object followed by
LF. Strings encode embedded newlines. Commands never emit tables, ANSI escapes,
progress, or arrays around multiple results. Agent, Side, and Message list commands
always finish with one list_summary line, including count zero. Non-streaming Inbox
queries always finish with one inbox_summary line, including count zero.
Finite logs always finish with one logs_summary line; follow streams Events only.
Streaming output is flushed after each object. An early stdout consumer disconnect is
treated as successful completion rather than a panic.

Help and version are plain text. Local parsing, configuration, startup, and daemon
connection failures write one Error to stderr and exit 2. Errors returned by the
daemon write one Error to stderr and exit 4. Success exits 0. Unix signal exit status,
including Ctrl+C, follows the invoking shell and operating system.

~~~json
{"type":"error","code":"invalid_argument","message":"human-readable text","details":{"field":"wall_time_minutes"},"retryable":false}
~~~

Every Error contains type, code, message, details, and retryable. Details is always an
object. Current semantic codes:

- daemon_unavailable: no reachable socket and no conclusive lifecycle record.
- daemon_stopped: the last daemon shut down cleanly.
- daemon_crashed: a daemon recorded as starting/running disappeared; details include
  its last PID, timestamps, daemon log path, and a bounded last-log-line summary.
- daemon_already_running: start found a reachable daemon.
- agent_not_found, side_not_found, event_not_found, message_not_found,
  notification_not_found.
- invalid_argument, file_too_large.
- capacity_exceeded: Agent spawn or Side creation rejected at capacity; retryable true.
- conflict: current entity state forbids the command.
- timeout: reserved for synchronous bounded operations; retryable true. Side deadline
  expiry is persisted as stopped wall_time instead of returning this Error.
- api_error: endpoint/network/status failure; retryable reflects status.
- context_compaction_failed: semantic summarization was empty, unsplittable, or could
  not fit with the retained recent context; retryable reflects whether another model
  request may succeed.
- protocol_mismatch: CLI and daemon protocol versions differ; restart after upgrading.
- internal_error: unclassified I/O, persistence, decoding, or invariant failure.
- cli_error: local clap/config/file-input failure not assigned a narrower code.

The Web HTTP surface additionally uses unauthorized for missing/incorrect Basic Auth
and invalid_origin for rejected state-changing browser origins. These are HTTP API
errors, not Unix-socket CLI semantic codes.

A tool failure is not a CLI Error. It becomes:

~~~json
{"ok":false,"code":"tool_error","error":"string"}
~~~

A shell process exiting nonzero is a normal completed result with ok false, exit_code,
output, output_ref, and truncated fields.

## 2. Primitive inputs

### IDs

Agent IDs are `agt_` plus an uppercase canonical ULID. Event, message, side, terminal,
and output IDs use `evt_`, `msg_`, `side_`, `term_`, and `out_`. Agents, Sides,
Messages, and Events also receive installation-local `a_N`, `s_N`, `m_N`, and `e_N`
references. Each type has its own persistent monotonic counter. References survive
restart and lifecycle changes and are never reused after deletion; gaps are valid.

Local CLI commands accept a short reference or full durable prefixed ID. Agent arguments
also accept an exact name. Durable IDs and short refs are authoritative before names.
JSON exposes both forms. CLI and HTTP paths resolve short refs; Agent paths also resolve
exact unambiguous names. Use durable IDs for exports, backups, external integrations,
cross-machine communication, and merged stores; a destination
import must preserve the ULID and allocate a new local reference.

### Deadline minutes

MINUTES is ASCII base-10 digits with numeric range 1 through 6000 inclusive. Signs,
fractions, exponent notation, NaN, and infinity are rejected. The deadline is that
many whole minutes after daemon receipt. Omission means no deadline.

### Integers

Integer CLI/config values use unsigned base-10 Rust usize parsing on the supported
x86-64 build. Negative values, fractions, underscores, and overflow are rejected.
Leading plus and leading zero behavior follows Rust FromStr; do not emit either.
Agent and Side list limits are 1 through 1000 and default to 100. Log limit is
explicitly 1 through 10000. Tool-specific limits are below.
Inbox limit is 1 through 100 and priority is 1 through 4.

### RFC3339

Filters use chrono RFC3339 parsing. Z, numeric offsets, and fractional seconds are
accepted and compared as instants. Serialized timestamps are UTC RFC3339 with Z and
chrono's required fractional precision. Boundaries are inclusive.

### Messages

Inline, file, and stdin messages share rules. File/stdin decoding must be valid UTF-8.
The exact limit is 1,048,576 UTF-8 bytes after decoding. Whitespace-only and
NUL-containing values are rejected. BOMs and trailing newlines are retained. Invalid
UTF-8 and read failures are local errors.

message-file is read by the CLI before IPC; message-file - reads stdin.

### Agent names

NAME is mandatory. The daemon trims both ends, then requires 4 through 40 Unicode
scalar values and rejects control characters. Names are case-sensitive and unique
across all stored Agents. Canonical system IDs and short refs are reserved, while
prefix-like names such as a_team and agt_team remain valid. Deleting an Agent releases
its name. Legacy collisions remain stored, but the authoritative ID/ref wins lookup.

## 3. Core objects and lifecycle

The complete machine schema is references/cli.schema.json.

Agent timestamps:

- spawned_at never changes.
- last_message_at begins at spawned_at and changes when send is durably accepted.
- last_message_sent_at begins at spawned_at because the initial task is accepted with
  creation. last_message_delivered_at also begins at spawned_at because that task is
  placed directly into model context; it is not a durable Message record. Later sends
  update sent at acceptance and delivered when the worker consumes the Message.
- updated_at changes for consumed user messages, reasoning/assistant/tool Events,
  deadline changes, and lifecycle/error Events. Queue acceptance alone does not change
  it.
- run_started_at is the current run start.
- while working, finished_at, stopped_at, and failed_at are null.
- in a terminal state, only its matching terminal timestamp is non-null.
- deadline_at is null without a deadline and is cleared at terminal transition.

The model field is selected from `--model` or the daemon default at spawn and remains
attached to that agent across daemon configuration changes and resumed runs. Side
runs inherit the parent model unless their creation command overrides it.

Transitions:

| Current | Cause | New | Reason |
| --- | --- | --- | --- |
| none | spawn | working | spawned Event |
| working | final model turn without tools | finished | none |
| working | interrupt | interrupted | user_request |
| working | stop | stopped | user_request |
| working | deadline | stopped | wall_time |
| working | fatal worker/API/store error | failed | last_error |
| working | graceful daemon stop | stopped | daemon_shutdown |
| persisted working | daemon recovery | stopped | daemon_interrupted |
| finished/stopped/failed/interrupted + pending follow-up | scheduler capacity | working | resumed |

Resume increments run_number, sets run_started_at, and clears all terminal timestamps,
deadline unless supplied by follow-up, stop_reason, last_error, and the current
final_answer. Historical answers remain in Events and Notifications.

Agent objects include the unique human-readable name. `request_started_at` and
`provider_request_id` describe only an active provider request and are null in all
other phases. `last_provider_request_id` retains the most recently observed provider
ID. `last_progress_at` advances for provider output, model output, delivered input,
new terminal output, and meaningful lifecycle/tool activity. An empty terminal poll
does not advance it.

A no-content/no-tool turn receives one corrective retry. A second consecutive empty
turn fails with `empty_completion`; it is never silently finished. A nonzero shell
exit is offered to the model and does not fail the Agent. Failure saves Agent status
and last_error, then emits one error Event with the identical error string. It does not
also emit a failed lifecycle Event. If persistence itself fails, metadata and Event
may not both be present; this is an internal_error condition.

## 4. Commands

### daemon start

Requires a nonempty OPENAI_API_KEY. Loads/validates config, creates private state and
runtime directories, starts a detached process, and waits up to five seconds for the
Unix socket. Output is one daemon object. Already running returns
daemon_already_running.

Before opening daemon.log for append, startup rotates it to daemon.log.1 when its size
is at least 10 MiB, replacing the previous backup. Rotation occurs at startup rather
than while the detached daemon is running.

Running means the store opened, interrupted Agent reconciliation completed, pending
message scheduling ran, and the socket accepts requests. It does not test API
credentials or make a model request.

`--web-ui-port PORT` optionally starts the embedded human dashboard on
127.0.0.1 only; PORT is 1 through 65535. Without it, no TCP listener exists. Start and
status include a plain `web_ui_url`, null when disabled. `web_auth` is null when the UI
is disabled, `none` when enabled without authentication, and `basic` when
SUBAGENT_WEB_PASSWORD was set at daemon startup. Basic Auth uses the fixed username
subagent. An explicitly empty or non-UTF-8 password prevents daemon startup. The
password is never persisted or returned. The browser UI is not a public protocol
surface.

### daemon status

With a daemon, emits one daemon object. Without one, it returns a structured local
daemon_stopped, daemon_crashed, or daemon_unavailable Error. Crashed diagnostics use
the persisted daemon lifecycle record and never dump the full log. The object includes binary version and
protocol_version. Except status and stop, a CLI first checks protocol compatibility
and returns protocol_mismatch instead of sending a request to an older daemon.

### daemon stop

Emits status stopping immediately. The daemon stops accepting mutations, waits for
already accepted mutations, stops active Agents/Sides, and propagates cleanup errors.
It records a clean shutdown and removes socket/lock only after successful cleanup.
Poll status until daemon_stopped to observe completion.

### agents spawn

DIR must exist and be a directory. The CLI canonicalizes it relative to its own
current directory; symlinks resolve. Git is not required. A missing or invalid
directory fails before IPC.

Mode defaults readonly. Omitted wall time has no deadline. Name is required as
specified above. Optional `--model` must be nonempty after trimming and overrides the
daemon default for this Agent. max-agents defaults to 8; zero explicitly selects
unlimited capacity. Otherwise spawn rejects at working capacity and does not create an
Agent. Success saves metadata/context/Events, registers the worker, and returns one
working Agent without waiting for a model call.

The initial Event order is lifecycle spawned, system_message, user_message source
spawn.

### agents list

Repeated status values are OR. DIR is exact canonical-string equality, not recursive.
An existing path is canonicalized. A missing filter is accepted only as an absolute
lexically normalized path, allowing the stored Agent.dir value to filter Agents whose
workspace was deleted.

spawned/finished bounds are inclusive. Finished bounds inspect finished_at only, so
stopped and failed Agents are excluded. Sort defaults spawned_at descending; valid
keys are spawned_at, updated_at, finished_at. ID is the deterministic secondary key.
Null finished_at sorts first ascending and last descending.

Default limit 100, maximum 1000, and offset 0. Zero and values above 1000 are rejected.
`--after-cursor` accepts the opaque `next_cursor` from a prior list_summary and may be
combined only with omitted offset or explicit offset zero. The cursor binds the requested
sort/order and continues after the last `(sort value, durable ID)` pair. It avoids
offset shifts and duplicate ties, though records whose mutable sort key changes can
still move between pages. Offset remains for compatibility. Output is one compact
agent_list_item per line followed by list_summary; agent summaries include nullable
next_cursor. Compact items include type, id, ref, name, status, dir,
mode, model, spawned_at, last_message_at, updated_at, current_phase, last_event_at,
run_number, and working_sides. `--verbose` emits full Agent telemetry plus
working_sides and seconds_since_last_event.

### agents status

Emits one full Agent, including name, or agent_not_found. Telemetry separates current_phase,
phase_started_at, request_started_at, last_provider_activity_at, provider_request_id,
last_provider_request_id, retry_count, last_event_at, last_model_event_at,
last_tool_event_at, last_state_change_at, and last_progress_at. Phases are starting,
processing_messages, requesting_model, retrying_model, executing_tool,
waiting_terminal, interrupted, finished, stopped, and failed.

Active telemetry invariants:

| Phase | request_started_at | provider_request_id |
| --- | --- | --- |
| requesting_model, retrying_model | non-null after request begins | nullable until provider supplies one |
| every other phase | null | null |

`phase_started_at` is the current phase entry time. `last_state_change_at` is the most
recent lifecycle status transition. The `last_*` provider/model/tool/progress fields
are historical and may remain populated in terminal states. Terminal phases always
have no active request or deadline.

### agents wait

`agents wait AGENT [--timeout-seconds 1..86400]` waits for that Agent to become
terminal and returns its full Agent. A timeout returns retryable `timeout`; it never
stops the Agent.

### agents rename

Accepts AGENT and NEW_NAME, applies the same validation/uniqueness rules as spawn,
and is valid in every lifecycle state. It changes no Agent activity timestamp. Output:

~~~json
{"type":"agent_renamed","id":"agt_...","ref":"a_17","name":"New name","renamed_at":"2026-07-10T12:00:00Z"}
~~~

### agents send

Compatibility alias for `agents followup`. It keeps the historical message_sent
receipt type while using follow-up wake/resume semantics.

### agents followup

Atomically writes a durable followup Message and returns a followup_sent receipt. It
never waits for model delivery. If working, delivery occurs FIFO at the next safe model
boundary. If inactive and capacity exists, a new run starts. If capacity is full, the
Message remains pending with resume_state waiting_for_capacity.

### messages send

Writes a durable Message with intent message. It is delivered at the next safe boundary
when already working, but never wakes an inactive Agent. The inactive receipt has
resume_state not_woken.

### agents interrupt

Requires working. Cancels the active turn and Agent-owned terminals, preserves context
and pending Messages, and transitions to interrupted without a terminal timestamp.
Only follow-up or the send compatibility alias resumes it.

### team list

Emits one team_member for every Agent and Side followed by one team_summary. Membership
is flat; Side members include their parent Agent ID/ref only for provenance. Members
contain task, model, coordination state, progress, pending count, and the complete
current final_answer. The summary exposes working and available Agent capacity.

While stop or interrupt cleanup is active, input commands return retryable conflict and
create no Message.

### agents time

Requires working. Replaces deadline with MINUTES from daemon receipt and emits the
Agent. It may shorten or extend the run.

### agents stop

Requires working. Cancels pending Messages, signals the worker, terminates owned
terminal process groups, waits for the worker cleanup boundary, marks stopped
user_request, and emits the Agent. A successful response means the stopped run can no
longer resume an in-flight tool task. Process
termination sends TERM, waits about 500 ms, then sends KILL for each owned group.
Escaped/daemonized descendants may survive.

### agents delete

Requires a non-working parent. It stops any working Side runs, then removes the Agent
and all associated Side metadata, contexts, Events, and stored command output. It
never changes workspace/Git files or escaped processes.

### config

Keys are base-url, model, max-agents, context-token-budget,
tool-output-preview-bytes, and stall-notification-seconds. base-url/model must be
nonempty. The two budgets must be positive. max-agents may be zero for unlimited;
stall-notification-seconds is 0 through 86400 and zero disables diagnostics.

Compiled defaults are base-url https://api.openai.com/v1, model gpt-5.4-mini,
max-agents 8, context-token-budget 64000, tool-output-preview-bytes 16384, and
stall-notification-seconds 180.

Precedence is compiled defaults, persisted TOML, then OPENAI_BASE_URL, OPENAI_MODEL,
SUBAGENT_MAX_AGENTS, and SUBAGENT_STALL_NOTIFICATION_SECONDS. list/get emit one
config_value per key, separating default, persisted, caller-local effective, and
running-daemon active values and sources. active_differs_from_local is null without a
daemon and otherwise reports any active/local mismatch. restart_required is also null
without a daemon and is true only when an unmasked persisted/default value changed
since daemon startup; environment divergence alone does not request a restart. set loads
persisted values, changes one key, and atomically rewrites the complete file; an
environment override can mask it. Concurrent config set calls are atomic individually
but not transactionally merged, so last writer wins.

The daemon captures config and API key at startup. Existing Agents keep their stored
model. Resumed and side runs use the running daemon's API key with the stored model.
Empty API key is rejected. Credentials are not validated until a model request.

### inbox

`subagent inbox list` reads unread records from the durable global notification journal. It emits compact
Notification JSONL newest-first followed by exactly one
`{"type":"inbox_summary","count":N,"acknowledged_through":SEQUENCE,"next_cursor":CURSOR_OR_NULL}` record.
Defaults are limit 20, offset zero, and minimum
priority 2. Limit is 1 through 100. `--priority N` includes N and higher. `--agent`
accepts an Agent ref, durable ID, or exact name and includes that Agent's Side
notifications. `--all` also includes acknowledged records. A cursor continues toward
older records and binds Agent, priority, and acknowledgement filters. Offset remains
for compatibility and must be zero in cursor mode. List-only flags are rejected by
ack and follow rather than silently ignored.

`subagent inbox ack <SEQUENCE|NOTIFICATION_ID>` advances one installation-local,
durable acknowledgement watermark to that record; it never moves backward.
`subagent inbox follow [--after SEQUENCE] [--priority N] [--agent AGENT]` emits matching unread history oldest-first,
then flushes new matching Notifications as JSONL until disconnected. Without after,
follow begins after the acknowledgement watermark.

`subagent inbox wait` waits for the first matching Notification and exits. Without
after, it snapshots the newest sequence at invocation and considers future records
only. Timeout returns a successful wait_summary with matched false. Agent, priority,
and repeated type filters are supported.

The visible journal is capped to the newest 10,000 global records. Deleting an Agent
or Side does not delete its existing Notifications.

## 5. Notifications

~~~json
{"type":"notification","id":"ntf_...","sequence":42,"agent_id":"agt_...","agent_ref":"a_1","agent_name":"Website","side_id":null,"side_ref":null,"timestamp":"RFC3339","event_type":"progress","envelope_type":"PROGRESS","priority":2,"status":"working","summary":"Homepage complete","acknowledged":false}
~~~

IDs are ntf_ plus ULID and sequence is globally increasing; gaps are permitted after
an I/O failure. summary is at most 5,000 Unicode scalar values. Side records contain
the parent Agent ID/name plus a non-null side_id. Agent names are captured at creation
of each Notification and do not change when the Agent is later renamed.

Typed envelopes are NEW_TASK, MESSAGE, FOLLOWUP, PROGRESS, INTERRUPTED, FINAL_ANSWER,
and FAILED. Automatic mappings are spawned/resumed priority 1, finished priority 2,
interrupted/stopped priority 3, and failed priority 4. FINAL_ANSWER payload contains
the complete nonempty answer, run number, answer Event ID/ref, and timestamp. The
summary is capped at 5,000 scalar values. Recovery
stop Notifications explain daemon interruption.

The watchdog publishes one priority-3 possible_stall notification when a working
Agent or Side has no phase-appropriate provider, model, tool, or message progress for
stall-notification-seconds. It deduplicates until progress advances and never performs
automatic recovery.

The notify tool publishes progress priority 1, milestone priority 2, input_required
priority 3, or blocked priority 4. Notifications are a
high-signal coordination feed; model messages, reasoning, tool calls, and tool results
are not copied into it.

## 6. Durable messages

Messages are stored in each Agent directory. Shape:

~~~json
{"type":"message","id":"msg_...","ref":"m_1","agent_id":"agt_...","agent_ref":"a_1","content":"text","intent":"message|followup","status":"pending|delivered|cancelled","sent_at":"RFC3339","delivered_at":"RFC3339|null","cancelled_at":"RFC3339|null"}
~~~

list emits newest-first or OR-filtered status matches. Limit defaults to 100 and is
bounded 1 through 1000. A nullable next_cursor continues toward older Messages and is
bound to the Agent and status filters. status emits one.
cancel changes pending to cancelled; delivered/cancelled returns conflict.

Delivery is FIFO. Before marking delivered, the worker atomically saves the user model
message plus an internal delivered-ID marker, then ensures a user_message Event with
message_id exists, then marks the Message delivered. Recovery uses the marker/Event to
avoid duplicate context delivery.

Pending Messages survive daemon failure. Recovery first marks interrupted Agents
stopped daemon_interrupted. Only pending `followup` Messages automatically resume
Agents, oldest-Agent first as max-agents capacity permits; an inactive Agent with only
`message` intent remains inactive until a later follow-up starts a run. Worker exit
schedules more pending follow-ups.

For a delivered send Event:

~~~json
{"content":"text","source":"send","message_id":"msg_..."}
~~~

The `message_sent` and `followup_sent` receipts are acceptance snapshots and always
say queued; polling may already show delivered by the time the client receives one.
Each includes intent, message_id/ref, agent_id/ref, agent_resumed, run_number,
agent_status, and resume_state. Resume state is `not_needed`, `started`,
`waiting_for_capacity`, or `not_woken`; only a non-waking Message may use
`not_woken`.

## 7. Events, logs, and context

Event shape:

~~~json
{"event_id":"evt_...","ref":"e_1","agent_id":"agt_...","agent_ref":"a_1","side_id":"side_... (Side Events only)","side_ref":"s_1 (Side Events only)","sequence":1,"timestamp":"RFC3339","type":"system_message|user_message|assistant_message|reasoning|tool_call|tool_result|lifecycle|error","data":{}}
~~~

Data variants:

- system_message: content string.
- user_message spawn: content, source spawn.
- user_message create: Side question content, source create.
- user_message send: content, source send, message_id.
- assistant_message: content string, usage null or provider object.
- reasoning: content string.
- tool_call: tool_call_id, name, arguments JSON-encoded string.
- tool_result: tool_call_id, name, result object.
- lifecycle working: reason spawned/resumed/deadline_updated plus applicable fields.
- lifecycle finished: status finished.
- lifecycle stopped: status stopped and reason.
- error: status failed and error string.

logs applies cursor first, type filter second, and limit last. It selects newest N
matches after the exclusive cursor and emits that selection oldest-first. Unknown or
cross-Agent cursors return event_not_found. Agent defaults are system/user/assistant;
Side defaults are user/assistant. Default limit is 20. Explicit types replace defaults.
all selects every type. Every finite query ends with logs_summary, including count
zero. Follow emits Events only.

follow emits the historical selection, polls about every 500 ms, flushes every new
match, and exits 0 after terminal status. Socket loss after output can terminate
without a final stdout Error; clients must inspect process status/stderr. Ctrl+C uses
normal signal behavior. Deleting while following causes stream termination.

context emits context_meta containing agent_id, agent_ref, agent_name, message_count,
and compacted_at, then every raw stored model message. It does not expose
internal delivery markers. Context can already be compacted: older tool payloads may
be replaced by output references and older turns summarized/removed. Events are the
lifetime journal. Context output can contain large strings and image data URLs.

## 8. Side runs

`sides create AGENT` copies the parent's current persisted model messages, drops an
incomplete trailing tool turn, compacts to the daemon budget, adds the Side
system/question messages, durably creates a working Side, and immediately emits:

~~~json
{"type":"side_created","id":"side_...","ref":"s_1","agent_id":"agt_...","agent_ref":"a_1","status":"working","created_at":"RFC3339"}
~~~

`sides create AGENT` is the sole Agent-context creation command. Side is one-shot: it accepts no
follow-up messages. At most two Side runs may be working for one parent. This limit is
independent of max-agents; a third creation returns capacity_exceeded and creates
nothing.

Optional `--model` overrides the parent model for that Side only. Without it, the Side
inherits the parent model. The selected value is stored in Side metadata.

Side creation persists lifecycle created, system_message, then user_message source
create Events. `sides list AGENT` supports repeatable OR status filters plus limit,
cursor, and compatibility offset and emits
newest-first side_list_item JSONL followed by list_summary. Limit defaults to 100 and
accepts 1 through 1000. Compact records include model and current_phase. A cursor is
bound to the parent and status filters. question_preview is the first 200 Unicode scalar
values. `sides status SIDE` emits complete Side metadata including full question,
nullable answer, lifecycle timestamps, inherited_context_messages, tool_calls,
stop_reason, and last_error.

`sides logs SIDE` uses the same type, cursor, limit, follow, ordering, and flushing
rules as agents logs. Side Events add side_id while agent_id remains the parent ID.
Every new Side persists its system prompt, user question, reasoning, assistant answer, tool calls, tool
results, lifecycle/error Events, context snapshot, and complete tool outputs. Default
logs are user/assistant; use --all or --type for tools and reasoning.

`sides stop SIDE` requires working and transitions to stopped user_request.
`sides delete SIDE` requires terminal and deletes its complete stored history.
Deadline expiry is stopped wall_time. Fatal errors are failed. Daemon shutdown stops
working Sides daemon_shutdown; crash recovery marks them stopped daemon_interrupted
without resuming. Parent deletion stops working Sides with parent_deleted and removes
all Side history.

The filesystem remains live, not snapshotted. Parent pending Messages are absent and
Side Events never enter parent context or Events. Each Side owns its terminal manager
and cannot see parent terminals. Readonly remains advisory because Bash is
unrestricted; the prompt forbids mutation but this is not a sandbox.

## 9. Model API

The daemon uses OpenAI Chat Completions. It POSTs to BASE_URL with trailing slashes
removed plus /chat/completions, Authorization: Bearer API_KEY, Accept:
text/event-stream, JSON messages/tools, tool_choice auto, and stream true.

It accepts SSE data lines ending with [DONE] and also a non-streaming JSON response
when content-type is not text/event-stream. Tool calls use Chat Completions function
objects with string JSON arguments. A stream that produces no data for five minutes
fails as a retryable api_error.

Network failures, HTTP 429, HTTP 5xx, and stream I/O/idle failures retry up to five
total attempts. Default delays are one, two, four, and eight seconds. A numeric or
HTTP-date Retry-After overrides the delay and is clamped to 1 through 60 seconds.
Authentication and other HTTP 4xx failures do not retry.

Assistant content is accumulated from delta.content. Reasoning is accumulated from
delta.reasoning or delta.reasoning_content. usage is retained as a provider-defined
JSON object or null. The CLI schema fixes its outer type but deliberately permits
provider fields. HTTP/network errors become api_error; retryable is true for network,
429, and 5xx.

## 10. Agent tools

DIR is the default, not a boundary. Relative paths join DIR. Absolute paths, .., and
symlinks may access anywhere permitted to the daemon user.

### read

Input path required; offset default 1 and one-based; limit default 500, clamped 1–2000
logical lines. Uses Rust BufRead lines: CR/LF terminators are removed. Offset beyond
EOF returns an empty lines array. Invalid UTF-8 or read failure returns tool_error.
Returned entries are line-number-prefixed strings. The 64 KiB counter can stop before
line limit; truncated reports only that byte counter, not EOF/line-limit truncation.

### glob

Input pattern required; path defaults DIR; limit 500 clamped 1–5000. Uses globset
syntax and ignore WalkBuilder, so ** is recursive, ignore/hidden rules apply, symlinks
are not followed, and matching files or directories may be returned. Paths are
relative to root. Traversal order is filesystem order, not sorted. Invalid patterns
return tool_error.

### grep

Input pattern is Rust regex: Unicode-aware, case-sensitive, line-by-line, no multiline.
path defaults DIR. include is optional globset matched against relative paths. limit
200 clamped 1–2000. WalkBuilder applies ignore/hidden rules and does not follow
symlinks. Binary/invalid UTF-8 files are skipped/broken at decode error. Each matching
line yields one record containing the complete line truncated to 2000 UTF-8-safe
characters; multiple matches on one line still yield one record.

### write

Write mode only. path/content required. Creates missing parents and replaces the
target using a non-atomic fs write, following symlinks. Content bytes are UTF-8 bytes;
bytes is content byte length. Existing permissions normally remain on truncation.

### edit

Write mode only. Literal, case-sensitive replacement over a UTF-8 file. old_text must
be nonempty. expected_replacements defaults 1 and is checked before writing. Exactly
that many occurrences are all replaced. No regex or newline normalization. The final
write is non-atomic and follows symlinks.

### apply_patch

Write mode only. Patch must begin *** Begin Patch and end *** End Patch. Directives:
*** Add File: PATH with subsequent + content lines; *** Delete File: PATH; and
*** Update File: PATH containing one or more @@ hunks with space context, - removal,
and + addition lines. Update old text must match exactly once per hunk. Rename and
binary patches are unsupported. Paths join DIR and may escape. Changes apply
sequentially and are not atomic across files; failure can leave earlier changes.

### exec_command

command required. workdir defaults DIR and relative workdir joins DIR. Runs
/bin/bash -lc COMMAND in a new process group. Bash login semantics may load shell
startup configuration. It inherits daemon environment except OPENAI_API_KEY and
SUBAGENT_WEB_PASSWORD. stdin is
piped and left open. stdout and stderr append to the same private output file; kernel
write order determines merged ordering.

yield-time-ms defaults 10000 and clamps 250–30000. If process exits during that sleep,
return completed; otherwise return running with terminal_id. No command-length or
per-process total timeout exists beyond OS limits and Agent wall time. Daemonized
descendants can escape cleanup.

Completed:

~~~json
{"ok":false,"status":"completed","exit_code":7,"output":{"content":"text","head_bytes":4,"tail_bytes":0,"total_bytes":4,"truncated":false},"output_ref":"out_...","truncated":false}
~~~

Running includes the same output/output_ref/truncated plus terminal_id and ok true.
If Agent/Side stop cancels the session during the yield, it wakes immediately and
returns ok false with status cancelled, terminal_id, nullable exit_code, and the same
output fields. Cancellation never panics the daemon.

### Preview

If output fits budget, content is lossy UTF-8 of every byte, head_bytes is total,
tail_bytes zero. Otherwise the preview uses 75 percent head and 25 percent tail,
inserts a newline marker reporting omitted raw bytes, and converts each slice with
UTF-8 replacement. Counts refer to raw selected bytes; content includes the marker.

### write_stdin

terminal_id required. input defaults empty. Bytes are written exactly; no newline is
added. There is no EOF option. Default yield is 5000 ms for empty polling and 250 ms
after input, clamped 0–30000. Unknown/already-removed terminals return tool_error.
output is lossy UTF-8 for newly read bytes. next_offset is raw byte offset and
truncated means more bytes currently exist.

### terminals

At most eight live terminal sessions per Agent. list_terminals returns only sessions
whose exit code is still unknown; completed sessions are omitted and removed when
polled. Sessions are in-memory, Agent-local, unordered, and never survive Agent/daemon
termination. terminate_terminal returns terminated false if not found/raced.
terminate_all returns ok true after TERM/KILL and reaping of every owned group; a
signalling or wait failure returns tool_error instead of claiming cleanup succeeded.

### read_output

output_ref required. offset default zero and limit default 65536, clamped 1–65536 raw
bytes. content is lossy UTF-8. next_offset is offset plus raw bytes read. Output files
are append-only while processes run. eof means the read reached current file length,
not that the process exited. Offset beyond current EOF returns empty content and eof
true.

### view_image

path required. Reads at most 5 MiB. MIME recognition uses filename extension through
mime_guess, not decoder probing. MIME must start image/. The raw data becomes a data
URL in a model-visible user message after the current tool-call batch. Multiple image
results append multiple messages in call order.

### notify

Available in readonly and write modes, including Sides. Input requires event_type
`progress|milestone|input_required|blocked` and a nonempty summary of at most 5,000
Unicode scalar values. The daemon derives priorities 1, 2, 3, and 4 respectively and
captures the owner's current status. Success returns:

~~~json
{"ok":true,"notification_id":"ntf_...","priority":2,"event_type":"milestone"}
~~~

## 11. Optional localhost HTTP API

Starting with `daemon start --web-ui-port PORT` enables both the human dashboard and
an automation API on `127.0.0.1`. The Unix socket remains the preferred local CLI
transport; the HTTP API is useful for a separate harness or browser. When
`SUBAGENT_WEB_PASSWORD` is set, every asset and API request requires HTTP Basic Auth
with username `subagent`. State-changing browser requests must also carry a matching
localhost `Origin` header. Non-browser API clients do not need an Origin header.

Single-resource responses are JSON. Lists and event history are JSONL with the same
item and summary objects as the CLI. Streams use Server-Sent Events. Errors use the
shared Error object and an HTTP status: missing resources are 404, authentication is
401, invalid input is 400, conflict/capacity is 409, and internal errors are 500.
Agent paths accept a_N refs, durable IDs, or exact unambiguous names; Side, Message,
and Event paths accept their short ref or durable ID.

Core routes:

~~~text
GET|POST /api/agents
GET|DELETE /api/agents/{agent}
GET /api/team
POST /api/agents/{agent}/message
POST /api/agents/{agent}/followup
POST /api/agents/{agent}/interrupt
POST /api/agents/{agent}/stop
GET /api/agents/{agent}/events
GET /api/agents/{agent}/stream
GET|POST /api/agents/{agent}/sides
GET|DELETE /api/sides/{side}
GET /api/sides/{side}/events
GET /api/inbox
GET /api/inbox/follow
POST /api/inbox/ack/{sequence-or-notification-id}
~~~

Inbox queries accept `priority`, `agent`, `event_type`, `limit`, `offset`,
`after_cursor`, and `all`. `event_type` filters the typed envelope before pagination,
so a page cannot become spuriously empty from client-side filtering. The Web UI does
not expose daemon startup/configuration or raw model context.

## 12. Storage and security

Config uses XDG_CONFIG_HOME/subagent or HOME/.config/subagent. State uses
XDG_STATE_HOME/subagent or HOME/.local/state/subagent. Runtime socket/lock use
XDG_RUNTIME_DIR or state/run. Private directories are mode 0700 and files/socket are
0600 where created explicitly.

Each Agent directory stores metadata.json, context.json, messages.json, events.jsonl,
and outputs. Metadata/context/messages use private atomic temporary-write plus rename.
Events append and flush each JSONL line. Per-owner sequence counters avoid rescanning
complete Event files on append. Event/log queries scan incrementally and retain only
their bounded result window in memory.

Model context is append-only between compactions. Its approximate size is compact JSON
bytes divided by four. Once it exceeds context-token-budget, the daemon preserves the
first system message, chooses a complete-message boundary nearest 60% of the remaining
serialized weight, and sends that older prefix to the Agent's selected model with no
tools. The returned semantic handoff replaces only that prefix; the newest roughly 40%
is retained verbatim. Boundaries never begin the retained suffix with a tool result, so
assistant tool calls and their results remain together. Subsequent compactions fold the
prior rolling summary into a new summary. The new context is persisted before the task
model request. Empty, unsplittable, or still-oversized summaries fail the run with
context_compaction_failed rather than applying mechanical clipping. Side runs use the
same policy on their inherited context plus Side question.

The global notifications.jsonl journal, notification-sequence counter, and durable
acknowledgement watermark live in the state directory. Queries expose only the latest 10,000 global records. The physical
journal is compacted in 1,000-entry batches, retaining those latest 10,000, so it may
temporarily contain up to 10,999 lines.

One daemon is supported per user/runtime directory. IDs survive daemon restarts and
binary replacement while state remains. The daemon is detached but not installed as a
boot service and may be affected by host logout policy.

daemon-state.json records starting, running, and cleanly stopped lifecycle state so
an absent socket can distinguish a prior crash. daemon.log rotates at startup when it reaches 10 MiB, retaining daemon.log.1. Agent
Events and complete terminal outputs have no automatic age or byte retention; delete
obsolete Agents to reclaim their daemon-managed history and output files.

Readonly removes structured write/edit/apply_patch definitions but does not constrain
Bash, absolute paths, network, credentials, or other Agents. It is not a security
boundary. Use trusted prompts and directories.

Terminal ownership is held in daemon memory. Graceful stop sends TERM and then KILL
to still-live owned process groups, but an ungraceful daemon SIGKILL, host crash, or a
descendant that deliberately escapes its process group can leave processes behind.
The daemon does not claim OS-level isolation or crash-proof descendant cleanup.

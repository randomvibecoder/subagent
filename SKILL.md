---
name: subagent-cli
description: Use the `subagent` CLI to install, configure, start, monitor, question, continue, stop, and delete persistent background coding agents. Trigger for delegated or parallel coding work and for requests involving `subagent daemon`, `subagent agents`, or `subagent config`.
---

# Subagent CLI

`subagent` is an agent-only, JSONL CLI for persistent background coding agents. Each
agent has a task, canonical working directory, conversation, tools, status, and stable
`agt_<ULID>` ID. A detached per-user daemon runs agents and persists their records.

Use `agents spawn` for independent work, `agents side`/`btw` for one disposable
question about an agent, and `agents send` to continue an agent durably.

## Install

Linux x86-64 (`x86_64`/`amd64`) only. Install without `sudo`:

~~~sh
curl -fsSL https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh | sh
~~~

The installer always downloads the latest GitHub release, verifies its SHA-256
checksum, and installs `$HOME/.local/bin/subagent`. To use another user-owned
destination:

~~~sh
curl -fsSL https://raw.githubusercontent.com/randomvibecoder/subagent/main/install.sh |
  SUBAGENT_INSTALL_DIR="$HOME/bin" sh
~~~

Unsupported operating systems or architectures fail without installing.

## Configure and start

~~~sh
export OPENAI_API_KEY='...'
export OPENAI_BASE_URL='https://example.com/v1'
export OPENAI_MODEL='your-model'
subagent daemon start
~~~

`OPENAI_API_KEY` is required to start the daemon. Base URL and model have compiled
OpenAI-compatible defaults; use `subagent config list` to inspect effective values.
Never put secrets in tasks. Agent Bash inherits the daemon environment except
`OPENAI_API_KEY`; other host credentials and files are not isolated.

## Output contract

Operational stdout is UTF-8 JSONL: one compact JSON object per line, no banners,
tables, ANSI escapes, or progress text. Newlines inside strings are escaped. Streaming
commands flush each line. There is no declared maximum JSONL line size. Help and
`subagent --version` are plain text.

Local parsing, configuration, startup, and connection errors emit one Error object to
stderr and exit 2. Daemon command errors emit one Error object to stderr and exit 4.
Successful commands exit 0. A follow stream may already have emitted events if its
connection later ends.

Current v0.1.0 shapes are below. Consumers must accept additional fields and should pin
the binary version when strict compatibility matters; there is no separate protocol
version or capability-discovery command.

### Agent

~~~json
{"type":"agent","id":"agt_<ULID>","title":"string","dir":"/canonical/path","mode":"readonly|write","advisory_readonly":true,"model":"string","status":"working|finished|stopped|failed","spawned_at":"RFC3339","run_started_at":"RFC3339","updated_at":"RFC3339","finished_at":"RFC3339|null","stopped_at":"RFC3339|null","failed_at":"RFC3339|null","deadline_at":"RFC3339|null","run_number":1,"stop_reason":"string|null","last_error":"string|null"}
~~~

`run_started_at` identifies the current run. Resuming increments `run_number`,
clears all terminal timestamps, `stop_reason`, and `last_error`, and sets a new
`run_started_at`. While working, all terminal timestamps are null; afterward only the
timestamp matching the current terminal state is non-null. `deadline_at` is cleared
when a run ends. Queuing a message alone does not update `updated_at`.

Stop reasons currently are `user_request`, `wall_time`, `daemon_shutdown`, and
`daemon_interrupted`. A final model turn without tool calls is `finished`, including
a refusal or empty answer. Fatal worker/API/storage errors are `failed`. A nonzero
shell exit is a tool result for the model and does not by itself fail the agent.

| Current state | Action | New state |
| --- | --- | --- |
| none | spawn | working |
| working | final model turn without tools | finished |
| working | user stop | stopped (`user_request`) |
| working | deadline expires | stopped (`wall_time`) |
| working | fatal worker/API/storage error | failed |
| working | daemon shutdown | stopped (`daemon_shutdown`) |
| persisted working after daemon crash | next daemon start | stopped (`daemon_interrupted`) |
| finished, stopped, or failed | send | working in the next run |

### Event

~~~json
{"event_id":"evt_<ULID>","agent_id":"agt_<ULID>","sequence":1,"timestamp":"RFC3339","type":"lifecycle|user_message|assistant_message|reasoning|tool_call|tool_result|error","data":{}}
~~~

Events are stored and emitted in ascending `sequence` order. Exact `data` shapes:

- `lifecycle`: `{"status":"working","reason":"spawned"}`,
  `{"status":"working","reason":"resumed","run_number":2}`,
  `{"status":"working","reason":"deadline_updated","deadline_at":"RFC3339"}`,
  `{"status":"finished"}`, or
  `{"status":"stopped","reason":"user_request|wall_time|daemon_shutdown|daemon_interrupted"}`.
- `user_message`: `{"content":"string","source":"spawn|send"}`.
- `assistant_message`: `{"content":"string","usage":null|<opaque API value>}`.
- `reasoning`: `{"content":"string"}`.
- `tool_call`: `{"tool_call_id":"string","name":"string","arguments":"JSON-encoded string"}`.
- `tool_result`: `{"tool_call_id":"string","name":"string","result":<tool result>}`.
- `error`: `{"status":"failed","error":"string"}`.

### Error

~~~json
{"type":"error","code":"cli_error|not_found|max_agents_reached|conflict|invalid_argument|internal_error","message":"string"}
~~~

`details` is not currently emitted. Do not expect argument names, retry hints, or
underlying structured causes.

## Daemon commands

### `subagent daemon start`

Start a detached daemon, wait up to five seconds for its socket, then emit:

~~~json
{"type":"daemon","status":"running","pid":1234,"socket":"/path/subagent.sock","working_agents":0,"max_agents":0,"model":"string","base_url":"string"}
~~~

It fails if a daemon is already reachable. Agent commands never auto-start it. Startup
failures reference the daemon log in the XDG state directory.

### `subagent daemon status`

Emit the same running daemon object. With no reachable daemon, emit a CLI connection
error; there is no `{"status":"stopped"}` object.

### `subagent daemon stop`

~~~json
{"type":"daemon","status":"stopping","working_agents":2}
~~~

This returns before shutdown finishes. The daemon stops working agents with reason
`daemon_shutdown`, terminates their owned terminal process groups, then removes its
socket. On the next start, persisted `working` records left by a crash become
`stopped` with reason `daemon_interrupted`. To wait for complete shutdown, poll
`daemon status` until it exits 2 with the normal daemon-not-running connection Error.

## Agent commands

### `subagent agents spawn`

~~~sh
subagent agents spawn --dir DIR \
  (--message TEXT | --message-file PATH) \
  [--title TITLE] [--mode readonly|write] [--wall-time HOURS]
~~~

- `DIR` must already be a directory. The CLI resolves relative paths and symlinks
  against its own current directory and sends a canonical path. Git is not required.
- `--message-file -` reads UTF-8 from stdin; files are read by the caller. Messages
  must be nonempty and at most 1 MiB.
- Title defaults to the first nonempty task line, truncated to 80 characters.
- Mode defaults to `readonly`. No wall time means no deadline; otherwise decimal
  hours must satisfy `0 < HOURS <= 100`.
- `max-agents:0` is unlimited. A positive full capacity rejects spawn; work is not
  queued.

Emit one working Agent with run 1. This means the worker was registered, not that an
API request has necessarily begun. The agent inherits no caller conversation, so make
the task self-contained. If its canonical directory is later removed or becomes
inaccessible, subsequent file and command tools fail; the daemon does not recreate it.

### `subagent agents list`

~~~sh
subagent agents list \
  [--status working|finished|stopped|failed]... [--dir DIR] \
  [--spawned-after RFC3339] [--spawned-before RFC3339] \
  [--finished-after RFC3339] [--finished-before RFC3339] \
  [--sort spawned_at|updated_at|finished_at] [--order asc|desc] \
  [--limit N] [--offset N]
~~~

Defaults are `spawned_at`, `desc`, limit 100, and offset 0. Repeated statuses are OR.
Directory matching is exact after caller-side canonicalization. Time boundaries are
inclusive; finished filters exclude null timestamps. Ties use ID deterministically.
For `finished_at`, null sorts first ascending and last descending. Limit and offset
are non-negative machine-sized integers with no explicit maximum; limit 0 acts as 100.
Pagination is not snapshot-isolated, so concurrent updates may cause skips/duplicates.
Emit zero or more Agent objects; no match emits nothing.

### `subagent agents status ID`

Require the full ID and emit one Agent. Prefix matching is not supported.

### `subagent agents logs ID`

~~~sh
subagent agents logs ID [--type EVENT_TYPE]... [--after EVENT_ID] [--limit N] [--follow]
~~~

Repeated types are OR. Filtering occurs after the exclusive cursor and before the
limit. The default/zero limit selects the newest 100 matching events but emits that
selection oldest-first. A cursor must belong to the agent; an unknown cursor returns
`not_found` rather than replaying history.

Without `--follow`, emit zero or more Events and exit. With it, emit the same
historical selection, flush new matching Events about every 500 ms, and remain open
even after the agent finishes until disconnected. Events are never compacted, so a
valid cursor remains available until agent deletion.

### `subagent agents context ID`

~~~sh
subagent agents context ID [--include EVENT_TYPE]... [--max-tokens N]
~~~

Default includes are `user_message` and `assistant_message`; repeated includes are
unioned. Any Event type may be named. Default/zero maximum is 12000.

First line:

~~~json
{"type":"context_meta","agent_id":"agt_<ULID>","estimated_tokens":123,"max_tokens":12000,"truncated":false,"included_types":["user_message","assistant_message"]}
~~~

Remaining lines are chronological whole Events. The estimate is serialized event
bytes divided by four, not a model tokenizer. Selection works newest-first, skips
events that do not fit, tries to retain the first user message, never cuts an Event,
then restores chronology. `truncated:true` means at least one matching Event was
omitted. System prompts and side runs are absent; all persisted parent runs are
eligible. This command's maximum is independent of the daemon's internal context
budget and may exceed it.

### `subagent agents send ID`

~~~sh
subagent agents send ID \
  (--message TEXT | --message-file PATH) [--wall-time HOURS]
~~~

For a working agent, enqueue messages FIFO for the next model-loop boundary. Delivery
does not interrupt an API request, multi-tool turn, or shell command. The returned
Agent does not show queue position or delivery. Messages cannot be inspected or
cancelled, and retries are not idempotent. Multiple messages may be pending, but the
queue exists only in daemon memory: pending messages do not survive daemon stop/crash
and are discarded by agent stop. A `user_message` Event is appended only when the
worker consumes a queued message, not when `send` acknowledges it.

For `finished`, `stopped`, or `failed`, resume with persisted context and original
directory as a new run. Old terminals are not restored. `--wall-time` resets a
working run's deadline immediately or sets the resumed run's deadline. Input rules and
output are the same as spawn: emit one updated Agent.

### `subagent agents side ID` / `subagent agents btw ID`

~~~sh
subagent agents side ID \
  (--message TEXT | --message-file PATH) [--wall-time HOURS]
~~~

Answer one question using a copy of the parent's current internal model context. The
copy is taken when the daemon handles the request, compacted to the internal budget,
and drops an incomplete trailing tool turn. It is not the output of `agents context`.
It contains retained model messages and tool calls/results; separate reasoning Events
and queued messages not yet consumed by the parent are absent. The workspace is live
rather than snapshotted, so concurrent file changes may be seen.

The side agent receives `read`, `glob`, `grep`, inspection-only advisory Bash,
`write_stdin`, `list_terminals`, `terminate_terminal`,
`terminate_all_terminals`, `read_output`, and `view_image`. It never receives
`write`, `edit`, or `apply_patch`. Its terminals and stored outputs are private;
it cannot poll the parent's terminals. It does not count against `max-agents`.

Its system prompt says it must not modify files, repositories, processes,
configuration, or external state. Bash exists only for non-mutating inspection such as
`rg`, `grep`, `find`, and non-in-place `sed`. This is a model instruction, not
an enforced filesystem sandbox.

No wall time means no deadline; otherwise `0 < HOURS <= 100`. Failure or timeout
returns an Error. Successful output is:

~~~json
{"type":"side_answer","side_id":"side_<ULID>","agent_id":"agt_<ULID>","answer":"string","model":"string","mode":"readonly","parent_mode":"readonly|write","ephemeral":true,"inherited_context_messages":12,"tool_calls":3,"usage":null}
~~~

`usage` is null or an opaque OpenAI-compatible API value. The side ID is informational:
its context, events, tool history, and outputs are discarded before return and cannot
be queried later. Nothing is appended to the parent transcript.

### `subagent agents time ID HOURS`

Require a working agent and decimal `0 < HOURS <= 100`. Replace its deadline with
HOURS from daemon receipt; this may shorten or extend it. At expiry, stop the run and
owned terminals with reason `wall_time`. The deadline persists through queued
messages in that run unless reset. Emit the updated Agent.

### `subagent agents stop ID`

Require a working agent. Signal the run, terminate each owned terminal process group
with TERM then KILL after about 500 ms, mark it stopped synchronously, and discard
queued messages. Emit the Agent with reason `user_request`. Escaped/detached child
processes are not guaranteed to be found. A stopped agent can later resume via send.

### `subagent agents delete ID`

Require a non-working agent and emit:

~~~json
{"type":"agent_deleted","id":"agt_<ULID>"}
~~~

Deletion removes only daemon-managed metadata, context, events, and stored terminal
outputs. It does not remove or revert the working directory, project files, Git state,
commits, branches, or escaped processes. There is no automatic retention cleanup.

## Agent tool calls

Tool-call Event `arguments` is a JSON-encoded string using these parameters.
Tool-result Event `result` is the corresponding object. Relative paths use the
agent's canonical directory. Any tool failure returns `{"ok":false,"error":"string"}`.

- `read(path, offset=1, limit=500)` → `{"ok":true,"path":"path","offset":1,"lines":["1: text"],"truncated":false}`. Limit clamps to 1–2000; content preview is at most 64 KiB.
- `glob(pattern, path=".", limit=500)` → `{"ok":true,"root":"path","paths":["relative/path"],"truncated":false}`. Limit clamps to 1–5000.
- `grep(pattern, path=".", include?, limit=200)` → `{"ok":true,"root":"path","matches":[{"path":"relative/path","line":1,"text":"string"}],"truncated":false}`. Pattern is regex; include is glob; limit clamps to 1–2000.
- `write(path, content)` → `{"ok":true,"path":"path","bytes":1}`.
- `edit(path, old_text, new_text, expected_replacements=1)` → `{"ok":true,"path":"path","replacements":1}`.
- `apply_patch(patch)` → `{"ok":true,"changed_files":["relative/path"]}`. Patch supports Begin/End Patch with Add, Update, and Delete File directives.
- `exec_command(command, workdir?, yield_time_ms=10000)` runs `bash -lc`; yield clamps to 250–30000 ms. A completed process returns `{"ok":true,"status":"completed","exit_code":0,"output":<Preview>,"output_ref":"out_<ULID>","truncated":false}`. A live process returns `{"ok":true,"status":"running","terminal_id":"term_<ULID>","output":<Preview>,"output_ref":"out_<ULID>","truncated":false}`.
- `write_stdin(terminal_id, input="", yield_time_ms?)` → `{"ok":true,"terminal_id":"term_<ULID>","status":"running|completed","exit_code":null,"output":"new text","output_ref":"out_<ULID>","next_offset":1,"truncated":false}`. `exit_code` is null while running and an integer when completed; `ok` then reflects whether it is zero. Default yield is 5000 ms when polling and 250 ms after input; it clamps to 0–30000.
- `list_terminals()` → `{"ok":true,"terminals":[{"terminal_id":"term_<ULID>","command":"string","cwd":"path","pid":123,"output_ref":"out_<ULID>"}],"count":1,"limit":8}`.
- `terminate_terminal(terminal_id)` → `{"ok":true,"terminated":true}`.
- `terminate_all_terminals()` → `{"ok":true}`.
- `read_output(output_ref, offset=0, limit=65536)` → `{"ok":true,"output_ref":"out_<ULID>","offset":0,"next_offset":1,"content":"string","eof":true}`.
- `view_image(path)` → `{"ok":true,"path":"path","mime_type":"image/png","bytes":1,"note":"image attached in the next model-visible message"}`. Images must be recognized and at most 5 MiB.

`Preview` is `{"content":"string","head_bytes":1,"tail_bytes":0,"total_bytes":1,"truncated":false}`.
Command output is stored in a private file even when the preview is complete. Each
agent owns at most eight live background terminals. For completed `exec_command`,
`ok` is true only when `exit_code` is zero. Write tools are defined only in write mode;
all other tools are available in both modes.

## Configuration

Keys and persisted types:

- `base-url`, `model`: strings; empty strings are currently accepted.
- `max-agents`: non-negative integer; 0 means unlimited.
- `context-token-budget`: non-negative integer; compiled default 64000.
- `tool-output-preview-bytes`: non-negative integer; compiled default 16384.

There are no explicit numeric maxima and no reset command. Values are parsed as Rust
unsigned integers. Every setting requires daemon restart.

- `subagent config list` emits
  `{"type":"config","base-url":"string","model":"string","max-agents":0,"context-token-budget":64000,"tool-output-preview-bytes":16384}`.
- `subagent config get KEY` emits
  `{"type":"config_value","key":"model","value":"string"}`.
- `subagent config set KEY VALUE` updates the persisted value and emits
  `{"type":"config_value","key":"max-agents","value":8,"note":"restart daemon for this value to take effect"}`.

Precedence is compiled default, then the XDG config file, then
`OPENAI_BASE_URL`/`OPENAI_MODEL`/`SUBAGENT_MAX_AGENTS`. List/get show effective
values. Set changes the persisted value without copying environment overrides into the
file; an active override may therefore mask the stored value until unset. List/get
read the caller's current environment. The daemon captures effective values at startup
and does not observe later environment changes. Base URLs are not validated or
required to end in `/v1`; use the form required by the selected endpoint.

Configuration lives under `$XDG_CONFIG_HOME/subagent` or `$HOME/.config/subagent`.
State and daemon logs live under `$XDG_STATE_HOME/subagent` or
`$HOME/.local/state/subagent`. The socket/lock use `$XDG_RUNTIME_DIR`, falling back
to the state run directory. Directories are mode 0700 and the socket/records are
private to the daemon user. One daemon is supported per user/runtime directory.

## Safety and persistence

Readonly means structured writers are withheld and the model is explicitly forbidden
to mutate. Bash itself is unrestricted, so readonly and side mode are not security
boundaries. Agents otherwise run with the daemon user's filesystem, network, process,
and credential access. Use trusted tasks and directories.

`subagent` does not create worktrees, isolate concurrent agents, filter secrets other
than `OPENAI_API_KEY`, or stop agents from interfering with one another. IDs survive
daemon restarts and binary upgrades while the state directory remains. The detached
daemon does not automatically restart after reboot and may be affected by the host's
logout/session policy.

If installed behavior differs from this skill, use
`subagent <group> <command> --help` and `subagent --version` as the authority.

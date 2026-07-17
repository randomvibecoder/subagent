#!/usr/bin/env bash
set -euo pipefail

BIN=${SUBAGENT_BIN:-./target/release/subagent}
ROOT=/tmp/subagent-e2e
rm -rf "$ROOT"
export HOME="$ROOT/home"
export XDG_CONFIG_HOME="$ROOT/config"
export XDG_STATE_HOME="$ROOT/state"
export XDG_RUNTIME_DIR="$ROOT/run"
export OPENAI_API_KEY=test-key
export SUBAGENT_WEB_PASSWORD=test-web-password
export OPENAI_BASE_URL=http://127.0.0.1:18080/v1
export OPENAI_MODEL=test-model
WEB_PORT=${SUBAGENT_E2E_WEB_PORT:-17342}

python3 tests/schema_contract.py >/dev/null

mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR" "$ROOT/project"
BIN=$(realpath "$BIN")
python3 tests/mock_openai.py &
MOCK_PID=$!
sleep 0.2

cleanup() {
  "$BIN" daemon stop >/dev/null 2>&1 || true
  kill "$MOCK_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

json_field() {
  python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
}

wait_status() {
  local id=$1 expected=$2
  for _ in $(seq 1 120); do
    status=$($BIN agents status "$id" | json_field status)
    if [[ "$status" == "$expected" ]]; then return 0; fi
    if [[ "$status" == "failed" ]]; then
      $BIN agents logs "$id" --limit 1000
      return 1
    fi
    sleep 0.1
  done
  echo "timed out waiting for $id to become $expected" >&2
  return 1
}

wait_side_status() {
  local id=$1 expected=$2
  for _ in $(seq 1 120); do
    status=$($BIN sides status "$id" | json_field status)
    if [[ "$status" == "$expected" ]]; then return 0; fi
    if [[ "$status" == "failed" && "$expected" != "failed" ]]; then
      $BIN sides logs "$id" --all --limit 1000
      return 1
    fi
    sleep 0.1
  done
  echo "timed out waiting for Side $id to become $expected" >&2
  return 1
}

$BIN config get max-agents | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["default_value"] == 8 and row["local_effective_value"] == 8 and row["active_value"] is None'
$BIN config set max-agents 1 >/dev/null
$BIN config set stall-notification-seconds 1 >/dev/null
python3 -c 'import pathlib,sys; body=pathlib.Path(sys.argv[1]).read_text(); assert "test-model" not in body and "127.0.0.1:18080" not in body' "$XDG_CONFIG_HOME/subagent/config.toml"
if SUBAGENT_WEB_PASSWORD='' $BIN daemon start 2>"$ROOT/empty-web-password-error.jsonl"; then
  echo "empty Web UI password unexpectedly started the daemon" >&2
  exit 1
fi
python3 -c 'import json,sys; value=json.load(open(sys.argv[1])); assert value["code"] == "cli_error" and "SUBAGENT_WEB_PASSWORD is empty" in value["message"]' "$ROOT/empty-web-password-error.jsonl"
$BIN daemon start | python3 -c 'import json,sys; value=json.load(sys.stdin); assert value["status"] == "running" and value["version"] == "0.1.8" and value["protocol_version"] == 5 and value["web_ui_url"] is None and value["web_auth"] is None'
$BIN config list | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows) == 6 and all(row["type"] == "config_value" and row["active_value"] is not None and row["active_differs_from_local"] is False and row["restart_required"] is False for row in rows); row=next(x for x in rows if x["key"] == "model"); assert row["active_source"] == "OPENAI_MODEL"'
env -u OPENAI_MODEL -u OPENAI_BASE_URL "$BIN" config get model | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["local_source"] == "persisted" and row["active_source"] == "OPENAI_MODEL" and row["active_differs_from_local"] is True and row["restart_required"] is False'
$BIN config set context-token-budget 65000 >/dev/null
$BIN config get context-token-budget | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["local_source"] == "persisted" and row["active_source"] == "persisted" and row["active_differs_from_local"] is True and row["restart_required"] is True'

mkdir -p "$ROOT/caller/project"
RELATIVE=$(cd "$ROOT/caller" && "$BIN" agents spawn --name relative-test --dir project --message FINAL_ONLY)
RELATIVE_ID=$(printf '%s\n' "$RELATIVE" | json_field id)
printf '%s\n' "$RELATIVE" | python3 -c 'import json,os,sys; row=json.load(sys.stdin); assert row["dir"] == os.path.realpath(sys.argv[1])' "$ROOT/caller/project"
$BIN agents status "$RELATIVE_ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["last_message_sent_at"] == row["spawned_at"] and row["last_message_delivered_at"] == row["spawned_at"]'
wait_status "$RELATIVE_ID" finished
(cd "$ROOT/caller" && "$BIN" agents list --dir project) | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert len(rows) == 2 and rows[-1] == {"type":"list_summary","resource":"agents","count":1,"next_cursor":None}'
$BIN agents rename "$RELATIVE_ID" a_team >/dev/null
$BIN agents status a_team | python3 -c 'import json,sys; assert json.load(sys.stdin)["name"] == "a_team"'
$BIN agents rename "$RELATIVE_ID" agt_team >/dev/null
$BIN agents status agt_team | python3 -c 'import json,sys; assert json.load(sys.stdin)["name"] == "agt_team"'
if $BIN agents rename "$RELATIVE_ID" a_10 2>"$ROOT/reserved-name-error.jsonl"; then exit 1; fi
python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["code"] == "invalid_argument" and "canonical system ID" in row["message"]' "$ROOT/reserved-name-error.jsonl"

READONLY=$($BIN agents spawn --name readonly-test --dir "$ROOT/project" --mode readonly --message READONLY_PROMPT)
READONLY_ID=$(printf '%s\n' "$READONLY" | json_field id)
wait_status "$READONLY_ID" finished
$BIN agents logs "$READONLY_ID" --type assistant_message | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[-2]["data"]["content"] == "readonly prompt correct" and rows[-1]["type"] == "logs_summary"'

MODEL=$($BIN agents spawn --name model-override --dir "$ROOT/project" --mode readonly --model custom-main-model --message MODEL_ECHO)
MODEL_ID=$(printf '%s\n' "$MODEL" | json_field id)
MODEL_REF=$(printf '%s\n' "$MODEL" | json_field ref)
wait_status "$MODEL_ID" finished
$BIN agents wait "$MODEL_REF" --timeout-seconds 2 | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["status"] == "finished" and row["current_phase"] == "finished"'
$BIN agents status model-override | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["name"] == "model-override" and row["model"] == "custom-main-model" and row["provider_request_id"] is None and row["request_started_at"] is None and row["last_provider_request_id"] == "mock-request-id" and row["last_model_event_at"] is not None'
$BIN agents logs "$MODEL_ID" --type assistant_message | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[-2]["data"]["content"] == "custom-main-model" and rows[-1]["type"] == "logs_summary"'
$BIN agents send "$MODEL_REF" --message MODEL_ECHO >/dev/null
wait_status "$MODEL_ID" finished
$BIN agents status "$MODEL_ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["model"] == "custom-main-model" and row["run_number"] == 2'
$BIN agents logs "$MODEL_ID" --type assistant_message | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[-2]["data"]["content"] == "custom-main-model" and rows[-1]["type"] == "logs_summary"'
$BIN inbox list --agent "$MODEL_ID" --priority 1 --limit 10 | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert rows[-1]["type"] == "inbox_summary" and rows[-1]["count"] == len(rows)-1; assert [row["event_type"] for row in rows[:-1][:3]] == ["finished","resumed","finished"]'

NOTIFY=$($BIN agents spawn --name notify-test --dir "$ROOT/project" --mode readonly --message NOTIFY_TOOL)
NOTIFY_ID=$(printf '%s\n' "$NOTIFY" | json_field id)
wait_status "$NOTIFY_ID" finished
$BIN inbox list --agent "$NOTIFY_ID" | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; notes=rows[:-1]; assert rows[-1]["type"] == "inbox_summary" and rows[-1]["count"] == 2; assert [row["event_type"] for row in notes] == ["finished","milestone"]; assert notes[0]["summary"] == "notification task complete" and notes[1]["summary"] == "explicit milestone"; assert all(row["priority"] >= 2 for row in notes)'
$BIN inbox list --agent "$NOTIFY_ID" --limit 1 --offset 1 --priority 1 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[0]["event_type"] == "milestone" and rows[1]["type"] == "inbox_summary" and rows[1]["count"] == 1'
$BIN inbox list --agent "$NOTIFY_ID" --priority 3 | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["type"] == "inbox_summary" and row["count"] == 0'
INBOX_PAGE=$($BIN inbox list --agent "$NOTIFY_ID" --priority 1 --limit 1)
INBOX_CURSOR=$(printf '%s\n' "$INBOX_PAGE" | tail -n1 | json_field next_cursor)
FIRST_NOTIFICATION=$(printf '%s\n' "$INBOX_PAGE" | head -n1 | json_field id)
$BIN inbox list --agent "$NOTIFY_ID" --priority 1 --limit 1 --after-cursor "$INBOX_CURSOR" | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[0]["id"] != sys.argv[1] and rows[-1]["type"]=="inbox_summary"' "$FIRST_NOTIFICATION"
if $BIN inbox list --agent "$NOTIFY_ID" --priority 2 --after-cursor "$INBOX_CURSOR" 2>"$ROOT/inbox-cursor-filter-error.jsonl"; then exit 1; fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "invalid_argument"' "$ROOT/inbox-cursor-filter-error.jsonl"

printf '%s\n' WRITE_EDIT_PATCH >"$ROOT/task.md"
SPAWN=$($BIN agents spawn --dir "$ROOT/project" --mode write --name tool-test --message-file "$ROOT/task.md")
ID=$(printf '%s\n' "$SPAWN" | json_field id)
wait_status "$ID" finished
[[ "$(cat "$ROOT/project/generated.txt")" == "gamma" ]]

$BIN agents logs "$ID" --type tool_result --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows[:-1]) >= 6 and rows[-1]["type"] == "logs_summary"'
$BIN agents logs "$ID" --type reasoning --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[-2]["data"]["content"] == "mock reasoning" and rows[-1]["type"] == "logs_summary"'
CURSOR=$($BIN agents logs "$ID" --limit 100 | sed -n '1p' | python3 -c 'import json,sys; print(json.load(sys.stdin)["event_id"])')
CURSOR_REF=$($BIN agents logs "$ID" --limit 100 | sed -n '1p' | python3 -c 'import json,sys; print(json.load(sys.stdin)["ref"])')
$BIN agents logs "$ID" --after "$CURSOR_REF" --limit 100 | python3 -c 'import json,sys; cursor=sys.argv[1]; rows=[json.loads(x) for x in sys.stdin]; assert all(row["event_id"] != cursor for row in rows[:-1]) and rows[-1]["type"] == "logs_summary"' "$CURSOR"
if $BIN agents logs "$ID" --after evt_01ARZ3NDEKTSV4RRFFQ69G5FAV 2>"$ROOT/cursor-error.jsonl"; then
  echo "logs unexpectedly accepted an unknown cursor" >&2
  exit 1
fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "event_not_found"' "$ROOT/cursor-error.jsonl"
if timeout 2 $BIN agents logs "$ID" --after evt_01ARZ3NDEKTSV4RRFFQ69G5FAV --follow 2>"$ROOT/follow-cursor-error.jsonl"; then
  echo "follow unexpectedly accepted an unknown cursor" >&2
  exit 1
fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "event_not_found"' "$ROOT/follow-cursor-error.jsonl"
$BIN agents logs "$ID" | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[-1]["type"] == "logs_summary" and all(x["type"] in ("system_message","user_message","assistant_message") for x in rows[:-1])'
$BIN agents logs "$ID" --type error | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["type"] == "logs_summary" and row["count"] == 0'
$BIN agents context "$ID" | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[0]["type"] == "context_meta" and rows[0]["agent_ref"].startswith("a_") and rows[0]["agent_name"] == "tool-test"; assert rows[0]["message_count"] == len(rows)-1; assert all("role" in x for x in rows[1:])'

printf '%s\n' FINAL_ONLY >"$ROOT/followup.md"
RECEIPT=$($BIN agents send "$ID" --message-file "$ROOT/followup.md")
MESSAGE_ID=$(printf '%s\n' "$RECEIPT" | json_field message_id)
MESSAGE_REF=$(printf '%s\n' "$RECEIPT" | json_field message_ref)
printf '%s\n' "$RECEIPT" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["type"] == "message_sent" and row["status"] == "queued" and row["agent_resumed"] and row["run_number"] == 2 and row["resume_state"] == "started"'
wait_status "$ID" finished
$BIN agents status "$ID" | python3 -c 'import json,sys; assert json.load(sys.stdin)["run_number"] == 2'
$BIN messages status "$ID" "$MESSAGE_REF" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["status"] == "delivered" and row["delivered_at"] is not None'
$BIN messages list "$ID" --status delivered | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows) == 2 and rows[-1]["count"] == 1'
$BIN agents send "$ID" --message FINAL_ONLY >/dev/null
wait_status "$ID" finished
$BIN agents send "$ID" --message FINAL_ONLY >/dev/null
wait_status "$ID" finished
MESSAGE_PAGE=$($BIN messages list "$ID" --status delivered --limit 2)
MESSAGE_CURSOR=$(printf '%s\n' "$MESSAGE_PAGE" | tail -n1 | json_field next_cursor)
printf '%s\n' "$MESSAGE_PAGE" | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows)==3 and all(x["status"]=="delivered" for x in rows[:-1]) and rows[-1]["next_cursor"]'
$BIN messages list "$ID" --status delivered --limit 2 --after-cursor "$MESSAGE_CURSOR" | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows)==2 and rows[-1]["next_cursor"] is None'
if $BIN messages list "$ID" --status pending --after-cursor "$MESSAGE_CURSOR" 2>"$ROOT/message-cursor-filter-error.jsonl"; then exit 1; fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "invalid_argument"' "$ROOT/message-cursor-filter-error.jsonl"
$BIN agents status "$ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["last_message_at"] > row["spawned_at"]'

printf '%s\n' side-file-content >"$ROOT/project/side.txt"
python3 -c 'import base64,sys; open(sys.argv[1],"wb").write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="))' "$ROOT/project/pixel.png"
SIDE_PARENT=$($BIN agents spawn --name side-parent --dir "$ROOT/project" --mode write --message SIDE_PARENT_MARKER)
SIDE_PARENT_ID=$(printf '%s\n' "$SIDE_PARENT" | json_field id)
wait_status "$SIDE_PARENT_ID" finished
CONTEXT_BEFORE=$(sha256sum "$XDG_STATE_HOME/subagent/agents/$SIDE_PARENT_ID/context.json" | cut -d' ' -f1)
EVENTS_BEFORE=$(sha256sum "$XDG_STATE_HOME/subagent/agents/$SIDE_PARENT_ID/events.jsonl" | cut -d' ' -f1)
SIDE=$($BIN sides create "$SIDE_PARENT_ID" --message SIDE_TOOL_QUESTION)
SIDE_ID=$(printf '%s\n' "$SIDE" | json_field id)
SIDE_REF=$(printf '%s\n' "$SIDE" | json_field ref)
printf '%s\n' "$SIDE" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["type"] == "side_created" and row["status"] == "working"'
wait_side_status "$SIDE_ID" finished
$BIN sides status "$SIDE_REF" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["answer"] == "side inherited context and tools"; assert row["tool_calls"] == 5; assert row["inherited_context_messages"] >= 3; assert row["mode"] == "readonly" and row["parent_mode"] == "write" and row["model"] == "test-model"'
$BIN sides logs "$SIDE_REF" --type tool_call --limit 100 | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert len(rows[:-1]) == 5 and all(row["side_id"] == sys.argv[1] and row["ref"].startswith("e_") for row in rows[:-1]); assert rows[-1]["type"] == "logs_summary"' "$SIDE_ID"
$BIN sides logs "$SIDE_REF" --type system_message | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert len(rows) == 2 and rows[0]["type"] == "system_message" and rows[0]["data"]["content"].startswith("You are a persistent") and rows[1]["type"] == "logs_summary"'
[[ -e "$XDG_STATE_HOME/subagent/sides/$SIDE_ID/metadata.json" ]]
if $BIN agents btw --help >"$ROOT/btw-help.txt" 2>"$ROOT/btw-error.jsonl"; then
  echo "removed btw alias unexpectedly succeeded" >&2
  exit 1
fi
python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["code"] == "cli_error" and "unrecognized subcommand" in row["message"] and "btw" in row["message"]' "$ROOT/btw-error.jsonl"
SIDE_MODEL=$($BIN sides create "$SIDE_PARENT_ID" --model custom-side-model --message MODEL_ECHO)
SIDE_MODEL_ID=$(printf '%s\n' "$SIDE_MODEL" | json_field id)
wait_side_status "$SIDE_MODEL_ID" finished
$BIN sides status "$SIDE_MODEL_ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["answer"] == "custom-side-model" and row["model"] == "custom-side-model"'
$BIN inbox list --agent "$SIDE_PARENT_ID" --priority 2 --limit 100 | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert any(row.get("side_id") == sys.argv[1] and row.get("event_type") == "finished" for row in rows)' "$SIDE_MODEL_ID"
$BIN sides list "$SIDE_PARENT_ID" | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert len(rows) == 3 and all(row["type"] == "side_list_item" and row["model"] and row["current_phase"] for row in rows[:-1]) and rows[-1]["count"] == 2'
SIDE_PAGE=$($BIN sides list "$SIDE_PARENT_ID" --limit 1)
SIDE_CURSOR=$(printf '%s\n' "$SIDE_PAGE" | tail -n1 | json_field next_cursor)
FIRST_SIDE=$(printf '%s\n' "$SIDE_PAGE" | head -n1 | json_field id)
$BIN sides list "$SIDE_PARENT_ID" --limit 1 --after-cursor "$SIDE_CURSOR" | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows)==2 and rows[0]["id"] != sys.argv[1] and rows[-1]["next_cursor"] is None' "$FIRST_SIDE"
[[ "$CONTEXT_BEFORE" == "$(sha256sum "$XDG_STATE_HOME/subagent/agents/$SIDE_PARENT_ID/context.json" | cut -d' ' -f1)" ]]
[[ "$EVENTS_BEFORE" == "$(sha256sum "$XDG_STATE_HOME/subagent/agents/$SIDE_PARENT_ID/events.jsonl" | cut -d' ' -f1)" ]]
SIDE_DELAY_1=$($BIN sides create "$SIDE_PARENT_ID" --message SIDE_DELAY)
SIDE_DELAY_ID_1=$(printf '%s\n' "$SIDE_DELAY_1" | json_field id)
SIDE_DELAY_2=$($BIN sides create "$SIDE_PARENT_ID" --message SIDE_DELAY)
SIDE_DELAY_ID_2=$(printf '%s\n' "$SIDE_DELAY_2" | json_field id)
$BIN agents list --limit 1000 | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; row=next(row for row in rows if row["id"] == sys.argv[1]); assert row["working_sides"] == 2' "$SIDE_PARENT_ID"
if $BIN sides create "$SIDE_PARENT_ID" --message SIDE_DELAY 2>"$ROOT/side-capacity-error.jsonl"; then
  echo "third Side unexpectedly passed per-Agent capacity" >&2
  exit 1
fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "capacity_exceeded"' "$ROOT/side-capacity-error.jsonl"
$BIN sides stop "$SIDE_DELAY_ID_1" >/dev/null
$BIN sides stop "$SIDE_DELAY_ID_2" >/dev/null
wait_side_status "$SIDE_DELAY_ID_1" stopped
wait_side_status "$SIDE_DELAY_ID_2" stopped

DELAY=$($BIN agents spawn --name delay-test --dir "$ROOT/project" --mode readonly --message DELAY)
DELAY_ID=$(printf '%s\n' "$DELAY" | json_field id)
WORKING_SIDE=$($BIN sides create "$DELAY_ID" --message SIDE_WHILE_WORKING)
WORKING_SIDE_ID=$(printf '%s\n' "$WORKING_SIDE" | json_field id)
wait_side_status "$WORKING_SIDE_ID" finished
$BIN sides status "$WORKING_SIDE_ID" | python3 -c 'import json,sys; assert json.load(sys.stdin)["answer"] == "parent still running"'
$BIN agents time "$DELAY_ID" 1 | python3 -c 'import json,sys; assert json.load(sys.stdin)["deadline_at"] is not None'
sleep 6
$BIN inbox list --agent "$DELAY_ID" --priority 3 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; stalls=[x for x in rows if x.get("event_type") == "possible_stall"]; assert len(stalls) == 1 and "requesting_model" in stalls[0]["summary"]'
if $BIN agents spawn --name capacity-test --dir "$ROOT/project" --message SECOND_AGENT 2>"$ROOT/capacity-error.jsonl"; then
  echo "spawn unexpectedly passed max-agents" >&2
  exit 1
fi
CANCEL_RECEIPT=$($BIN agents send "$DELAY_ID" --message FINAL_ONLY)
CANCEL_MESSAGE_ID=$(printf '%s\n' "$CANCEL_RECEIPT" | json_field message_id)
$BIN messages cancel "$DELAY_ID" "$CANCEL_MESSAGE_ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["status"] == "cancelled" and row["cancelled_at"] is not None'
$BIN agents status "$DELAY_ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["last_message_sent_at"] > row["last_message_delivered_at"] == row["spawned_at"] and row["current_phase"] in ("requesting_model","retrying_model")'
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "capacity_exceeded"' "$ROOT/capacity-error.jsonl"
$BIN agents stop "$DELAY_ID" >/dev/null
wait_status "$DELAY_ID" stopped

BG=$($BIN agents spawn --name background-test --dir "$ROOT/project" --mode write --message BACKGROUND_LIMIT)
BG_ID=$(printf '%s\n' "$BG" | json_field id)
wait_status "$BG_ID" finished
$BIN agents logs "$BG_ID" --type tool_result --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert any("background terminal limit reached (8)" in json.dumps(x) for x in rows)'

POLL=$($BIN agents spawn --name terminal-poll --dir "$ROOT/project" --mode write --message TERMINAL_POLL)
POLL_ID=$(printf '%s\n' "$POLL" | json_field id)
wait_status "$POLL_ID" finished
$BIN agents logs "$POLL_ID" --type tool_call --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; names=[x["data"]["name"] for x in rows[:-1]]; assert names == ["exec_command", "write_stdin"]'
$BIN agents logs "$POLL_ID" --type tool_result --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert "end" in json.dumps(rows[-2])'

STOP_TERMINAL=$($BIN agents spawn --name stop-terminal --dir "$ROOT/project" --mode write --message STOP_DURING_TERMINAL)
STOP_TERMINAL_ID=$(printf '%s\n' "$STOP_TERMINAL" | json_field id)
for _ in $(seq 1 100); do
  PHASE=$($BIN agents status "$STOP_TERMINAL_ID" | json_field current_phase)
  [[ "$PHASE" == "executing_tool" ]] && break
  sleep 0.05
done
[[ "$PHASE" == "executing_tool" ]]
sleep 0.2
$BIN agents stop "$STOP_TERMINAL_ID" >"$ROOT/stop-race-result.jsonl" &
STOP_RACE_PID=$!
sleep 0.05
if $BIN agents send "$STOP_TERMINAL_ID" --message FINAL_ONLY >"$ROOT/stop-race-send-result.jsonl" 2>"$ROOT/stop-race-send-error.jsonl"; then
  python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["agent_resumed"] is True and row["run_number"] == 2' "$ROOT/stop-race-send-result.jsonl"
else
  python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["code"]=="conflict" and row["retryable"] is True and row["details"]["status"]=="stopping"' "$ROOT/stop-race-send-error.jsonl"
fi
wait "$STOP_RACE_PID"
python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["status"] == "stopped" and row["current_phase"] == "stopped"' "$ROOT/stop-race-result.jsonl"
if [[ -s "$ROOT/stop-race-send-error.jsonl" ]]; then
  $BIN messages list "$STOP_TERMINAL_ID" --status pending | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["type"]=="list_summary" and row["count"]==0'
else
  $BIN agents stop "$STOP_TERMINAL_ID" >/dev/null || true
fi
$BIN daemon status | python3 -c 'import json,sys; assert json.load(sys.stdin)["status"] == "running"'

SECRET=$($BIN agents spawn --name secret-env --dir "$ROOT/project" --mode write --message SECRET_ENV)
SECRET_ID=$(printf '%s\n' "$SECRET" | json_field id)
wait_status "$SECRET_ID" finished
$BIN agents logs "$SECRET_ID" --type tool_result --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; text=json.dumps(rows); assert "hidden" in text and "test-key" not in text'

if $BIN agents spawn --name missing-input --dir "$ROOT/project" 2>"$ROOT/usage-error.jsonl"; then
  echo "spawn without input unexpectedly succeeded" >&2
  exit 1
fi
python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["type"] == "error"' "$ROOT/usage-error.jsonl"
TIME_HELP=$($BIN agents time --help)
SEND_HELP=$($BIN agents send --help)
WAIT_HELP=$($BIN agents wait --help)
AGENT_LIST_HELP=$($BIN agents list --help)
SIDE_LIST_HELP=$($BIN sides list --help)
grep -F 'New deadline from now in integer minutes, from 1 through 6000' <<<"$TIME_HELP" >/dev/null
grep -F 'from 1 through 6000' <<<"$SEND_HELP" >/dev/null
grep -F 'from 1 through 86400' <<<"$WAIT_HELP" >/dev/null
grep -F 'from 1 through 1000' <<<"$AGENT_LIST_HELP" >/dev/null
grep -F 'from 1 through 1000' <<<"$SIDE_LIST_HELP" >/dev/null
SIDE_LOG_HELP=$($BIN sides logs --help)
grep -F '<SIDE>' <<<"$SIDE_LOG_HELP" >/dev/null
grep -F 'Side short ref (s_N) or durable ID (side_<ULID>)' <<<"$SIDE_LOG_HELP" >/dev/null
$BIN agents list --help | head -n 1 >/dev/null
CREATE_SIDE_HELP=$($BIN sides create --help)
grep -F 'subagent sides create' <<<"$CREATE_SIDE_HELP" >/dev/null
! $BIN agents --help | grep -F 'side' >/dev/null
if $BIN sides status agt_01ARZ3NDEKTSV4RRFFQ69G5FAV 2>"$ROOT/typed-id-error.jsonl"; then exit 1; fi
python3 -c 'import json,sys; assert "expected SIDE" in json.load(open(sys.argv[1]))["message"]' "$ROOT/typed-id-error.jsonl"
$BIN agents list --status working | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[-1]["type"] == "list_summary"'
for bad_limit in 0 1001 18446744073709551615; do
  if $BIN agents list --limit "$bad_limit" 2>"$ROOT/agent-limit-error.jsonl"; then exit 1; fi
  grep -q '1 through 1000' "$ROOT/agent-limit-error.jsonl"
  if $BIN sides list "$SIDE_PARENT_ID" --limit "$bad_limit" 2>"$ROOT/side-limit-error.jsonl"; then exit 1; fi
  grep -q '1 through 1000' "$ROOT/side-limit-error.jsonl"
done

$BIN agents list --status finished --spawned-after 2020-01-01T00:00:00Z | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows) >= 3; assert all(x["status"] == "finished" for x in rows[:-1]); assert rows[-1]["type"] == "list_summary"'
$BIN agents rename "$ID" renamed-tool-test | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["type"] == "agent_renamed" and row["name"] == "renamed-tool-test"'
$BIN agents list --limit 1000 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; row=next(x for x in rows if x.get("name") == "renamed-tool-test"); assert set(row) == {"type","id","ref","name","status","dir","mode","model","spawned_at","last_message_at","updated_at","current_phase","last_event_at","run_number","working_sides"}'
$BIN agents list --verbose --limit 1000 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; row=next(x for x in rows if x.get("id") == sys.argv[1]); assert row["type"] == "agent_list_item_verbose" and "last_model_event_at" in row and "seconds_since_last_event" in row' "$ID"
FIRST_PAGE=$($BIN agents list --sort spawned_at --order asc --limit 2)
NEXT_CURSOR=$(printf '%s\n' "$FIRST_PAGE" | tail -n 1 | python3 -c 'import json,sys; value=json.load(sys.stdin); assert value["count"] == 2; print(value["next_cursor"])')
FIRST_IDS=$(printf '%s\n' "$FIRST_PAGE" | head -n -1 | python3 -c 'import json,sys; print(" ".join(row["id"] for row in map(json.loads,sys.stdin)))')
$BIN agents list --sort spawned_at --order asc --limit 2 --after-cursor "$NEXT_CURSOR" | python3 -c 'import json,sys; prior=set(sys.argv[1].split()); rows=[json.loads(line) for line in sys.stdin]; assert rows[-1]["type"] == "list_summary"; assert all(row["id"] not in prior for row in rows[:-1])' "$FIRST_IDS"
$BIN agents list --sort spawned_at --order asc --limit 2 --after-cursor "$NEXT_CURSOR" --offset 0 | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert rows[-1]["type"] == "list_summary"'
if $BIN agents list --sort spawned_at --order asc --status finished --after-cursor "$NEXT_CURSOR" 2>"$ROOT/cursor-filter-error.jsonl"; then exit 1; fi
python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["code"] == "invalid_argument" and row["details"]["resource"] == "agents"' "$ROOT/cursor-filter-error.jsonl"
if $BIN agents list --after-cursor "$NEXT_CURSOR" --offset 1 2>"$ROOT/cursor-offset-error.jsonl"; then exit 1; fi
python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["code"] == "invalid_argument" and "--after-cursor" in row["message"] and "--offset 0" in row["message"] and "Usage:" not in row["message"]' "$ROOT/cursor-offset-error.jsonl"

$BIN agents delete "$ID" | python3 -c 'import json,sys; assert json.load(sys.stdin)["type"] == "agent_deleted"'

INTERRUPTED=$($BIN agents spawn --name interrupted-test --dir "$ROOT/project" --mode readonly --message DELAY)
INTERRUPTED_ID=$(printf '%s\n' "$INTERRUPTED" | json_field id)
INTERRUPTED_REF=$(printf '%s\n' "$INTERRUPTED" | json_field ref)
sleep 0.2
INTERRUPTED_RECEIPT=$($BIN agents send "$INTERRUPTED_ID" --message FINAL_ONLY)
INTERRUPTED_MESSAGE_ID=$(printf '%s\n' "$INTERRUPTED_RECEIPT" | json_field message_id)
DAEMON_PID=$($BIN daemon status | json_field pid)
kill -9 "$DAEMON_PID"
for _ in $(seq 1 50); do
  [[ ! -S "$XDG_RUNTIME_DIR/subagent.sock" ]] || sleep 0.05
done
if $BIN daemon status 2>"$ROOT/crashed-daemon.jsonl"; then
  echo "crashed daemon unexpectedly reported success" >&2
  exit 1
fi
python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["code"] == "daemon_crashed" and row["details"]["log_path"].endswith("daemon.log")' "$ROOT/crashed-daemon.jsonl"
$BIN daemon start >/dev/null
wait_status "$INTERRUPTED_ID" finished
$BIN agents status "$INTERRUPTED_ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["run_number"] == 2'
$BIN messages status "$INTERRUPTED_ID" "$INTERRUPTED_MESSAGE_ID" | python3 -c 'import json,sys; assert json.load(sys.stdin)["status"] == "delivered"'

LATEST_NOTIFICATION=$($BIN inbox list --all --limit 1 --priority 1)
LATEST_SEQUENCE=$(printf '%s\n' "$LATEST_NOTIFICATION" | head -n 1 | json_field sequence)
LATEST_NOTIFICATION_ID=$(printf '%s\n' "$LATEST_NOTIFICATION" | head -n 1 | json_field id)
$BIN inbox ack "$LATEST_NOTIFICATION_ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["type"] == "inbox_acknowledged" and row["acknowledged_through"] == int(sys.argv[1])' "$LATEST_SEQUENCE"
$BIN inbox list --priority 1 | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row == {"type":"inbox_summary","count":0,"acknowledged_through":int(sys.argv[1]),"next_cursor":None}' "$LATEST_SEQUENCE"
$BIN inbox list --all --limit 1 --priority 1 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[0]["acknowledged"] is True and rows[1]["type"]=="inbox_summary" and rows[1]["count"]==1 and rows[1]["acknowledged_through"]==int(sys.argv[1]) and rows[1]["next_cursor"]' "$LATEST_SEQUENCE"

timeout 8 "$BIN" inbox follow --after "$LATEST_SEQUENCE" --priority 1 >"$ROOT/inbox-follow.jsonl" &
INBOX_FOLLOW_PID=$!
FOLLOW_AGENT=$($BIN agents spawn --name follow-notify --dir "$ROOT/project" --mode readonly --message FINAL_ONLY)
FOLLOW_AGENT_ID=$(printf '%s\n' "$FOLLOW_AGENT" | json_field id)
FOLLOW_AGENT_REF=$(printf '%s\n' "$FOLLOW_AGENT" | json_field ref)
python3 -c 'import sys; assert int(sys.argv[2].split("_")[1]) > int(sys.argv[1].split("_")[1])' "$INTERRUPTED_REF" "$FOLLOW_AGENT_REF"
wait_status "$FOLLOW_AGENT_ID" finished
for _ in $(seq 1 40); do
  grep -q "$FOLLOW_AGENT_ID" "$ROOT/inbox-follow.jsonl" && break
  sleep 0.05
done
grep -q "$FOLLOW_AGENT_ID" "$ROOT/inbox-follow.jsonl"
kill "$INBOX_FOLLOW_PID" >/dev/null 2>&1 || true
wait "$INBOX_FOLLOW_PID" >/dev/null 2>&1 || true

FILTER_AFTER=$($BIN inbox list --all --limit 1 --priority 1 | head -n1 | json_field sequence)
timeout 8 "$BIN" inbox follow --agent "$FOLLOW_AGENT_ID" --priority 2 --after "$FILTER_AFTER" >"$ROOT/inbox-parent-filter-follow.jsonl" &
FILTER_FOLLOW_PID=$!
UNRELATED=$($BIN agents spawn --name unrelated-follow --dir "$ROOT/project" --mode readonly --message FINAL_ONLY)
UNRELATED_ID=$(printf '%s\n' "$UNRELATED" | json_field id)
wait_status "$UNRELATED_ID" finished
$BIN agents send "$FOLLOW_AGENT_ID" --message FINAL_ONLY >/dev/null
wait_status "$FOLLOW_AGENT_ID" finished
for _ in $(seq 1 40); do
  grep -q "$FOLLOW_AGENT_ID" "$ROOT/inbox-parent-filter-follow.jsonl" && break
  sleep 0.05
done
grep -q "$FOLLOW_AGENT_ID" "$ROOT/inbox-parent-filter-follow.jsonl"
! grep -q "$UNRELATED_ID" "$ROOT/inbox-parent-filter-follow.jsonl"
kill "$FILTER_FOLLOW_PID" >/dev/null 2>&1 || true
wait "$FILTER_FOLLOW_PID" >/dev/null 2>&1 || true

$BIN daemon status | python3 tests/validate_schema.py
$BIN agents status "$INTERRUPTED_ID" | python3 tests/validate_schema.py
$BIN agents list --limit 1000 | python3 tests/validate_schema.py
$BIN agents list --verbose --limit 1000 | python3 tests/validate_schema.py
$BIN agents logs "$INTERRUPTED_ID" --all --limit 1000 | python3 tests/validate_schema.py
$BIN agents context "$INTERRUPTED_ID" | python3 tests/validate_schema.py
$BIN messages list "$INTERRUPTED_ID" | python3 tests/validate_schema.py
$BIN sides list "$SIDE_PARENT_ID" | python3 tests/validate_schema.py
$BIN sides status "$SIDE_ID" | python3 tests/validate_schema.py
$BIN sides logs "$SIDE_ID" --all --limit 1000 | python3 tests/validate_schema.py
$BIN inbox list --all --limit 100 --priority 1 | python3 tests/validate_schema.py
$BIN inbox ack "$LATEST_SEQUENCE" | python3 tests/validate_schema.py

$BIN daemon stop | python3 -c 'import json,sys; assert json.load(sys.stdin)["status"] == "stopping"'
for _ in $(seq 1 100); do
  [[ ! -S "$XDG_RUNTIME_DIR/subagent.sock" ]] && break
  sleep 0.05
done
if $BIN daemon status 2>"$ROOT/stopped-daemon.jsonl"; then
  echo "stopped daemon unexpectedly reported success" >&2
  exit 1
fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "daemon_stopped"' "$ROOT/stopped-daemon.jsonl"
unset SUBAGENT_WEB_PASSWORD
WEB=$($BIN daemon start --web-ui-port "$WEB_PORT")
printf '%s\n' "$WEB" | python3 -c 'import json,sys; value=json.load(sys.stdin); assert value["web_auth"] == "none" and value["web_ui_url"] == sys.argv[1]' "http://127.0.0.1:$WEB_PORT/"
curl -fsS "http://127.0.0.1:$WEB_PORT/api/agents" | python3 -c 'import json,sys; [json.loads(line) for line in sys.stdin]'
$BIN daemon stop >/dev/null
for _ in $(seq 1 100); do
  [[ ! -S "$XDG_RUNTIME_DIR/subagent.sock" ]] && break
  sleep 0.05
done

WEB=$(SUBAGENT_WEB_PASSWORD='test-web-password' $BIN daemon start --web-ui-port "$WEB_PORT")
printf '%s\n' "$WEB" | python3 -c 'import json,sys; value=json.load(sys.stdin); assert value["web_auth"] == "basic" and value["web_ui_url"] == sys.argv[1]' "http://127.0.0.1:$WEB_PORT/"
[[ "$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$WEB_PORT/")" == 401 ]]
[[ "$(curl -sS -u 'subagent:wrong-password' -o /dev/null -w '%{http_code}' "http://127.0.0.1:$WEB_PORT/")" == 401 ]]
curl -sS -D - -o /dev/null "http://127.0.0.1:$WEB_PORT/" | tr -d '\r' | grep -qi '^www-authenticate: Basic realm="subagent"'
curl -fsS -u 'subagent:test-web-password' "http://127.0.0.1:$WEB_PORT/assets/app.css" | python3 -c 'import re,sys; css=sys.stdin.read(); assert re.search(r"background:\s*#000000",css) and re.search(r"color:\s*#ffffff",css)'
curl -fsS -u 'subagent:test-web-password' "http://127.0.0.1:$WEB_PORT/" | python3 -c 'import sys; html=sys.stdin.read(); assert all(value in html for value in ("dashboard-page","agent-page","side-page","open-spawn","agent-tabs","side-tabs","main-scroll","side-list","side-main-tab","controls-tab","side-dialog","inbox-filters","inbox-agent"))'
curl -fsS -u 'subagent:test-web-password' "http://127.0.0.1:$WEB_PORT/assets/ui-core.js" | python3 -c 'import sys; js=sys.stdin.read(); assert "patchDiffHtml" in js and "patchLineKind" in js and "deletion" in js and "addition" in js'
curl -fsS -u 'subagent:test-web-password' "http://127.0.0.1:$WEB_PORT/assets/app.js" | python3 -c 'import sys; js=sys.stdin.read(); assert all(value in js for value in ("TimelineController","loadOlder","nearBottom","tool-accordion","/api/sides/","loadInbox","/api/inbox"))'
[[ "$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$WEB_PORT/api/agents")" == 401 ]]
curl -fsS -u 'subagent:test-web-password' "http://127.0.0.1:$WEB_PORT/api/agents" | python3 -c 'import json,sys; [json.loads(line) for line in sys.stdin]'
curl -fsS -u 'subagent:test-web-password' "http://127.0.0.1:$WEB_PORT/api/inbox?all=true&priority=2&limit=10&offset=0" | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert rows[-1]["type"] == "inbox_summary" and rows[-1]["count"] == len(rows)-1; assert all(row["type"] == "notification" and row["priority"] >= 2 for row in rows[:-1])'
curl -fsS -u 'subagent:test-web-password' "http://127.0.0.1:$WEB_PORT/api/inbox?all=true&agent=$NOTIFY_ID&priority=1&limit=1&offset=1" | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[0]["agent_id"] == sys.argv[1] and rows[0]["event_type"] == "milestone" and rows[1]["type"] == "inbox_summary" and rows[1]["count"] == 1' "$NOTIFY_ID"
curl -fsS -u 'subagent:test-web-password' "http://127.0.0.1:$WEB_PORT/api/agents/$SIDE_PARENT_ID/sides" | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert rows[-1]["type"]=="list_summary" and all(row["type"] == "side_list_item" for row in rows[:-1])'
curl -fsS -u 'subagent:test-web-password' -X POST -H "Origin: http://127.0.0.1:$WEB_PORT" -H 'Content-Type: application/json' -d '{"name":"web-renamed"}' "http://127.0.0.1:$WEB_PORT/api/agents/$INTERRUPTED_ID/rename" | python3 -c 'import json,sys; assert json.load(sys.stdin)["name"] == "web-renamed"'
$BIN agents delete "$SIDE_PARENT_ID" | python3 -c 'import json,sys; assert json.load(sys.stdin)["type"] == "agent_deleted"'
if $BIN sides status "$SIDE_ID" 2>"$ROOT/deleted-side-error.jsonl"; then
  echo "Side history survived parent cascade deletion" >&2
  exit 1
fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "side_not_found"' "$ROOT/deleted-side-error.jsonl"
$BIN daemon status | python3 -c 'import json,sys; value=json.load(sys.stdin); assert value["web_ui_url"] == sys.argv[1] and value["web_auth"] == "basic"' "http://127.0.0.1:$WEB_PORT/"
$BIN daemon stop >/dev/null

echo '{"type":"test_result","status":"passed","suite":"e2e"}'

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
export OPENAI_BASE_URL=http://127.0.0.1:18080/v1
export OPENAI_MODEL=test-model

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

$BIN config set max-agents 1 >/dev/null
python3 -c 'import pathlib,sys; body=pathlib.Path(sys.argv[1]).read_text(); assert "test-model" not in body and "127.0.0.1:18080" not in body' "$XDG_CONFIG_HOME/subagent/config.toml"
$BIN daemon start | python3 -c 'import json,sys; assert json.load(sys.stdin)["status"] == "running"'

mkdir -p "$ROOT/caller/project"
RELATIVE=$(cd "$ROOT/caller" && "$BIN" agents spawn --dir project --message FINAL_ONLY)
RELATIVE_ID=$(printf '%s\n' "$RELATIVE" | json_field id)
printf '%s\n' "$RELATIVE" | python3 -c 'import json,os,sys; row=json.load(sys.stdin); assert row["dir"] == os.path.realpath(sys.argv[1])' "$ROOT/caller/project"
wait_status "$RELATIVE_ID" finished
(cd "$ROOT/caller" && "$BIN" agents list --dir project) | python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert len(rows) == 1'

READONLY=$($BIN agents spawn --dir "$ROOT/project" --mode readonly --message READONLY_PROMPT)
READONLY_ID=$(printf '%s\n' "$READONLY" | json_field id)
wait_status "$READONLY_ID" finished
$BIN agents logs "$READONLY_ID" --type assistant_message | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[-1]["data"]["content"] == "readonly prompt correct"'

printf '%s\n' WRITE_EDIT_PATCH >"$ROOT/task.md"
SPAWN=$($BIN agents spawn --dir "$ROOT/project" --mode write --title tool-test --message-file "$ROOT/task.md")
ID=$(printf '%s\n' "$SPAWN" | json_field id)
wait_status "$ID" finished
[[ "$(cat "$ROOT/project/generated.txt")" == "gamma" ]]

$BIN agents logs "$ID" --type tool_result --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows) >= 6'
$BIN agents logs "$ID" --type reasoning --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows and rows[-1]["data"]["content"] == "mock reasoning"'
CURSOR=$($BIN agents logs "$ID" --limit 100 | sed -n '1p' | python3 -c 'import json,sys; print(json.load(sys.stdin)["event_id"])')
$BIN agents logs "$ID" --after "$CURSOR" --limit 100 | python3 -c 'import json,sys; cursor=sys.argv[1]; rows=[json.loads(x) for x in sys.stdin]; assert all(row["event_id"] != cursor for row in rows)' "$CURSOR"
if $BIN agents logs "$ID" --after evt_missing 2>"$ROOT/cursor-error.jsonl"; then
  echo "logs unexpectedly accepted an unknown cursor" >&2
  exit 1
fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "not_found"' "$ROOT/cursor-error.jsonl"
if timeout 2 $BIN agents logs "$ID" --after evt_missing --follow 2>"$ROOT/follow-cursor-error.jsonl"; then
  echo "follow unexpectedly accepted an unknown cursor" >&2
  exit 1
fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "not_found"' "$ROOT/follow-cursor-error.jsonl"
$BIN agents context "$ID" | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert rows[0]["type"] == "context_meta"; assert all(x["type"] in ("context_meta","user_message","assistant_message") for x in rows)'

printf '%s\n' FINAL_ONLY >"$ROOT/followup.md"
$BIN agents send "$ID" --message-file "$ROOT/followup.md" >/dev/null
wait_status "$ID" finished
$BIN agents status "$ID" | python3 -c 'import json,sys; assert json.load(sys.stdin)["run_number"] == 2'

printf '%s\n' side-file-content >"$ROOT/project/side.txt"
python3 -c 'import base64,sys; open(sys.argv[1],"wb").write(base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="))' "$ROOT/project/pixel.png"
SIDE_PARENT=$($BIN agents spawn --dir "$ROOT/project" --mode write --message SIDE_PARENT_MARKER)
SIDE_PARENT_ID=$(printf '%s\n' "$SIDE_PARENT" | json_field id)
wait_status "$SIDE_PARENT_ID" finished
CONTEXT_BEFORE=$(sha256sum "$XDG_STATE_HOME/subagent/agents/$SIDE_PARENT_ID/context.json" | cut -d' ' -f1)
EVENTS_BEFORE=$(sha256sum "$XDG_STATE_HOME/subagent/agents/$SIDE_PARENT_ID/events.jsonl" | cut -d' ' -f1)
SIDE=$($BIN agents side "$SIDE_PARENT_ID" --message SIDE_TOOL_QUESTION)
SIDE_ID=$(printf '%s\n' "$SIDE" | json_field side_id)
printf '%s\n' "$SIDE" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["type"] == "side_answer"; assert row["answer"] == "side inherited context and tools"; assert row["tool_calls"] == 5; assert row["inherited_context_messages"] >= 3; assert row["ephemeral"] is True; assert row["mode"] == "readonly"; assert row["parent_mode"] == "write"'
[[ ! -e "$XDG_STATE_HOME/subagent/agents/$SIDE_ID" ]]
$BIN agents btw "$SIDE_PARENT_ID" --message SIDE_CONTEXT_ONLY | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["answer"] == "context inherited" and row["tool_calls"] == 0'
[[ "$CONTEXT_BEFORE" == "$(sha256sum "$XDG_STATE_HOME/subagent/agents/$SIDE_PARENT_ID/context.json" | cut -d' ' -f1)" ]]
[[ "$EVENTS_BEFORE" == "$(sha256sum "$XDG_STATE_HOME/subagent/agents/$SIDE_PARENT_ID/events.jsonl" | cut -d' ' -f1)" ]]

DELAY=$($BIN agents spawn --dir "$ROOT/project" --mode readonly --message DELAY)
DELAY_ID=$(printf '%s\n' "$DELAY" | json_field id)
$BIN agents side "$DELAY_ID" --message SIDE_WHILE_WORKING | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["answer"] == "parent still running" and row["mode"] == "readonly" and row["parent_mode"] == "readonly"'
$BIN agents time "$DELAY_ID" 1 | python3 -c 'import json,sys; assert json.load(sys.stdin)["deadline_at"] is not None'
if $BIN agents spawn --dir "$ROOT/project" --message SECOND_AGENT 2>"$ROOT/capacity-error.jsonl"; then
  echo "spawn unexpectedly passed max-agents" >&2
  exit 1
fi
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["code"] == "max_agents_reached"' "$ROOT/capacity-error.jsonl"
$BIN agents stop "$DELAY_ID" >/dev/null
wait_status "$DELAY_ID" stopped

BG=$($BIN agents spawn --dir "$ROOT/project" --mode write --message BACKGROUND_LIMIT)
BG_ID=$(printf '%s\n' "$BG" | json_field id)
wait_status "$BG_ID" finished
$BIN agents logs "$BG_ID" --type tool_result --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert any("background terminal limit reached (8)" in json.dumps(x) for x in rows)'

POLL=$($BIN agents spawn --dir "$ROOT/project" --mode write --message TERMINAL_POLL)
POLL_ID=$(printf '%s\n' "$POLL" | json_field id)
wait_status "$POLL_ID" finished
$BIN agents logs "$POLL_ID" --type tool_call --limit 100 | python3 -c 'import json,sys; names=[json.loads(x)["data"]["name"] for x in sys.stdin]; assert names == ["exec_command", "write_stdin"]'
$BIN agents logs "$POLL_ID" --type tool_result --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert "end" in json.dumps(rows[-1])'

SECRET=$($BIN agents spawn --dir "$ROOT/project" --mode write --message SECRET_ENV)
SECRET_ID=$(printf '%s\n' "$SECRET" | json_field id)
wait_status "$SECRET_ID" finished
$BIN agents logs "$SECRET_ID" --type tool_result --limit 100 | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; text=json.dumps(rows); assert "hidden" in text and "test-key" not in text'

if $BIN agents spawn --dir "$ROOT/project" 2>"$ROOT/usage-error.jsonl"; then
  echo "spawn without input unexpectedly succeeded" >&2
  exit 1
fi
python3 -c 'import json,sys; row=json.load(open(sys.argv[1])); assert row["type"] == "error"' "$ROOT/usage-error.jsonl"

$BIN agents list --status finished --spawned-after 2020-01-01T00:00:00Z | python3 -c 'import json,sys; rows=[json.loads(x) for x in sys.stdin]; assert len(rows) >= 2; assert all(x["status"] == "finished" for x in rows)'

$BIN agents delete "$ID" | python3 -c 'import json,sys; assert json.load(sys.stdin)["type"] == "agent_deleted"'

INTERRUPTED=$($BIN agents spawn --dir "$ROOT/project" --mode readonly --message DELAY)
INTERRUPTED_ID=$(printf '%s\n' "$INTERRUPTED" | json_field id)
DAEMON_PID=$($BIN daemon status | json_field pid)
kill -9 "$DAEMON_PID"
for _ in $(seq 1 50); do
  [[ ! -S "$XDG_RUNTIME_DIR/subagent.sock" ]] || sleep 0.05
done
$BIN daemon start >/dev/null
wait_status "$INTERRUPTED_ID" stopped
$BIN agents status "$INTERRUPTED_ID" | python3 -c 'import json,sys; row=json.load(sys.stdin); assert row["stop_reason"] == "daemon_interrupted"'

$BIN daemon stop | python3 -c 'import json,sys; assert json.load(sys.stdin)["status"] == "stopping"'

echo '{"type":"test_result","status":"passed","suite":"e2e"}'

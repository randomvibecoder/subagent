#!/usr/bin/env bash
set -euo pipefail

ROOT=${SUBAGENT_DEMO_ROOT:-/tmp/subagent-cli-demo}
SESSION=subagent-demo
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${SUBAGENT_DEMO_BIN:-$REPO/target/release/subagent}

if [[ ${1:-} == watch ]]; then
  export HOME="$ROOT/home"
  export XDG_CONFIG_HOME="$ROOT/config"
  export XDG_STATE_HOME="$ROOT/state"
  export XDG_RUNTIME_DIR="$ROOT/run"
  kind=${2:?missing watcher name}
  color=36
  [[ $kind == test ]] && color=35
  printf '\033[1;%sm%s AGENT\033[0m  waiting for work...\n' "$color" "${kind^^}"
  until id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' \
    "$ROOT/$kind.json" 2>/dev/null); do sleep 0.1; done
  "$BIN" agents logs "$id" \
    --type user_message --type assistant_message --type tool_call --type tool_result --follow || true
  printf '\n\033[2mAgent stream closed.\033[0m\n'
  sleep 60
fi

cleanup() {
  HOME="$ROOT/home" \
    XDG_CONFIG_HOME="$ROOT/config" \
    XDG_STATE_HOME="$ROOT/state" \
    XDG_RUNTIME_DIR="$ROOT/run" \
    "$BIN" daemon stop >/dev/null 2>&1 || true
  tmux kill-session -t "$SESSION" >/dev/null 2>&1 || true
  if [[ -f "$ROOT/mock.pid" ]]; then
    kill "$(<"$ROOT/mock.pid")" >/dev/null 2>&1 || true
  fi
}

if [[ ${1:-setup} == cleanup ]]; then
  cleanup
  exit 0
fi

cleanup
rm -rf "$ROOT"
mkdir -p "$ROOT"/{home,config,state,run,auth-project/src,test-project/src}
printf '%s\n' 'pub fn validate_token(token: &str) -> bool { !token.is_empty() }' \
  >"$ROOT/auth-project/src/auth.rs"
printf '%s\n' '[package]' 'name = "demo-tests"' 'version = "0.1.0"' \
  'edition = "2021"' '' '[dependencies]' >"$ROOT/test-project/Cargo.toml"
printf '%s\n' '#[test]' 'fn parallel_agents_work() { assert!(true); }' \
  >"$ROOT/test-project/src/lib.rs"

python3 "$REPO/tests/mock_openai.py" >"$ROOT/mock.log" 2>&1 &
echo $! >"$ROOT/mock.pid"
sleep 0.2

tmux new-session -d -s "$SESSION" -x 150 -y 42 -c "$REPO"
for assignment in \
  "PATH=$(dirname "$BIN"):$PATH" \
  "HOME=$ROOT/home" \
  "XDG_CONFIG_HOME=$ROOT/config" \
  "XDG_STATE_HOME=$ROOT/state" \
  "XDG_RUNTIME_DIR=$ROOT/run" \
  'OPENAI_API_KEY=demo-key' \
  'OPENAI_BASE_URL=http://127.0.0.1:18080/v1' \
  'OPENAI_MODEL=demo-model'; do
  tmux set-environment -t "$SESSION" "${assignment%%=*}" "${assignment#*=}"
done

tmux set-option -t "$SESSION" status off
tmux split-window -v -l 58% -t "$SESSION":0.0 -c "$REPO" \
  "bash '$REPO/scripts/demo-session.sh' watch auth"
tmux split-window -h -l 50% -t "$SESSION":0.1 -c "$REPO" \
  "bash '$REPO/scripts/demo-session.sh' watch test"
tmux select-pane -t "$SESSION":0.0
tmux send-keys -t "$SESSION":0.0 \
  "export PATH='$(dirname "$BIN")':\"\$PATH\" HOME='$ROOT/home' XDG_CONFIG_HOME='$ROOT/config' XDG_STATE_HOME='$ROOT/state' XDG_RUNTIME_DIR='$ROOT/run' OPENAI_API_KEY=demo-key OPENAI_BASE_URL=http://127.0.0.1:18080/v1 OPENAI_MODEL=demo-model; clear" Enter

printf '%s\n' "$ROOT"

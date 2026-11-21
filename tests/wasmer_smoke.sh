#!/usr/bin/env bash
set -euo pipefail

wasmer run dist/wrev.wasm -- --help | grep -F 'wrev [login'
wasmer package build --check

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
log=$(mktemp)
wasmer run dist/wrev.wasm --net \
  --env "PORT=$port" \
  --env WREV_WEBHOOK_SECRET=fixture-signing-key \
  --env WREV_REPOSITORIES=fixture/wrev \
  --env WREV_GITHUB_TOKEN=fixture-github-token \
  --env OPENAI_MODEL=fixture-model \
  -- listen > "$log" 2>&1 &
listener=$!
cleanup() {
  kill "$listener" 2>/dev/null || true
  wait "$listener" 2>/dev/null || true
  rm -f "$log"
}
trap cleanup EXIT

url="http://127.0.0.1:$port"
for attempt in {1..45}; do
  if curl --silent --fail --max-time 1 "$url/health" > /dev/null; then
    status=$(curl --silent --max-time 3 --output /dev/null --write-out '%{http_code}' \
      --data '{}' "$url/github/webhook")
    test "$status" = 401
    echo 'WASIX listener verified: /health returned 200 and unsigned webhook returned 401'
    exit 0
  fi
  if ! kill -0 "$listener" 2>/dev/null; then
    break
  fi
  sleep 1
done
cat "$log"
exit 1

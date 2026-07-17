#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  printf '%s\n' 'managed-process release smoke is Linux-only' >&2
  exit 1
fi

for command in env git od pgrep pkill realpath setsid; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'required command is unavailable: %s\n' "$command" >&2
    exit 1
  }
done

binary="${NIB_BINARY:-$(pwd)/target/release/nib}"
if [ ! -x "$binary" ]; then
  printf 'release binary is unavailable: %s\n' "$binary" >&2
  exit 1
fi
binary="$(realpath "$binary")"
token="$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/nib-ft017-${token}.XXXXXX")"
owner_pid=""

cleanup() {
  if [ -n "$owner_pid" ] && kill -0 "$owner_pid" 2>/dev/null; then
    kill -KILL -- "-$owner_pid" 2>/dev/null || true
    wait "$owner_pid" 2>/dev/null || true
  fi
  pkill -KILL -f "^nib-ft017-${token} 5$" 2>/dev/null || true
  rm -rf "$fixture"
}
trap cleanup EXIT INT TERM

mkdir -p "$fixture/home"
git -C "$fixture" init --quiet
git -C "$fixture" config user.email nib-smoke@example.invalid
git -C "$fixture" config user.name 'nib managed-process smoke'
printf '.nib/\n' >"$fixture/.gitignore"
printf 'fixture\n' >"$fixture/README.md"
git -C "$fixture" add .gitignore README.md
git -C "$fixture" commit --quiet -m initial
mkdir -p "$fixture/.nib"
printf '%s\n' \
  '[llm]' \
  'active_provider = "mock"' \
  '' \
  '[llm.providers.mock]' \
  'model = "mock-model"' \
  >"$fixture/.nib/config.toml"

(
  cd "$fixture"
  exec env HOME="$fixture/home" NIB_ENABLE_MANAGED_PROCESS_SMOKE=1 \
    setsid "$binary" run \
    "managed supervisor release smoke parent ${token}" \
    --provider mock --model mock-model --max-steps 10 --yes \
    >"$fixture/owner.log" 2>&1
) &
owner_pid=$!

deadline=$((SECONDS + 20))
while ! pgrep -f "^nib-ft017-${token} 5$" >/dev/null 2>&1; do
  if ! kill -0 "$owner_pid" 2>/dev/null; then
    cat "$fixture/owner.log" >&2
    printf '%s\n' 'nib owner exited before the detached descendant started' >&2
    exit 1
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    cat "$fixture/owner.log" >&2
    printf '%s\n' 'timed out waiting for the detached descendant' >&2
    exit 1
  fi
  sleep 0.05
done

kill -KILL -- "-$owner_pid"
wait "$owner_pid" 2>/dev/null || true
owner_pid=""

record=""
deadline=$((SECONDS + 20))
while [ -z "$record" ]; do
  while IFS= read -r candidate; do
    if grep -q '"status": "failed"' "$candidate" \
      && grep -q '"cleanup_verified": true' "$candidate"; then
      record="$candidate"
      break
    fi
  done < <(find "$fixture" -path '*/.nib/subagents/sub-*.json' -type f -print)
  if [ -n "$record" ]; then
    break
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    find "$fixture" -path '*/.nib/subagents/sub-*.json' -type f -print -exec cat {} \; >&2
    printf '%s\n' 'timed out waiting for verified subagent reconciliation' >&2
    exit 1
  fi
  sleep 0.05
done

subagent_id="$(basename "$record" .json)"
state_root="$(dirname "$(dirname "$record")")"
scope="$state_root/process-scopes/${subagent_id}.json"
grep -q '"descendants_reaped": true' "$record"
deadline=$((SECONDS + 10))
while [ -e "$scope" ] || [ -e "$state_root/process-scopes/${subagent_id}.cleanup.lease" ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    find "$state_root/process-scopes" -maxdepth 1 -type f -print -exec cat {} \; >&2
    printf '%s\n' 'timed out waiting for completed process-scope retirement' >&2
    exit 1
  fi
  sleep 0.05
done
if pgrep -f "^nib-ft017-${token} 5$" >/dev/null 2>&1; then
  printf '%s\n' 'terminal state was published while the detached descendant was live' >&2
  exit 1
fi

printf 'managed-process release smoke passed for %s\n' "$subagent_id"

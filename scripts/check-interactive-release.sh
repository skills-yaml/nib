#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  printf '%s\n' 'interactive release smoke is Linux-only' >&2
  exit 1
fi

for command in find git grep head realpath script sed timeout; do
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
fixture="$(mktemp -d "${TMPDIR:-/tmp}/nib-interactive-smoke.XXXXXX")"

cleanup() {
  rm -rf "$fixture"
}

preserve_fixture_on_error() {
  local status=$?
  trap - ERR EXIT INT TERM
  printf 'interactive release smoke failed; fixture retained at %s\n' "$fixture" >&2
  exit "$status"
}

trap cleanup EXIT INT TERM
trap preserve_fixture_on_error ERR

mkdir -p "$fixture/.nib"
git -C "$fixture" init --quiet
git -C "$fixture" config user.email nib-smoke@example.invalid
git -C "$fixture" config user.name 'nib interactive smoke'
printf '.nib/\n' >"$fixture/.gitignore"
printf 'interactive smoke fixture\n' >"$fixture/README.md"
git -C "$fixture" add .gitignore README.md
git -C "$fixture" commit --quiet -m initial
printf '%s\n' \
  '[llm]' \
  'active_provider = "mock"' \
  '' \
  '[llm.providers.mock]' \
  'model = "mock-model"' \
  '' \
  '[skills]' \
  'enabled = false' \
  '' \
  '[daemons]' \
  'cron_enabled = false' \
  'curator_enabled = false' \
  >"$fixture/.nib/config.toml"

plain_output="$fixture/plain-output.txt"
(
  cd "$fixture"
  printf '/quit\n' | env NIB_NO_UPDATE_CHECK=1 "$binary"
) >"$plain_output" 2>&1
grep -Fq 'mode: plain' "$plain_output"

(
  cd "$fixture"
  printf '/quit\n' | env NIB_NO_UPDATE_CHECK=1 "$binary" chat --plain
) >"$plain_output" 2>&1
grep -Fq 'mode: plain' "$plain_output"

session_count() {
  local directory="$fixture/.nib/profiles/default/sessions"
  if [ ! -d "$directory" ]; then
    printf '0\n'
    return
  fi
  find "$directory" -maxdepth 1 -type f -name '*.json' -print | wc -l | tr -d ' '
}

sessions_before="$(session_count)"
forced_error="$fixture/forced-tui-error.txt"
if (
  cd "$fixture"
  env NIB_NO_UPDATE_CHECK=1 "$binary" --tui
) >"$forced_error" 2>&1; then
  printf '%s\n' 'forced TUI unexpectedly succeeded without a terminal' >&2
  exit 1
fi
grep -Fq 'use --plain instead' "$forced_error"
if [ "$(session_count)" != "$sessions_before" ]; then
  printf '%s\n' 'forced TUI failure mutated the session store' >&2
  exit 1
fi

(
  cd "$fixture"
  env NIB_NO_UPDATE_CHECK=1 "$binary" run \
    'finish the release smoke' \
    --provider mock --model mock-model --max-steps 4 --yes
) >"$fixture/run-output.txt" 2>&1
grep -Fq 'Agent run completed for session' "$fixture/run-output.txt"

quote_for_sh() {
  local value=${1//\'/\'\\\'\'}
  printf "'%s'" "$value"
}

quoted_fixture="$(quote_for_sh "$fixture")"
quoted_binary="$(quote_for_sh "$binary")"
alternate_screen_exit="$(printf '\033[?1049l')"

run_tui_smoke() {
  local label=$1
  local arguments=$2
  local output="$fixture/$label.txt"
  local child_command="cd $quoted_fixture && TERM=xterm-256color NIB_NO_UPDATE_CHECK=1 $quoted_binary $arguments"

  { sleep 1; printf '\021'; } |
    timeout 20 script -q -e -c "$child_command" /dev/null >"$output" 2>&1
  grep -Fq "$alternate_screen_exit" "$output"
}

run_tui_smoke automatic ''
run_tui_smoke compatibility 'tui'
grep -Fq 'compatibility alias' "$fixture/compatibility.txt"

plain_pty_output="$fixture/forced-plain-question.txt"
plain_pty_command="cd $quoted_fixture && TERM=xterm-256color NIB_NO_UPDATE_CHECK=1 $quoted_binary --plain --run 'ask a question before continuing'"
{ sleep 1; printf 'y\n2\n/quit\n'; } |
  timeout 20 script -q -e -c "$plain_pty_command" /dev/null >"$plain_pty_output" 2>&1
grep -Fq 'mode: plain' "$plain_pty_output"
grep -Fq 'Approval required for approve_plan' "$plain_pty_output"
grep -Fq 'Answer (number or text):' "$plain_pty_output"
grep -Fq '"answer":"full"' "$plain_pty_output"
grep -Fq 'Goodbye. Session saved' "$plain_pty_output"

forced_tui_output="$fixture/forced-tui-cancellation.txt"
forced_tui_command="cd $quoted_fixture && TERM=xterm-256color NIB_NO_UPDATE_CHECK=1 $quoted_binary --tui --run 'ask a question before continuing'"
{ sleep 2; printf '\003'; sleep 1; printf '\021'; } |
  timeout 20 script -q -e -c "$forced_tui_command" /dev/null >"$forced_tui_output" 2>&1
grep -Fq "$alternate_screen_exit" "$forced_tui_output"
grep -R -Fq '"outcome": "cancelled_by_user"' \
  "$fixture/.nib/profiles/default/sessions"

session_directory="$fixture/.nib/profiles/default/sessions"
if [ "$(session_count)" -lt 2 ]; then
  printf '%s\n' 'interactive session smoke requires at least two persisted sessions' >&2
  exit 1
fi

source_creation_output="$fixture/source-session.txt"
(
  cd "$fixture"
  printf '/quit\n' | env NIB_NO_UPDATE_CHECK=1 "$binary" --plain
) >"$source_creation_output" 2>&1
source_session="$(
  sed -n 's/.*session: \([^ ]*\).*/\1/p' "$source_creation_output" | head -n 1
)"
if [ -z "$source_session" ] || [ ! -f "$session_directory/$source_session.json" ]; then
  printf '%s\n' 'could not identify the fresh source session' >&2
  exit 1
fi
quoted_source_session="$(quote_for_sh "$source_session")"
session_switch_output="$fixture/session-switch.txt"
session_switch_command="cd $quoted_fixture && stty rows 40 cols 120 && TERM=xterm-256color NIB_NO_UPDATE_CHECK=1 $quoted_binary --tui --session $quoted_source_session"
resumed_goal='continue the resumed session'
{
  sleep 2
  printf '/pro'
  sleep 1
  printf '\t'
  sleep 0.3
  printf '\r'
  sleep 1
  printf '/session\r'
  sleep 1
  printf '\033[B'
  sleep 0.6
  printf '\033'
  sleep 0.6
  printf '/session\r'
  sleep 1
  printf '\033[B'
  sleep 0.6
  printf '\r'
  sleep 0.6
  printf '\033'
  sleep 0.6
  printf '\r'
  sleep 0.6
  printf '\r'
  sleep 1
  printf '%s\r' "$resumed_goal"
  sleep 1.5
  printf '\003'
  sleep 1
  printf '\021'
} | timeout 30 script -q -e -c "$session_switch_command" /dev/null >"$session_switch_output" 2>&1
grep -Fq 'Command' "$session_switch_output"
grep -Fq 'Completion' "$session_switch_output"
grep -Fq 'Configured' "$session_switch_output"
grep -Fq 'providers:' "$session_switch_output"
grep -Fq 'Session' "$session_switch_output"
grep -Fq 'Switcher' "$session_switch_output"
grep -Fq 'Confirm session switch' "$session_switch_output"
grep -Fq 'Resumed session ' "$session_switch_output"
mapfile -t resumed_sessions < <(
  grep -F -l "$resumed_goal" "$session_directory"/*.json
)
if [ "${#resumed_sessions[@]}" -ne 1 ]; then
  printf '%s\n' 'resumed turn was not persisted to exactly one session' >&2
  exit 1
fi
if [ "${resumed_sessions[0]}" = "$session_directory/$source_session.json" ]; then
  printf '%s\n' 'resumed turn was persisted to the former session' >&2
  exit 1
fi

printf '%s\n' 'Interactive release smoke passed.'

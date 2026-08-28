#!/usr/bin/env bash
set -Eeuo pipefail

platform="$(uname -s)"
case "$platform" in
  Linux | Darwin) ;;
  *)
    printf 'interactive release smoke requires native Linux or macOS, found %s\n' "$platform" >&2
    exit 1
    ;;
esac

for command in basename dirname env find git grep head mktemp pgrep script sed sleep sort stty tr uname wc; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'required command is unavailable: %s\n' "$command" >&2
    exit 1
  }
done
if [ "$platform" = "Linux" ]; then
  command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' 'required command is unavailable: timeout' >&2
    exit 1
  }
fi

binary="${NIB_BINARY:-$(pwd)/target/release/nib}"
if [ ! -x "$binary" ]; then
  printf 'release binary is unavailable: %s\n' "$binary" >&2
  exit 1
fi
binary_directory="$(cd "$(dirname "$binary")" && pwd -P)"
binary="$binary_directory/$(basename "$binary")"
temporary_root="${TMPDIR:-/tmp}"
fixture="$(mktemp -d "$temporary_root/nib-interactive-smoke.XXXXXX")"
current_case='preflight'

cleanup() {
  if [[ -n "${fixture:-}" && "$fixture" == "$temporary_root"/nib-interactive-smoke.* ]]; then
    rm -rf -- "$fixture"
  fi
}

report_error() {
  local status=$?
  trap - ERR
  if [ "${NIB_KEEP_INTERACTIVE_SMOKE_FIXTURE:-0}" = "1" ]; then
    trap - EXIT
    printf 'interactive release smoke failed in case %s near line %s; fixture retained at %s\n' \
      "$current_case" "${BASH_LINENO[0]:-unknown}" "$fixture" >&2
  else
    printf 'interactive release smoke failed in case %s near line %s; isolated fixture will be removed\n' \
      "$current_case" "${BASH_LINENO[0]:-unknown}" >&2
  fi
  exit "$status"
}

trap cleanup EXIT
trap report_error ERR
trap 'exit 130' INT
trap 'exit 143' TERM

isolated_home="$fixture/home"
isolated_config="$fixture/xdg-config"
mkdir -p "$fixture/.nib" "$isolated_home" "$isolated_config"
git -C "$fixture" init --quiet
git -C "$fixture" config user.email nib-smoke@example.invalid
git -C "$fixture" config user.name 'nib interactive smoke'
printf '.nib/\nhome/\nxdg-config/\n*.txt\n' >"$fixture/.gitignore"
printf 'interactive smoke fixture\n' >"$fixture/README.md"
git -C "$fixture" add .gitignore README.md
git -C "$fixture" commit --quiet -m initial

private_sentinel='interactive-private-sentinel-q7v9k2'
printf '%s\n' \
  '[llm]' \
  'active_provider = "mock"' \
  '' \
  '[llm.providers.mock]' \
  'model = "mock-model"' \
  '' \
  '[llm.providers.openai]' \
  'model = "gpt-5"' \
  "api_key = \"$private_sentinel\"" \
  '' \
  '[skills]' \
  'enabled = false' \
  '' \
  '[daemons]' \
  'cron_enabled = false' \
  'curator_enabled = false' \
  >"$fixture/.nib/config.toml"

# Every child is credential-free and has an isolated user/config directory. The Mock-
# only delay is scoped to one queue/steering fixture goal in src/llm/mock.rs.
offline_environment=(
  env
  -u OPENAI_API_KEY
  -u ANTHROPIC_API_KEY
  -u GOOGLE_API_KEY
  -u XAI_API_KEY
  -u META_API_KEY
  -u OPENROUTER_API_KEY
  -u NIB_MANAGED_PROCESS_SCOPE
  -u NIB_SKILLS_DIR
  "HOME=$isolated_home"
  "XDG_CONFIG_HOME=$isolated_config"
  NIB_NO_UPDATE_CHECK=1
  NIB_ENABLE_INTERACTIVE_SMOKE=1
)

session_directory="$fixture/.nib/profiles/default/sessions"
session_count() {
  if [ ! -d "$session_directory" ]; then
    printf '0\n'
    return
  fi
  find "$session_directory" -type f -name '*.json' -print | wc -l | tr -d ' '
}

terminate_process_tree() {
  local parent_pid=$1
  local signal=$2
  local child_pid
  for child_pid in $(pgrep -P "$parent_pid" 2>/dev/null || true); do
    terminate_process_tree "$child_pid" "$signal"
  done
  kill "-$signal" "$parent_pid" 2>/dev/null || true
}

run_bounded_command() {
  local limit_seconds=$1
  local output=$2
  shift 2

  if [ "$platform" = "Linux" ]; then
    timeout -k 2 "$limit_seconds" "$@" >"$output" 2>&1
    return
  fi

  local timeout_marker="$output.timeout"
  rm -f -- "$timeout_marker"
  "$@" <&0 >"$output" 2>&1 &
  local command_pid=$!
  (
    sleep "$limit_seconds"
    if kill -0 "$command_pid" 2>/dev/null; then
      : >"$timeout_marker"
      terminate_process_tree "$command_pid" TERM
      sleep 2
      terminate_process_tree "$command_pid" KILL
    fi
  ) &
  local watchdog_pid=$!
  local status=0
  wait "$command_pid" || status=$?
  kill "$watchdog_pid" 2>/dev/null || true
  wait "$watchdog_pid" 2>/dev/null || true
  if [ -f "$timeout_marker" ]; then
    rm -f -- "$timeout_marker"
    printf 'bounded command exceeded %s seconds\n' "$limit_seconds" >&2
    return 124
  fi
  return "$status"
}

plain_output="$fixture/plain-redirected.txt"
current_case='redirected-plain-fallback'
(
  cd "$fixture"
  printf '/quit\n' | run_bounded_command 20 "$plain_output" "${offline_environment[@]}" "$binary"
)
grep -Fq 'mode: plain' "$plain_output"
grep -Fq 'Goodbye. Session saved' "$plain_output"

(
  cd "$fixture"
  printf '/quit\n' |
    run_bounded_command 20 "$fixture/plain-explicit.txt" "${offline_environment[@]}" "$binary" chat --plain
)
grep -Fq 'mode: plain' "$fixture/plain-explicit.txt"

(
  cd "$fixture"
  printf '/status\n/quit\n' |
    run_bounded_command 20 "$fixture/plain-dumb-no-color.txt" "${offline_environment[@]}" TERM=dumb NO_COLOR=1 "$binary"
)
grep -Fq 'mode: plain' "$fixture/plain-dumb-no-color.txt"
grep -Fq 'Configured approval preset:' "$fixture/plain-dumb-no-color.txt"
if grep -Fq $'\033' "$fixture/plain-dumb-no-color.txt"; then
  printf '%s\n' 'TERM=dumb/NO_COLOR plain fallback emitted ANSI escapes' >&2
  exit 1
fi

sessions_before="$(session_count)"
current_case='redirected-forced-tui-rejection'
forced_error="$fixture/forced-tui-redirected-error.txt"
if (
  cd "$fixture"
  run_bounded_command 20 "$forced_error" "${offline_environment[@]}" "$binary" --tui
); then
  printf '%s\n' 'forced TUI unexpectedly succeeded without a terminal' >&2
  exit 1
fi
grep -Fq 'use --plain instead' "$forced_error"
if [ "$(session_count)" != "$sessions_before" ]; then
  printf '%s\n' 'forced TUI failure mutated the session store' >&2
  exit 1
fi

current_case='one-shot-contract'
(
  cd "$fixture"
  run_bounded_command 60 "$fixture/run-output.txt" "${offline_environment[@]}" "$binary" run \
    'finish the release smoke' \
    --provider mock --model mock-model --max-steps 4 --yes
)
grep -Fq 'Agent run completed for session' "$fixture/run-output.txt"

quote_for_sh() {
  local value=${1//\'/\'\\\'\'}
  printf "'%s'" "$value"
}

quoted_fixture="$(quote_for_sh "$fixture")"
quoted_binary="$(quote_for_sh "$binary")"
quoted_home="$(quote_for_sh "$isolated_home")"
quoted_config="$(quote_for_sh "$isolated_config")"
offline_prefix="env -u OPENAI_API_KEY -u ANTHROPIC_API_KEY -u GOOGLE_API_KEY -u XAI_API_KEY -u META_API_KEY -u OPENROUTER_API_KEY -u NIB_MANAGED_PROCESS_SCOPE -u NIB_SKILLS_DIR HOME=$quoted_home XDG_CONFIG_HOME=$quoted_config NIB_NO_UPDATE_CHECK=1 NIB_ENABLE_INTERACTIVE_SMOKE=1"
alternate_screen_exit="$(printf '\033[?1049l')"
bracketed_paste_enable="$(printf '\033[?2004h')"
bracketed_paste_disable="$(printf '\033[?2004l')"
terminal_restored_marker='__NIB_TERMINAL_RESTORED__'
child_status_marker='__NIB_INTERACTIVE_CHILD_STATUS__'

quit_tui_input() {
  sleep 0.8
  printf '\021'
}

quit_plain_input() {
  sleep 0.5
  printf '/status\n/quit\n'
}

wait_for_pty_output() {
  local output=$1
  local expected=$2
  local attempts=0
  while [ "$attempts" -lt 100 ]; do
    if [ -f "$output" ] && grep -Fq "$expected" "$output"; then
      return 0
    fi
    attempts=$((attempts + 1))
    sleep 0.1
  done
  printf 'PTY output did not reach the expected prompt: %s\n' "$expected" >&2
  return 1
}

run_pty_case() {
  local label=$1
  local input_function=$2
  local terminal_environment=$3
  local arguments=$4
  local resize=${5:-no}
  local output="$fixture/$label.txt"
  current_case="$label"
  local resize_setup=''
  local resize_finish=''
  if [ "$resize" = "yes" ]; then
    resize_setup='(sleep 3; stty rows 32 cols 100 </dev/tty) & resize_pid=$!;'
    resize_finish='wait "$resize_pid" || true; stty rows 18 cols 50;'
  fi
  local child_command="cd $quoted_fixture || exit 81; stty rows 18 cols 50 || exit 82; before=\$(stty -g) || exit 83; $resize_setup $offline_prefix $terminal_environment $quoted_binary $arguments; child_status=\$?; $resize_finish after=\$(stty -g) || exit 84; if [ \"\$before\" != \"\$after\" ]; then printf '%s\\n' '__NIB_TERMINAL_NOT_RESTORED__'; exit 85; fi; printf '%s\\n' '$terminal_restored_marker'; printf '%s:%s\\n' '$child_status_marker' \"\$child_status\"; exit \"\$child_status\""

  if [ "$platform" = "Linux" ]; then
    "$input_function" |
      run_bounded_command 40 "$output" script -q -e -c "$child_command" /dev/null
  else
    "$input_function" |
      run_bounded_command 40 "$output" script -q /dev/null /bin/sh -c "$child_command"
  fi
  grep -Fq "$terminal_restored_marker" "$output"
  if [ "$(grep -F -c "$child_status_marker:" "$output")" -ne 1 ] ||
    ! grep -Fq "$child_status_marker:0" "$output"; then
    printf 'PTY case %s did not report one successful child status\n' "$label" >&2
    exit 1
  fi
  if grep -Fq '__NIB_TERMINAL_NOT_RESTORED__' "$output"; then
    printf 'terminal state was not restored in PTY case %s\n' "$label" >&2
    exit 1
  fi
}

run_tui_case() {
  local label=$1
  local input_function=$2
  local arguments=$3
  local resize=${4:-no}
  local terminal_environment=${5:-TERM=xterm-256color}
  run_pty_case "$label" "$input_function" "$terminal_environment" "$arguments" "$resize"
  grep -Fq "$alternate_screen_exit" "$fixture/$label.txt"
  grep -Fq "$bracketed_paste_enable" "$fixture/$label.txt"
  grep -Fq "$bracketed_paste_disable" "$fixture/$label.txt"
}

run_tui_case automatic quit_tui_input ''
run_tui_case compatibility quit_tui_input 'tui'
grep -Fq 'compatibility alias' "$fixture/compatibility.txt"

# A PTY with insufficient terminal capabilities must stay usable through the plain
# renderer; it must not enter raw or alternate-screen mode.
run_pty_case dumb-terminal-fallback quit_plain_input 'TERM=dumb NO_COLOR=1' ''
grep -Fq 'mode: plain' "$fixture/dumb-terminal-fallback.txt"
grep -Fq 'Configured approval preset:' "$fixture/dumb-terminal-fallback.txt"
if grep -Fq "$alternate_screen_exit" "$fixture/dumb-terminal-fallback.txt"; then
  printf '%s\n' 'TERM=dumb automatic fallback unexpectedly entered the alternate screen' >&2
  exit 1
fi

# Keep the project dirty so /review and /diff have an authoritative, harmless target.
printf 'interactive review smoke change\n' >>"$fixture/README.md"

plain_semantics_input() {
  printf '%s\n' 'inspect @README.md'
  sleep 0.8
  printf '%s\n' 'n' ''
  sleep 0.8
  printf '%s\n' \
    '/sta' \
    '1' \
    '/permissions' \
    '/review' \
    '/diff' \
    'queue: retained plain follow-up' \
    '/history inspect' \
    '1' \
    'n' \
    '/fork' \
    '/quit'
}

run_pty_case plain-semantics plain_semantics_input 'TERM=xterm-256color NO_COLOR=1' '--plain'
grep -Fq 'mode: plain' "$fixture/plain-semantics.txt"
grep -Fq 'Command completions:' "$fixture/plain-semantics.txt"
grep -Fq 'Configured approval preset:' "$fixture/plain-semantics.txt"
grep -Fq 'README.md' "$fixture/plain-semantics.txt"
grep -Fq 'interactive review smoke change' "$fixture/plain-semantics.txt"
grep -Fq 'queued follow-up retained on session' "$fixture/plain-semantics.txt"
grep -Fq 'Draft history matches for inspect:' "$fixture/plain-semantics.txt"
grep -Fq 'Forked session' "$fixture/plain-semantics.txt"

plain_question_input() {
  sleep 1.2
  printf 'y\n\n'
  sleep 1.2
  printf '2\n\n'
  sleep 1.8
  printf '/quit\n'
}

run_pty_case \
  plain-question \
  plain_question_input \
  'TERM=xterm-256color NO_COLOR=1' \
  "--plain --run 'ask a question before continuing'"
grep -Fq 'Approval required' "$fixture/plain-question.txt"
grep -Fq 'Action: approve_plan' "$fixture/plain-question.txt"
grep -Fq 'Answer (number or text):' "$fixture/plain-question.txt"
grep -Fq '"answer":"full"' "$fixture/plain-question.txt"
grep -Fq 'Goodbye. Session saved' "$fixture/plain-question.txt"

# A real terminal must remain usable after a typed provider failure. Mock exposes this
# credential-free fault only under NIB_ENABLE_INTERACTIVE_SMOKE and this exact goal.
plain_failure_recovery_input() {
  printf '%s\n' 'interactive provider failure smoke'
  sleep 0.8
  printf '%s\n' 'list workspace after provider recovery'
  sleep 0.8
  printf 'y\n\n'
  sleep 1.8
  printf '/status\n/quit\n'
}

run_pty_case \
  plain-provider-failure-recovery \
  plain_failure_recovery_input \
  'TERM=xterm-256color NO_COLOR=1' \
  '--plain'
if [ "$(grep -F -c 'LLM request failed [LLM-AUTH]' "$fixture/plain-provider-failure-recovery.txt")" -ne 1 ]; then
  printf '%s\n' 'provider failure did not render exactly one actionable terminal report' >&2
  exit 1
fi
grep -Fq 'Provider: mock (mock), model: mock-model' "$fixture/plain-provider-failure-recovery.txt"
grep -Fq 'HTTP: 401; retry: not attempted' "$fixture/plain-provider-failure-recovery.txt"
grep -Fq 'Final answer: task complete. (mock LLM response)' "$fixture/plain-provider-failure-recovery.txt"
failure_recovery_session_list="$fixture/failure-recovery-sessions.txt"
grep -F -l 'interactive provider failure smoke' "$session_directory"/*.json \
  >"$failure_recovery_session_list"
if [ "$(wc -l <"$failure_recovery_session_list" | tr -d ' ')" -ne 1 ]; then
  printf '%s\n' 'provider failure recovery did not bind to exactly one session' >&2
  exit 1
fi
failure_recovery_session="$(sed -n '1p' "$failure_recovery_session_list")"
grep -Fq '"outcome": "planning_failed"' "$failure_recovery_session"
grep -Fq '"class": "authentication"' "$failure_recovery_session"
grep -Fq 'list workspace after provider recovery' "$failure_recovery_session"
grep -Fq '"outcome": "completed"' "$failure_recovery_session"
if grep -Fq 'LLM request failed' "$failure_recovery_session"; then
  printf '%s\n' 'rendered provider failure was persisted as chat content' >&2
  exit 1
fi

tui_docks_input() {
  sleep 1.8
  printf 'y'
  sleep 1.8
  printf '\033[B\r'
  sleep 1.8
  printf '\021'
}

run_tui_case \
  tui-approval-question-docks \
  tui_docks_input \
  "--tui --run 'ask a question before continuing'" \
  no \
  'TERM=xterm-256color NO_COLOR=1'
grep -Fq 'ask a question before continuing' "$fixture/tui-approval-question-docks.txt"
grep -Fq 'approval  Action: approve_plan' "$fixture/tui-approval-question-docks.txt"
grep -Fq 'Which verification mode?' "$fixture/tui-approval-question-docks.txt"
grep -Fq 'question  Which verification mode?' "$fixture/tui-approval-question-docks.txt"

tui_composer_input() {
  sleep 0.8
  printf '/hel\t\r'
  sleep 1
  printf '\033[5~'
  sleep 0.5
  printf '\033[6~'
  sleep 0.5
  printf '\033[1;5F'
  sleep 0.5
  printf '/sta\t\r'
  sleep 0.8
  printf '\033[200~edit line\r\nunicode 🙂XY\000\007\033[201~'
  printf '\033[D\033[3~\r'
  sleep 1.8
  printf 'n'
  sleep 1
  printf 'inspect @REA\t\r'
  sleep 1.8
  printf 'n'
  sleep 1
  printf '\022unicode'
  sleep 0.6
  printf '\r'
  sleep 0.6
  printf '\021'
}

run_tui_case tui-composer-scroll-history tui_composer_input '--tui' yes
grep -Fq 'Command' "$fixture/tui-composer-scroll-history.txt"
grep -Fq 'Completion' "$fixture/tui-composer-scroll-history.txt"
grep -Fq 'paused row' "$fixture/tui-composer-scroll-history.txt"
grep -Fq 'tail:following' "$fixture/tui-composer-scroll-history.txt"
grep -Fq 'History | type' "$fixture/tui-composer-scroll-history.txt"
grep -Fq 'README.md' "$fixture/tui-composer-scroll-history.txt"
grep -R -Fq 'edit line\nunicode 🙂X' "$session_directory"
grep -R -Fq '"path": "README.md"' "$session_directory"
if grep -R -Eq '\\u0000|\\u0007|\\u001b' "$session_directory"; then
  printf '%s\n' 'unsafe pasted control data reached the session ledger' >&2
  exit 1
fi

tui_queue_input() {
  sleep 0.8
  printf 'interactive queue smoke\r'
  sleep 0.1
  printf 'steering release verification'
  printf '\023'
  sleep 0.2
  printf 'queued release follow-up\r'
  sleep 2.5
  printf '\003'
  sleep 1.4
  printf '\021'
}

run_tui_case tui-queue-steer-cancel tui_queue_input '--tui'
grep -Fq 'queued follow-up(s) retained on session' "$fixture/tui-queue-steer-cancel.txt"
grep -Fq 'instruction' "$fixture/tui-queue-steer-cancel.txt"
grep -Fq 'persisted' "$fixture/tui-queue-steer-cancel.txt"
grep -Fq 'exact' "$fixture/tui-queue-steer-cancel.txt"
grep -Fq 'active' "$fixture/tui-queue-steer-cancel.txt"
queue_session_list="$fixture/queue-sessions.txt"
grep -F -l 'interactive queue smoke' "$session_directory"/*.json >"$queue_session_list"
if [ "$(wc -l <"$queue_session_list" | tr -d ' ')" -ne 1 ]; then
  printf '%s\n' 'queue smoke goal did not bind to exactly one session' >&2
  exit 1
fi
queue_session_file="$(sed -n '1p' "$queue_session_list")"
grep -Fq 'queued release follow-up' "$queue_session_file"
grep -Fq 'steering release verification' "$queue_session_file"
grep -Fq '"kind": "steering_intake"' "$queue_session_file"
grep -Fq '"kind": "plan_superseded_by_steering"' "$queue_session_file"
if [ "$(grep -F -c '"kind": "run_terminal"' "$queue_session_file")" -ne 1 ] ||
  ! grep -Fq '"outcome": "cancelled_by_user"' "$queue_session_file"; then
  printf '%s\n' 'queue smoke cancellation did not reconcile exactly once' >&2
  exit 1
fi
if grep -Fq 'Run cancelled by user.' "$queue_session_file"; then
  printf '%s\n' 'cancellation leaked synthetic assistant content into the transcript' >&2
  exit 1
fi

# Resume by exact persisted ID through the bounded plain selector, then prove the
# active session changes only after explicit confirmation.
create_session() {
  local output=$1
  (
    cd "$fixture"
    printf '/quit\n' |
      run_bounded_command 20 "$output" "${offline_environment[@]}" "$binary" --plain
  )
  sed -n 's/.*session: \([^ ]*\).*/\1/p' "$output" | head -n 1
}

source_session="$(create_session "$fixture/resume-source.txt")"
target_session="$(create_session "$fixture/resume-target.txt")"
if [ -z "$source_session" ] || [ -z "$target_session" ] || [ "$source_session" = "$target_session" ]; then
  printf '%s\n' 'could not create distinct resume smoke sessions' >&2
  exit 1
fi
quoted_source_session="$(quote_for_sh "$source_session")"
resume_input() {
  local resume_output="$fixture/plain-resume.txt"
  wait_for_pty_output "$resume_output" 'You> '
  printf '/resume\n'
  wait_for_pty_output "$resume_output" 'Session to preview (number or exact ID, blank to cancel): '
  printf '%s\n' "$target_session"
  wait_for_pty_output \
    "$resume_output" \
    "Resume session $target_session instead of $source_session? [y/N]: "
  printf 'y\n'
  wait_for_pty_output \
    "$resume_output" \
    "Resumed session $target_session from persisted state."
  printf '/quit\n'
}
run_pty_case \
  plain-resume \
  resume_input \
  'TERM=xterm-256color NO_COLOR=1' \
  "--plain --session $quoted_source_session"
grep -Fq "Resumed session $target_session from persisted state." "$fixture/plain-resume.txt"
grep -E -q '"forked_from": "[^\"]+' "$session_directory"/*.json

# Lifecycle records retain exact private run IDs in persistence, but terminal
# presentation must never expose them. The configured secret and raw argument schema
# likewise stay absent from every captured renderer output.
private_run_ids_file="$fixture/private-run-ids.list"
sed -n 's/.*"run_id": "\([0-9a-f]\{32\}\)".*/\1/p' "$session_directory"/*.json |
  sort -u >"$private_run_ids_file"
current_case='renderer-privacy-scan'
if [ ! -s "$private_run_ids_file" ]; then
  printf '%s\n' 'interactive smoke did not persist an authoritative run identity' >&2
  exit 1
fi
for session_file in "$session_directory"/*.json; do
  if grep -Fq "$private_sentinel" "$session_file"; then
    printf 'inactive-provider sentinel persisted through %s\n' "$session_file" >&2
    exit 1
  fi
done
for output in "$fixture"/*.txt; do
  if grep -Fq "$private_sentinel" "$output"; then
    printf 'configured private sentinel leaked through %s\n' "$output" >&2
    exit 1
  fi
  if grep -Fq '"arguments"' "$output"; then
    printf 'raw tool arguments leaked through %s\n' "$output" >&2
    exit 1
  fi
  while IFS= read -r run_id; do
    if grep -Fq "$run_id" "$output"; then
      printf 'private run identity leaked through %s\n' "$output" >&2
      exit 1
    fi
  done <"$private_run_ids_file"
done

printf 'Interactive release smoke passed (offline %s PTY and redirected modes).\n' "$platform"

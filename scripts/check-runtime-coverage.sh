#!/usr/bin/env sh
set -eu

minimum="${NIB_RUNTIME_COVERAGE_MIN:-80}"
report="target/runtime-coverage.json"

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "cargo-llvm-cov is required for task coverage" >&2
  echo "Install it with: cargo install cargo-llvm-cov --locked" >&2
  exit 1
fi

cargo llvm-cov --workspace --all-features --json --output-path "$report" -- --test-threads=1

summary="$(jq -c '
  [
    .data[0].files[]
    | select(.filename | test("/src/.*\\.rs$"))
    | .summary.lines
  ]
  | {
      count: (map(.count) | add // 0),
      covered: (map(.covered) | add // 0)
    }
  | .percent = if .count == 0 then 0 else (.covered * 100 / .count) end
' "$report")"

count="$(printf '%s' "$summary" | jq -r '.count')"
covered="$(printf '%s' "$summary" | jq -r '.covered')"
percent="$(printf '%s' "$summary" | jq -r '.percent')"

printf 'Runtime line coverage: %.2f%% (%s/%s)\n' "$percent" "$covered" "$count"
printf '%s' "$summary" | jq -e --argjson minimum "$minimum" '.count > 0 and .percent >= $minimum' >/dev/null

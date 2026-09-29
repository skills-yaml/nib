#!/usr/bin/env sh
set -eu

minimum="${NIB_RUNTIME_COVERAGE_MIN:-80}"
report="target/runtime-coverage.json"
# Keep LLVM profile files under target/ so coverage never clutters the repo root.
# Child test processes can still emit default_*.profraw in cwd; scoop them back.
export LLVM_PROFILE_FILE="${LLVM_PROFILE_FILE:-target/coverage/nib-%p-%m.profraw}"
mkdir -p target/coverage

contain_profraw() {
  find . -name '*.profraw' ! -path './target/*' -exec mv -f {} target/coverage/ \;
  leaked="$(find . -name '*.profraw' ! -path './target/*' -print)"
  if [ -n "$leaked" ]; then
    printf 'coverage profraw leaked outside target/:\n%s\n' "$leaked" >&2
    exit 1
  fi
}

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "cargo-llvm-cov is required for task coverage" >&2
  echo "Install it with: cargo install cargo-llvm-cov --locked" >&2
  exit 1
fi

contain_profraw
# Crash-boundary tests intentionally kill child processes, which can leave an
# incomplete profile. Merge every valid profile and keep the coverage minimum
# as the acceptance gate.
cargo llvm-cov --workspace --all-features --failure-mode all --json --output-path "$report" -- --test-threads=1
contain_profraw

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

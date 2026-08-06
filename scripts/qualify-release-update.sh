#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: qualify-release-update.sh <bootstrap-archive> <bootstrap-commit> <candidate-commit> <candidate-version>" >&2
  exit 2
fi

bootstrap_archive=$1
bootstrap_commit=$2
candidate_commit=$3
expected_candidate_version=$4

if ! [[ "$bootstrap_commit" =~ ^[0-9a-f]{40}$ ]] ||
   ! [[ "$candidate_commit" =~ ^[0-9a-f]{40}$ ]] ||
   [ "$bootstrap_commit" = "$candidate_commit" ] ||
   ! [[ "$expected_candidate_version" =~ ^[0-9A-Za-z][0-9A-Za-z.+-]{0,63}$ ]]; then
  echo "qualification requires two distinct lowercase 40-hex commits and a valid candidate version" >&2
  exit 2
fi
if [ ! -f "$bootstrap_archive" ] || [ ! -f "$bootstrap_archive.sha256" ]; then
  echo "bootstrap archive or checksum is missing" >&2
  exit 2
fi

archive_dir=$(cd "$(dirname "$bootstrap_archive")" && pwd -P)
archive_name=$(basename "$bootstrap_archive")
(
  cd "$archive_dir"
  shasum -a 256 -c "$archive_name.sha256"
)

qualification_root=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/nib-release-update.XXXXXX")
trap 'rm -rf "$qualification_root"' EXIT
install_dir="$qualification_root/install"
mkdir -p "$install_dir"
tar -C "$install_dir" -xzf "$archive_dir/$archive_name"
nib_path="$install_dir/nib"
chmod 755 "$nib_path"

unset NIB_NO_UPDATE_CHECK
bootstrap_version=$(
  NIB_NO_UPDATE_CHECK=1 "$nib_path" version
)
if [[ "$bootstrap_version" =~ ^nib[[:space:]]+([^[:space:]]+)[[:space:]]+\(development[[:space:]]-[[:space:]]$bootstrap_commit\)$ ]]; then
  package_version=${BASH_REMATCH[1]}
else
  echo "unexpected bootstrap identity: $bootstrap_version" >&2
  exit 1
fi
bootstrap_digest=$(shasum -a 256 "$nib_path" | awk '{print $1}')

transcript="$qualification_root/startup-notice.txt"
notice_seen=0
candidate_short=${candidate_commit:0:7}
for attempt in 1 2 3; do
  : >"$transcript"
  if [ "$(uname -s)" = "Darwin" ]; then
    script -q "$transcript" "$nib_path" version >/dev/null 2>&1
  else
    script -q -e -c "$nib_path version" "$transcript" >/dev/null 2>&1
  fi
  if grep -Fq '[nib] Channel update available:' "$transcript" &&
     grep -Fq "$candidate_short" "$transcript"; then
    notice_seen=1
    break
  fi
done
if [ "$notice_seen" -ne 1 ]; then
  echo "bootstrap binary did not emit the candidate update notice in a terminal" >&2
  sed -n '1,80p' "$transcript" >&2
  exit 1
fi

update_output=$("$nib_path" update)
printf '%s\n' "$update_output"
grep -Fq 'Updated nib:' <<<"$update_output"
grep -Fq "${bootstrap_commit:0:7}" <<<"$update_output"
grep -Fq "$candidate_short" <<<"$update_output"

candidate_identity=$(NIB_NO_UPDATE_CHECK=1 "$nib_path" version)
expected_candidate="nib $expected_candidate_version (development - $candidate_commit)"
if [ "$candidate_identity" != "$expected_candidate" ]; then
  echo "updated executable has the wrong identity: $candidate_identity" >&2
  exit 1
fi
candidate_digest=$(shasum -a 256 "$nib_path" | awk '{print $1}')
if [ "$candidate_digest" = "$bootstrap_digest" ]; then
  echo "update did not replace the bootstrap executable bytes" >&2
  exit 1
fi

noop_output=$("$nib_path" update)
printf '%s\n' "$noop_output"
grep -Fq 'nib is already up to date:' <<<"$noop_output"
grep -Fq "$candidate_short" <<<"$noop_output"
noop_digest=$(shasum -a 256 "$nib_path" | awk '{print $1}')
if [ "$noop_digest" != "$candidate_digest" ]; then
  echo "already-current update changed the executable" >&2
  exit 1
fi

#!/usr/bin/env bash

set -euo pipefail

required=(
  GITHUB_REF_NAME
  GITHUB_REPOSITORY
  GITHUB_SHA
  RELEASE_CHANNEL
  RELEASE_PRERELEASE
  RELEASE_TAG
  RELEASE_TITLE
)
for name in "${required[@]}"; do
  if [ -z "${!name:-}" ]; then
    echo "Missing required release environment variable: $name" >&2
    exit 2
  fi
done

case "$RELEASE_CHANNEL:$RELEASE_TAG:$RELEASE_PRERELEASE:$GITHUB_REF_NAME" in
  prod:prod-latest:false:main | development:development-latest:true:development) ;;
  *)
    echo "Invalid release channel, tag, prerelease, or source branch combination." >&2
    exit 2
    ;;
esac
if [[ ! "$GITHUB_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "Invalid GITHUB_REPOSITORY value." >&2
  exit 2
fi
if [[ ! "$GITHUB_SHA" =~ ^[0-9A-Fa-f]{40}$ ]]; then
  echo "Invalid GITHUB_SHA value." >&2
  exit 2
fi

git_bin=${NIB_RELEASE_GIT_BIN:-git}
gh_bin=${NIB_RELEASE_GH_BIN:-gh}
origin=${NIB_RELEASE_ORIGIN:-origin}
dist_dir=${NIB_RELEASE_DIST_DIR:-dist}
stage_tag="nib-release-stage-$RELEASE_CHANNEL"
backup_tag="nib-release-backup-$RELEASE_CHANNEL"
stage_visibility_attempts=12
stage_visibility_delay_seconds=${NIB_RELEASE_STAGE_VISIBILITY_DELAY_SECONDS:-2}
case "$stage_visibility_delay_seconds" in
  0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10) ;;
  *)
    echo "Invalid NIB_RELEASE_STAGE_VISIBILITY_DELAY_SECONDS value." >&2
    exit 2
    ;;
esac
committed=0
transaction_started=0
notes="Automated $RELEASE_CHANNEL build from branch $GITHUB_REF_NAME at commit $GITHUB_SHA."

archive_names=(
  nib-linux-x86_64.tar.gz
  nib-macos-aarch64.tar.gz
  nib-macos-x86_64.tar.gz
  nib-windows-x86_64.zip
)
checksum_names=(
  nib-linux-x86_64.tar.gz.sha256
  nib-macos-aarch64.tar.gz.sha256
  nib-macos-x86_64.tar.gz.sha256
  nib-windows-x86_64.zip.sha256
)
manifest_name=nib-release.json
expected_asset_names=("${archive_names[@]}" "${checksum_names[@]}" "$manifest_name")
expected_asset_listing=$(printf '%s\n' "${expected_asset_names[@]}" | LC_ALL=C sort)
legacy_asset_names=("${archive_names[@]}" "${checksum_names[@]}")
legacy_asset_listing=$(printf '%s\n' "${legacy_asset_names[@]}" | LC_ALL=C sort)
marker_start='<!-- nib-release-transaction-v1'

build_transaction_body() {
  local candidate_sha=$1
  local prior_sha=${2:-none}
  local staged_release_id=${3:-pending}
  local prior_release_id=${4:-none}
  local transaction_mode=${5:-rollback}
  local transaction_phase=${6:-staged}
  cat <<EOF
$notes

$marker_start
channel=$RELEASE_CHANNEL
candidate_sha=$candidate_sha
prior_sha=$prior_sha
staged_release_id=$staged_release_id
prior_release_id=$prior_release_id
prior_release_draft=false
transaction_mode=$transaction_mode
transaction_phase=$transaction_phase
-->
EOF
}

marker_value() {
  local body=$1
  local key=$2
  local allow_missing=${3:-false}
  local matches first remainder
  matches=$(printf '%s\n' "$body" | awk -v start="$marker_start" -v key="$key" '
    $0 == start { inside = 1; next }
    inside && $0 == "-->" { inside = 0; next }
    inside && index($0, key "=") == 1 { print substr($0, length(key) + 2) }
  ')
  first=${matches%%$'\n'*}
  if [ "$first" = "$matches" ]; then
    remainder=
  else
    remainder=${matches#*$'\n'}
  fi
  if [ -z "$first" ]; then
    [ "$allow_missing" = true ]
    return
  fi
  if [ -n "$remainder" ]; then
    return 1
  fi
  printf '%s' "$first"
}

load_transaction_marker() {
  local release_id=$1
  local expected_candidate=$2
  local structure
  marker_body=$(release_field "$release_id" body) || return 1
  structure=$(printf '%s\n' "$marker_body" | awk -v start="$marker_start" '
    $0 == start { starts += 1; inside += 1; next }
    inside > 0 && $0 == "-->" { ends += 1; inside -= 1 }
    END { printf "%d:%d:%d", starts, ends, inside }
  ')
  if [ "$structure" != '1:1:0' ]; then
    echo "Release $release_id has a missing or ambiguous transaction marker." >&2
    return 1
  fi
  marker_channel=$(marker_value "$marker_body" channel) || return 1
  marker_candidate_sha=$(marker_value "$marker_body" candidate_sha) || return 1
  marker_prior_sha=$(marker_value "$marker_body" prior_sha) || return 1
  marker_staged_release_id=$(marker_value "$marker_body" staged_release_id) || return 1
  marker_prior_release_id=$(marker_value "$marker_body" prior_release_id) || return 1
  marker_prior_release_draft=$(marker_value "$marker_body" prior_release_draft) || return 1
  marker_transaction_mode=$(marker_value "$marker_body" transaction_mode true) || return 1
  marker_transaction_phase=$(marker_value "$marker_body" transaction_phase true) || return 1
  [ -n "$marker_transaction_mode" ] || marker_transaction_mode=rollback
  [ -n "$marker_transaction_phase" ] || marker_transaction_phase=staged
  if [ "$marker_channel" != "$RELEASE_CHANNEL" ] || { [ -n "$expected_candidate" ] && [ "$marker_candidate_sha" != "$expected_candidate" ]; }; then
    echo "Release $release_id transaction marker does not match this channel and candidate." >&2
    return 1
  fi
  if [[ ! "$marker_candidate_sha" =~ ^[0-9A-Fa-f]{40,64}$ ]]; then
    return 1
  fi
  if [ "$marker_prior_sha" != none ] && [[ ! "$marker_prior_sha" =~ ^[0-9A-Fa-f]{40,64}$ ]]; then
    return 1
  fi
  if [ "$marker_staged_release_id" != pending ] && [ "$marker_staged_release_id" != "$release_id" ]; then
    echo "Release $release_id transaction marker names another staged release." >&2
    return 1
  fi
  if [[ ! "$release_id" =~ ^[A-Za-z0-9_-]{1,64}$ ]] || { [ "$marker_staged_release_id" != pending ] && [[ ! "$marker_staged_release_id" =~ ^[A-Za-z0-9_-]{1,64}$ ]]; }; then
    echo "Release transaction marker contains an invalid staged release ID." >&2
    return 1
  fi
  if [ "$marker_prior_release_id" != none ] && [[ ! "$marker_prior_release_id" =~ ^[A-Za-z0-9_-]{1,64}$ ]]; then
    echo "Release transaction marker contains an invalid prior release ID." >&2
    return 1
  fi
  if [ "$marker_prior_release_id" = "$release_id" ]; then
    echo "Release transaction marker reuses the staged release as its predecessor." >&2
    return 1
  fi
  if [ "$marker_prior_release_draft" != false ]; then
    return 1
  fi
  if [ "$marker_transaction_mode" != rollback ] && [ "$marker_transaction_mode" != forward-only ]; then
    return 1
  fi
  if [ "$marker_transaction_phase" != staged ] && [ "$marker_transaction_phase" != forward ]; then
    return 1
  fi
}

remote_sha() {
  local ref=$1
  "$git_bin" ls-remote "$origin" "$ref" |
    awk 'NR == 1 { value = $1 } END { print value }'
}

release_id_for_tag() {
  local tag=$1
  local matches first remainder
  matches=$("$gh_bin" api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100" --jq ".[] | select(.tag_name == \"$tag\") | .id") || {
    echo "Failed to list releases for transaction tag $tag." >&2
    return 1
  }
  first=${matches%%$'\n'*}
  if [ "$first" = "$matches" ]; then
    remainder=
  else
    remainder=${matches#*$'\n'}
  fi
  if [ -n "$remainder" ]; then
    echo "Multiple releases use transaction tag $tag." >&2
    return 1
  fi
  printf '%s' "$first"
}

release_id_for_untagged_transaction() {
  local matches first remainder
  matches=$("$gh_bin" api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100" \
    --jq ".[] | select(.draft == true and (.tag_name | startswith(\"untagged-\")) and ((.body // \"\") | contains(\"\\nchannel=$RELEASE_CHANNEL\\n\"))) | .id") || {
    echo "Failed to list untagged draft transactions for $RELEASE_CHANNEL." >&2
    return 1
  }
  first=${matches%%$'\n'*}
  if [ "$first" = "$matches" ]; then
    remainder=
  else
    remainder=${matches#*$'\n'}
  fi
  if [ -n "$remainder" ]; then
    echo "Multiple untagged draft transactions exist for $RELEASE_CHANNEL." >&2
    return 1
  fi
  if [ -n "$first" ] && [[ ! "$first" =~ ^[A-Za-z0-9_-]{1,64}$ ]]; then
    echo "Untagged draft transaction has an invalid release ID." >&2
    return 1
  fi
  printf '%s' "$first"
}

release_id_for_staged_transaction() {
  local tagged_id untagged_id
  tagged_id=$(release_id_for_tag "$stage_tag") || return 1
  untagged_id=$(release_id_for_untagged_transaction) || return 1
  if [ -n "$tagged_id" ] && [ -n "$untagged_id" ] && [ "$tagged_id" != "$untagged_id" ]; then
    echo "Both tagged and untagged draft transactions exist for $RELEASE_CHANNEL." >&2
    return 1
  fi
  printf '%s' "${tagged_id:-$untagged_id}"
}

release_tag_for_id() {
  local release_id=$1
  local matches first remainder
  matches=$("$gh_bin" api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100" --jq ".[] | select((.id | tostring) == \"$release_id\") | .tag_name") || {
    echo "Failed to find release ID $release_id." >&2
    return 1
  }
  first=${matches%%$'\n'*}
  if [ "$first" = "$matches" ]; then
    remainder=
  else
    remainder=${matches#*$'\n'}
  fi
  if [ -n "$remainder" ]; then
    echo "Release ID $release_id appears more than once." >&2
    return 1
  fi
  printf '%s' "$first"
}

release_field() {
  local release_id=$1
  local field=$2
  "$gh_bin" api "repos/$GITHUB_REPOSITORY/releases/$release_id" --jq ".$field"
}

release_asset_names() {
  local release_id=$1
  "$gh_bin" api "repos/$GITHUB_REPOSITORY/releases/$release_id" --jq '.assets[].name' |
    LC_ALL=C sort
}

release_incomplete_asset_names() {
  local release_id=$1
  "$gh_bin" api "repos/$GITHUB_REPOSITORY/releases/$release_id" --jq '.assets[] | select(.state != "uploaded" or .size <= 0) | .name' |
    LC_ALL=C sort
}

patch_release() {
  local release_id=$1
  shift
  "$gh_bin" api --method PATCH "repos/$GITHUB_REPOSITORY/releases/$release_id" "$@" >/dev/null
}

read_exact_release_tag() {
  local release_id=$1
  local expected=$2
  local description=$3
  local actual
  actual=$(release_field "$release_id" tag_name) || {
    echo "Failed to read $description release $release_id; refusing to mutate it." >&2
    return 1
  }
  if [ "$actual" != "$expected" ]; then
    echo "$description release $release_id changed from $expected to $actual; refusing to mutate it." >&2
    return 1
  fi
}

release_has_expected_assets() {
  local release_id=$1
  local actual incomplete name unexpected=
  actual=$(release_asset_names "$release_id") || return 2
  if [ "$actual" != "$expected_asset_listing" ]; then
    while IFS= read -r name; do
      [ -z "$name" ] && continue
      if ! printf '%s\n' "$expected_asset_listing" | grep -Fxq -- "$name"; then
        unexpected="${unexpected}${unexpected:+, }$name"
      fi
    done <<<"$actual"
    if [ -n "$unexpected" ]; then
      echo "Release $release_id contains unexpected asset names: $unexpected" >&2
      return 3
    fi
    echo "Release $release_id does not contain the exact expected asset names." >&2
    return 1
  fi
  incomplete=$(release_incomplete_asset_names "$release_id") || return 2
  if [ -n "$incomplete" ]; then
    echo "Release $release_id contains incomplete or empty assets: $incomplete" >&2
    return 1
  fi
}

wait_for_staged_transaction() {
  local candidate_sha=$1
  local attempt release_id observed_release_id= observed_marker_body= asset_status
  for ((attempt = 1; attempt <= stage_visibility_attempts; attempt++)); do
    release_id=$(release_id_for_staged_transaction) || return 1
    if [ -n "$release_id" ]; then
      if [ -n "$observed_release_id" ] && [ "$release_id" != "$observed_release_id" ]; then
        echo "Staged release identity changed from $observed_release_id to $release_id." >&2
        return 1
      fi
      observed_release_id=$release_id
    elif [ -n "$observed_release_id" ]; then
      release_id=$observed_release_id
    fi
    if [ -n "$release_id" ]; then
      release_is_owned_staged_transaction "$release_id" "$candidate_sha" || return 1
      if [ -n "$observed_marker_body" ] && [ "$marker_body" != "$observed_marker_body" ]; then
        echo "Staged release $release_id transaction marker changed during visibility checks." >&2
        return 1
      fi
      observed_marker_body=$marker_body
      if release_has_expected_assets "$release_id"; then
        printf '%s' "$release_id"
        return 0
      else
        asset_status=$?
        if [ "$asset_status" -eq 2 ]; then
          echo "Failed to read staged release $release_id assets." >&2
          return 1
        elif [ "$asset_status" -ne 1 ]; then
          echo "Staged release $release_id exposed invalid asset metadata." >&2
          return 1
        fi
      fi
    fi
    if [ "$attempt" -lt "$stage_visibility_attempts" ]; then
      sleep "$stage_visibility_delay_seconds"
    fi
  done
  if [ -z "$observed_release_id" ]; then
    echo "Staged release did not become visible after $stage_visibility_attempts attempts." >&2
  else
    echo "Staged release $observed_release_id did not expose the exact expected assets after $stage_visibility_attempts attempts." >&2
  fi
  return 1
}

release_has_supported_prior_assets() {
  local release_id=$1
  local actual incomplete
  actual=$(release_asset_names "$release_id") || return 1
  if [ "$actual" != "$expected_asset_listing" ] && [ "$actual" != "$legacy_asset_listing" ]; then
    echo "Prior release $release_id does not contain a supported asset set." >&2
    return 1
  fi
  incomplete=$(release_incomplete_asset_names "$release_id") || return 1
  if [ -n "$incomplete" ]; then
    echo "Prior release $release_id contains incomplete or empty assets: $incomplete" >&2
    return 1
  fi
}

release_has_expected_state() {
  local release_id=$1
  local expected_tag=$2
  local expected_draft=$3
  local tag draft prerelease
  tag=$(release_field "$release_id" tag_name) || return 1
  draft=$(release_field "$release_id" draft) || return 1
  prerelease=$(release_field "$release_id" prerelease) || return 1
  [ "$tag" = "$expected_tag" ] && [ "$draft" = "$expected_draft" ] && [ "$prerelease" = "$RELEASE_PRERELEASE" ]
}

release_is_coherent() {
  local release_id=$1
  local expected_tag=$2
  local expected_draft=$3
  release_has_expected_state "$release_id" "$expected_tag" "$expected_draft" && release_has_expected_assets "$release_id"
}

release_is_prior_coherent() {
  local release_id=$1
  local expected_tag=$2
  local expected_draft=$3
  release_has_expected_state "$release_id" "$expected_tag" "$expected_draft" && release_has_supported_prior_assets "$release_id"
}

is_staged_release_tag() {
  local tag=$1
  [ "$tag" = "$stage_tag" ] || [[ "$tag" =~ ^untagged-[0-9a-f]{20}$ ]]
}

release_is_owned_staged_transaction() {
  local release_id=$1
  local candidate_sha=$2
  local tag draft prerelease target
  tag=$(release_field "$release_id" tag_name) || return 1
  draft=$(release_field "$release_id" draft) || return 1
  prerelease=$(release_field "$release_id" prerelease) || return 1
  target=$(release_field "$release_id" target_commitish) || return 1
  if ! is_staged_release_tag "$tag" || [ "$draft" != true ] ||
    [ "$prerelease" != "$RELEASE_PRERELEASE" ] || [ "$target" != "$candidate_sha" ]; then
    echo "Release $release_id is not the exact private staged transaction." >&2
    return 1
  fi
  load_transaction_marker "$release_id" "$candidate_sha" || return 1
  [ "$marker_staged_release_id" = pending ] || [ "$marker_staged_release_id" = "$release_id" ]
}

release_is_staged_transaction() {
  local release_id=$1
  local candidate_sha=$2
  release_is_owned_staged_transaction "$release_id" "$candidate_sha" || return 1
  release_has_expected_assets "$release_id"
}

patch_owned_staged_release() {
  local release_id=$1
  local candidate_sha=$2
  shift 2
  release_is_owned_staged_transaction "$release_id" "$candidate_sha" || return 1
  patch_release "$release_id" "$@"
}

patch_staged_release() {
  local release_id=$1
  local candidate_sha=$2
  shift 2
  release_is_staged_transaction "$release_id" "$candidate_sha" || return 1
  patch_release "$release_id" "$@"
}

delete_staged_release() {
  local release_id=$1
  local candidate_sha=$2
  local remaining
  release_is_owned_staged_transaction "$release_id" "$candidate_sha" || return 1
  "$gh_bin" api --method DELETE "repos/$GITHUB_REPOSITORY/releases/$release_id" >/dev/null || true
  remaining=$(release_tag_for_id "$release_id") || return 1
  if [ -z "$remaining" ]; then
    return 0
  fi
  echo "Failed to delete staged transaction release $release_id at $remaining." >&2
  return 1
}

patch_release_from_tag() {
  local release_id=$1
  local expected_tag=$2
  shift 2
  read_exact_release_tag "$release_id" "$expected_tag" "transaction-owned" || return 1
  patch_release "$release_id" "$@"
}

finalize_transaction_marker() {
  local release_id=$1
  local candidate_sha=$2
  local finalized_body
  release_is_owned_staged_transaction "$release_id" "$candidate_sha" || return 1
  if [ "$marker_staged_release_id" = "$release_id" ]; then
    return 0
  fi
  if [ "$marker_staged_release_id" != pending ]; then
    return 1
  fi
  finalized_body=${marker_body/staged_release_id=pending/staged_release_id=$release_id}
  patch_owned_staged_release "$release_id" "$candidate_sha" -f body="$finalized_body" || true
  release_is_owned_staged_transaction "$release_id" "$candidate_sha" || return 1
  [ "$marker_staged_release_id" = "$release_id" ]
}

delete_release_from_tag() {
  local release_id=$1
  local expected_tag=$2
  local remaining
  read_exact_release_tag "$release_id" "$expected_tag" "transaction-owned" || return 1
  "$gh_bin" api --method DELETE "repos/$GITHUB_REPOSITORY/releases/$release_id" >/dev/null || true
  remaining=$(release_tag_for_id "$release_id") || return 1
  if [ -z "$remaining" ]; then
    return 0
  fi
  if [ "$remaining" = "$expected_tag" ]; then
    echo "Failed to delete transaction-owned release $release_id." >&2
  else
    echo "Release $release_id changed to $remaining while it was being deleted." >&2
  fi
  return 1
}

delete_owned_ref() {
  local ref=$1
  local expected=$2
  local description=$3
  local current
  current=$(remote_sha "$ref") || {
    echo "Failed to read $description; refusing to delete it." >&2
    return 1
  }
  if [ "$current" = "$expected" ]; then
    "$git_bin" push --force-with-lease="$ref:$expected" "$origin" ":$ref" >/dev/null || true
    current=$(remote_sha "$ref") || return 1
    if [ -z "$current" ]; then
      return 0
    fi
    if [ "$current" = "$expected" ]; then
      echo "Failed to delete $description." >&2
    else
      echo "Refusing to delete $description changed by another actor: $current" >&2
    fi
    return 1
  elif [ -n "$current" ]; then
    echo "Refusing to delete $description changed by another actor: $current" >&2
    return 1
  fi
}

delete_stage_ref() {
  local stage_sha=$1
  delete_owned_ref "refs/tags/$stage_tag" "$stage_sha" "staging tag"
}

delete_backup_ref() {
  local backup_sha=$1
  delete_owned_ref "refs/tags/$backup_tag" "$backup_sha" "backup tag"
}

move_rolling_ref_via_api() {
  local expected=$1
  local desired=$2
  local current
  current=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  if [ "$current" = "$desired" ]; then
    return 0
  fi
  if [ "$current" != "$expected" ]; then
    echo "Rolling tag changed to an unowned SHA: $current" >&2
    return 1
  fi
  if [ -n "$current" ]; then
    "$gh_bin" api --method PATCH "repos/$GITHUB_REPOSITORY/git/refs/tags/$RELEASE_TAG" -f sha="$desired" -F force=true >/dev/null || true
  else
    "$gh_bin" api --method POST "repos/$GITHUB_REPOSITORY/git/refs" -f ref="refs/tags/$RELEASE_TAG" -f sha="$desired" >/dev/null || true
  fi
  current=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  if [ "$current" != "$desired" ]; then
    echo "Rolling tag did not reach the transaction's expected state." >&2
    return 1
  fi
}

create_stage_ref_via_api() {
  local desired=$1
  local current
  current=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ -n "$current" ]; then
    echo "Refusing to replace an existing staging tag: $current" >&2
    return 1
  fi
  "$gh_bin" api --method POST "repos/$GITHUB_REPOSITORY/git/refs" -f ref="refs/tags/$stage_tag" -f sha="$desired" >/dev/null || return 1
  current=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ "$current" != "$desired" ]; then
    echo "Staging tag did not reach the candidate SHA." >&2
    return 1
  fi
}

delete_stage_ref_via_api() {
  local expected=$1
  local current
  current=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ -z "$current" ]; then
    return 0
  fi
  if [ "$current" != "$expected" ]; then
    echo "Refusing to delete staging tag changed by another actor: $current" >&2
    return 1
  fi
  "$gh_bin" api --method DELETE "repos/$GITHUB_REPOSITORY/git/refs/tags/$stage_tag" >/dev/null || true
  current=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ -n "$current" ]; then
    echo "Failed to delete staging tag." >&2
    return 1
  fi
}

validate_local_assets() {
  local index archive checksum line hash
  local archives=("$dist_dir"/*.tar.gz "$dist_dir"/*.zip)
  local checksums=("$dist_dir"/*.sha256)
  if [ "${#archives[@]}" -ne 4 ] || [ "${#checksums[@]}" -ne 4 ]; then
    echo "Release transaction requires exactly four archives and four checksum assets." >&2
    return 1
  fi
  for index in "${!archive_names[@]}"; do
    archive=${archive_names[$index]}
    checksum=${checksum_names[$index]}
    if [ ! -f "$dist_dir/$archive" ] || [ -L "$dist_dir/$archive" ]; then
      echo "Missing regular release archive: $archive" >&2
      return 1
    fi
    if [ ! -f "$dist_dir/$checksum" ] || [ -L "$dist_dir/$checksum" ]; then
      echo "Missing regular release checksum: $checksum" >&2
      return 1
    fi
    line=$(tr -d '\r\n' <"$dist_dir/$checksum")
    hash=${line%% *}
    if [ "${#hash}" -ne 64 ] || [[ "$hash" == *[!0-9A-Fa-f]* ]] || [ "$line" != "$hash  $archive" ]; then
      echo "Invalid checksum manifest for $archive." >&2
      return 1
    fi
    (
      cd "$dist_dir"
      sha256sum --check --status "$checksum"
    ) || {
      echo "Checksum verification failed for $archive." >&2
      return 1
    }
  done
}

generate_release_manifest() {
  local release_version=${NIB_RELEASE_VERSION:-}
  local index archive checksum line hash size comma manifest_tmp normalized_commit
  if [ -z "$release_version" ]; then
    if [ ! -f Cargo.toml ] || [ -L Cargo.toml ]; then
      echo "Cannot resolve the release package version from Cargo.toml." >&2
      return 1
    fi
    release_version=$(awk '
      /^\[package\][[:space:]]*$/ { in_package = 1; next }
      /^\[/ { in_package = 0 }
      in_package && /^[[:space:]]*version[[:space:]]*=/ {
        value = $0
        sub(/^[^=]*=[[:space:]]*"/, "", value)
        sub(/"[[:space:]]*$/, "", value)
        print value
        exit
      }
    ' Cargo.toml)
  fi
  if [[ ! "$release_version" =~ ^[0-9A-Za-z][0-9A-Za-z.+-]{0,63}$ ]]; then
    echo "Invalid release package version." >&2
    return 1
  fi

  manifest_tmp="$dist_dir/.nib-release.json.tmp.$$"
  normalized_commit=$(printf '%s' "$GITHUB_SHA" | tr '[:upper:]' '[:lower:]')
  umask 077
  {
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "repository": "%s",\n' "$GITHUB_REPOSITORY"
    printf '  "channel": "%s",\n' "$RELEASE_CHANNEL"
    printf '  "tag": "%s",\n' "$RELEASE_TAG"
    printf '  "version": "%s",\n' "$release_version"
    printf '  "commit": "%s",\n' "$normalized_commit"
    printf '  "assets": {\n'
    for index in "${!archive_names[@]}"; do
      archive=${archive_names[$index]}
      checksum=${checksum_names[$index]}
      line=$(tr -d '\r\n' <"$dist_dir/$checksum")
      hash=${line%% *}
      hash=$(printf '%s' "$hash" | tr '[:upper:]' '[:lower:]')
      size=$(wc -c <"$dist_dir/$archive" | tr -d '[:space:]')
      comma=,
      if [ "$index" -eq "$((${#archive_names[@]} - 1))" ]; then
        comma=
      fi
      printf '    "%s": {"sha256": "%s", "size": %s}%s\n' "$archive" "$hash" "$size" "$comma"
    done
    printf '  }\n'
    printf '}\n'
  } >"$manifest_tmp"
  mv -f "$manifest_tmp" "$dist_dir/$manifest_name"
  if [ ! -f "$dist_dir/$manifest_name" ] || [ -L "$dist_dir/$manifest_name" ]; then
    echo "Failed to create a regular release manifest." >&2
    return 1
  fi
}

promote_stage_release() {
  local release_id=$1
  local candidate_sha=$2
  release_is_staged_transaction "$release_id" "$candidate_sha" || return 1
  if [ "$marker_staged_release_id" != "$release_id" ]; then
    return 1
  fi
  patch_staged_release "$release_id" "$candidate_sha" -f tag_name="$RELEASE_TAG" -f name="$RELEASE_TITLE" -f body="$marker_body" -F draft=false -F prerelease="$RELEASE_PRERELEASE" -f target_commitish="$candidate_sha" || true
  release_is_coherent "$release_id" "$RELEASE_TAG" false || return 1
  load_transaction_marker "$release_id" "$candidate_sha"
}

detach_promoted_release() {
  local release_id=$1
  local candidate_sha=$2
  load_transaction_marker "$release_id" "$candidate_sha" || return 1
  patch_release_from_tag "$release_id" "$RELEASE_TAG" -f tag_name="$stage_tag" -F draft=true || true
  release_is_staged_transaction "$release_id" "$candidate_sha"
}

backup_stable_release() {
  local release_id=$1
  release_is_prior_coherent "$release_id" "$RELEASE_TAG" false || return 1
  patch_release_from_tag "$release_id" "$RELEASE_TAG" -f tag_name="$backup_tag" -F draft=true || true
  release_is_prior_coherent "$release_id" "$backup_tag" true
}

restore_backup_release() {
  local release_id=$1
  release_is_prior_coherent "$release_id" "$backup_tag" true || return 1
  patch_release_from_tag "$release_id" "$backup_tag" -f tag_name="$RELEASE_TAG" -F draft=false || true
  release_is_prior_coherent "$release_id" "$RELEASE_TAG" false
}

release_has_marker_start() {
  local release_id=$1
  local body
  body=$(release_field "$release_id" body) || return 2
  printf '%s\n' "$body" | grep -Fqx "$marker_start"
}

move_rolling_ref() {
  local expected=$1
  local desired=$2
  local source_ref=$3
  local current
  current=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  if [ "$current" = "$desired" ]; then
    return 0
  fi
  if [ "$current" != "$expected" ]; then
    echo "Rolling tag changed to an unowned SHA: $current" >&2
    return 1
  fi
  if [ -n "$desired" ]; then
    "$git_bin" fetch --no-tags "$origin" "$source_ref" >/dev/null || return 1
    "$git_bin" push --force-with-lease="refs/tags/$RELEASE_TAG:$expected" "$origin" "$desired:refs/tags/$RELEASE_TAG" >/dev/null || true
  else
    "$git_bin" push --force-with-lease="refs/tags/$RELEASE_TAG:$expected" "$origin" ":refs/tags/$RELEASE_TAG" >/dev/null || true
  fi
  current=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  if [ "$current" != "$desired" ]; then
    echo "Rolling tag did not reach the transaction's expected state." >&2
    return 1
  fi
}

move_rolling_ref_to_candidate() {
  local expected=$1
  local candidate_sha=$2
  local stage_ref
  stage_ref=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ -n "$stage_ref" ]; then
    if [ "$stage_ref" != "$candidate_sha" ]; then
      echo "Staging tag changed before rolling-tag recovery." >&2
      return 1
    fi
    move_rolling_ref "$expected" "$candidate_sha" "refs/tags/$stage_tag"
  else
    move_rolling_ref_via_api "$expected" "$candidate_sha"
  fi
}

cleanup_forward_state() {
  local candidate_sha=$1
  local prior_sha=$2
  local candidate_release_id=$3
  local prior_release_id=$4
  local rolling prior_tag backup_ref stable_release_id stage_ref actual_backup_release_id
  rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  stable_release_id=$(release_id_for_tag "$RELEASE_TAG") || return 1
  if [ "$rolling" != "$candidate_sha" ] || [ "$stable_release_id" != "$candidate_release_id" ]; then
    echo "Forward release state is not coherent; retaining transaction artifacts." >&2
    return 1
  fi
  release_is_coherent "$candidate_release_id" "$RELEASE_TAG" false || return 1
  load_transaction_marker "$candidate_release_id" "$candidate_sha" || return 1
  if [ "$marker_staged_release_id" != "$candidate_release_id" ]; then
    return 1
  fi

  stage_ref=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ -n "$stage_ref" ] && [ "$stage_ref" != "$candidate_sha" ]; then
    echo "Staging tag changed to an unowned SHA: $stage_ref" >&2
    return 1
  fi
  backup_ref=$(remote_sha "refs/tags/$backup_tag") || return 1
  if [ -n "$backup_ref" ] && [ "$backup_ref" != "$prior_sha" ]; then
    echo "Backup tag changed to an unowned SHA: $backup_ref" >&2
    return 1
  fi
  actual_backup_release_id=$(release_id_for_tag "$backup_tag") || return 1
  if [ -n "$prior_release_id" ]; then
    if [ -n "$actual_backup_release_id" ] && [ "$actual_backup_release_id" != "$prior_release_id" ]; then
      echo "Backup tag names an unrecorded release." >&2
      return 1
    fi
  elif [ -n "$actual_backup_release_id" ]; then
    echo "Unexpected backup release exists for a transaction without a prior release." >&2
    return 1
  fi

  if [ -n "$prior_release_id" ]; then
    prior_tag=$(release_tag_for_id "$prior_release_id") || return 1
    if [ "$prior_tag" = "$backup_tag" ]; then
      if [ "$backup_ref" != "$prior_sha" ]; then
        echo "Backup release exists without its recorded backup ref." >&2
        return 1
      fi
      release_is_prior_coherent "$prior_release_id" "$backup_tag" true || return 1
      delete_release_from_tag "$prior_release_id" "$backup_tag" || return 1
    elif [ -n "$prior_tag" ]; then
      echo "Prior release $prior_release_id changed to $prior_tag; retaining transaction state." >&2
      return 1
    fi
  fi

  if [ -n "$backup_ref" ]; then
    delete_backup_ref "$prior_sha" || return 1
  fi
  if [ -n "$stage_ref" ]; then
    delete_stage_ref "$candidate_sha" || return 1
  fi
}

cleanup_rollback_state() {
  local candidate_sha=$1
  local prior_sha=$2
  local candidate_release_id=$3
  local prior_release_id=$4
  local rolling stable_release_id stage_ref backup_ref backup_release_id
  rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  stable_release_id=$(release_id_for_tag "$RELEASE_TAG") || return 1
  if [ "$rolling" != "$prior_sha" ]; then
    echo "Rollback ref state is not coherent; retaining transaction artifacts." >&2
    return 1
  fi
  if [ -n "$prior_release_id" ]; then
    if [ "$stable_release_id" != "$prior_release_id" ]; then
      echo "Rollback did not restore the recorded prior release." >&2
      return 1
    fi
    release_is_prior_coherent "$prior_release_id" "$RELEASE_TAG" false || return 1
  elif [ -n "$stable_release_id" ]; then
    echo "Rollback unexpectedly left a stable release." >&2
    return 1
  fi
  release_is_owned_staged_transaction "$candidate_release_id" "$candidate_sha" || return 1
  if [ "$marker_staged_release_id" != "$candidate_release_id" ]; then
    return 1
  fi

  stage_ref=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ -n "$stage_ref" ] && [ "$stage_ref" != "$candidate_sha" ]; then
    echo "Staging tag changed to an unowned SHA: $stage_ref" >&2
    return 1
  fi
  backup_ref=$(remote_sha "refs/tags/$backup_tag") || return 1
  if [ -n "$backup_ref" ] && [ "$backup_ref" != "$prior_sha" ]; then
    echo "Backup tag does not match the recorded rollback predecessor." >&2
    return 1
  fi
  backup_release_id=$(release_id_for_tag "$backup_tag") || return 1
  if [ -n "$backup_release_id" ]; then
    echo "Rollback cleanup found an un-restored backup release." >&2
    return 1
  fi
  if [ -n "$stage_ref" ]; then
    delete_stage_ref "$candidate_sha" || return 1
  fi
  if [ -n "$backup_ref" ]; then
    delete_backup_ref "$prior_sha" || return 1
  fi
  delete_staged_release "$candidate_release_id" "$candidate_sha" || return 1
}

recover_forward() {
  local candidate_sha=$1
  local prior_sha=$2
  local candidate_release_id=$3
  local prior_release_id=$4
  local candidate_tag prior_tag rolling backup_ref stage_ref
  candidate_tag=$(release_tag_for_id "$candidate_release_id") || return 1
  if [ -n "$prior_release_id" ]; then
    prior_tag=$(release_tag_for_id "$prior_release_id") || return 1
    if [ "$prior_tag" != "$RELEASE_TAG" ] && [ "$prior_tag" != "$backup_tag" ] && [ -n "$prior_tag" ]; then
      echo "Recorded prior release changed to $prior_tag; refusing forward recovery." >&2
      return 1
    fi
  fi

  stage_ref=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ -n "$stage_ref" ] && [ "$stage_ref" != "$candidate_sha" ]; then
    echo "Staging tag changed before forward recovery." >&2
    return 1
  fi
  backup_ref=$(remote_sha "refs/tags/$backup_tag") || return 1
  if [ "$backup_ref" != "$prior_sha" ]; then
    if [ "$candidate_tag" != "$RELEASE_TAG" ] || [ -n "${prior_tag:-}" ]; then
      echo "Backup tag changed before forward recovery." >&2
      return 1
    fi
  fi

  if [ -n "$prior_release_id" ] && [ "$prior_tag" = "$RELEASE_TAG" ]; then
    backup_stable_release "$prior_release_id" || return 1
    prior_tag=$backup_tag
  fi

  rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  if [ "$rolling" != "$candidate_sha" ]; then
    if [ "$rolling" != "$prior_sha" ]; then
      echo "Cannot move an unowned rolling tag during forward recovery." >&2
      return 1
    fi
    move_rolling_ref_to_candidate "$prior_sha" "$candidate_sha" || return 1
  fi

  candidate_tag=$(release_tag_for_id "$candidate_release_id") || return 1
  if is_staged_release_tag "$candidate_tag"; then
    promote_stage_release "$candidate_release_id" "$candidate_sha" || return 1
  elif [ "$candidate_tag" = "$RELEASE_TAG" ]; then
    release_is_coherent "$candidate_release_id" "$RELEASE_TAG" false || return 1
    load_transaction_marker "$candidate_release_id" "$candidate_sha" || return 1
  else
    echo "Recorded staged release changed to $candidate_tag; refusing forward recovery." >&2
    return 1
  fi
  cleanup_forward_state "$candidate_sha" "$prior_sha" "$candidate_release_id" "$prior_release_id"
}

recover_rollback() {
  local candidate_sha=$1
  local prior_sha=$2
  local candidate_release_id=$3
  local prior_release_id=$4
  local candidate_tag prior_tag rolling
  candidate_tag=$(release_tag_for_id "$candidate_release_id") || return 1
  if [ "$candidate_tag" = "$RELEASE_TAG" ]; then
    detach_promoted_release "$candidate_release_id" "$candidate_sha" || return 1
  elif ! is_staged_release_tag "$candidate_tag"; then
    echo "Recorded staged release changed to $candidate_tag; refusing rollback." >&2
    return 1
  fi

  rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  if [ "$rolling" != "$prior_sha" ]; then
    if ! move_rolling_ref "$candidate_sha" "$prior_sha" "refs/tags/$backup_tag"; then
      rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
      if [ "$rolling" = "$candidate_sha" ]; then
        echo "Rolling-tag rollback failed; attempting coherent forward repair." >&2
        recover_forward "$candidate_sha" "$prior_sha" "$candidate_release_id" "$prior_release_id"
        return
      fi
      return 1
    fi
  fi

  if [ -n "$prior_release_id" ]; then
    prior_tag=$(release_tag_for_id "$prior_release_id") || return 1
    if [ "$prior_tag" = "$backup_tag" ]; then
      if ! restore_backup_release "$prior_release_id"; then
        echo "Prior release restoration failed; attempting coherent forward repair." >&2
        move_rolling_ref_to_candidate "$prior_sha" "$candidate_sha" || return 1
        recover_forward "$candidate_sha" "$prior_sha" "$candidate_release_id" "$prior_release_id"
        return
      fi
    elif [ "$prior_tag" != "$RELEASE_TAG" ]; then
      echo "Recorded prior release changed to $prior_tag; refusing rollback." >&2
      return 1
    fi
  fi
  cleanup_rollback_state "$candidate_sha" "$prior_sha" "$candidate_release_id" "$prior_release_id"
}

mark_forward_only_phase() {
  local release_id=$1
  local candidate_sha=$2
  local forward_body
  load_transaction_marker "$release_id" "$candidate_sha" || return 1
  if [ "$marker_transaction_mode" != forward-only ]; then
    return 1
  fi
  if [ "$marker_transaction_phase" = forward ]; then
    return 0
  fi
  forward_body=${marker_body/transaction_phase=staged/transaction_phase=forward}
  patch_staged_release "$release_id" "$candidate_sha" -f body="$forward_body" || true
  release_is_staged_transaction "$release_id" "$candidate_sha" || return 1
  [ "$marker_transaction_mode" = forward-only ] && [ "$marker_transaction_phase" = forward ]
}

recover_forward_only_candidate() {
  local candidate_release_id=$1
  local candidate_sha candidate_tag expected_draft prior_sha prior_release_id prior_tag rolling stage_ref
  load_transaction_marker "$candidate_release_id" "" || return 1
  if [ "$marker_transaction_mode" != forward-only ]; then
    return 2
  fi
  candidate_sha=$marker_candidate_sha
  candidate_tag=$(release_tag_for_id "$candidate_release_id") || return 1
  if [ "$marker_staged_release_id" = pending ]; then
    if ! is_staged_release_tag "$candidate_tag"; then
      echo "Pending forward-only marker crossed the publication boundary." >&2
      return 1
    fi
    finalize_transaction_marker "$candidate_release_id" "$candidate_sha" || return 1
    load_transaction_marker "$candidate_release_id" "$candidate_sha" || return 1
  fi
  prior_sha=$marker_prior_sha
  [ "$prior_sha" = none ] && prior_sha=
  prior_release_id=$marker_prior_release_id
  [ "$prior_release_id" = none ] && prior_release_id=
  rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  stage_ref=$(remote_sha "refs/tags/$stage_tag") || return 1
  if [ -n "$stage_ref" ] && [ "$stage_ref" != "$candidate_sha" ]; then
    echo "Staging tag changed before forward-only recovery." >&2
    return 1
  fi

  if [ "$marker_transaction_phase" = staged ]; then
    if ! is_staged_release_tag "$candidate_tag" || [ "$rolling" != "$prior_sha" ]; then
      echo "Staged forward-only transaction crossed an unrecorded boundary." >&2
      return 1
    fi
    if [ -n "$prior_release_id" ]; then
      prior_tag=$(release_tag_for_id "$prior_release_id") || return 1
      if [ "$prior_tag" != "$RELEASE_TAG" ]; then
        echo "Recorded prior release changed before staged cleanup." >&2
        return 1
      fi
      release_is_prior_coherent "$prior_release_id" "$RELEASE_TAG" false || return 1
    fi
    delete_stage_ref_via_api "$candidate_sha" || return 1
    delete_staged_release "$candidate_release_id" "$candidate_sha"
    return
  fi

  if ! is_staged_release_tag "$candidate_tag" && [ "$candidate_tag" != "$RELEASE_TAG" ]; then
    echo "Forward-only release changed to unowned tag $candidate_tag." >&2
    return 1
  fi
  expected_draft=false
  if is_staged_release_tag "$candidate_tag"; then
    expected_draft=true
    release_is_staged_transaction "$candidate_release_id" "$candidate_sha" || return 1
  else
    release_is_coherent "$candidate_release_id" "$candidate_tag" "$expected_draft" || return 1
  fi
  if [ "$rolling" != "$prior_sha" ] && [ "$rolling" != "$candidate_sha" ]; then
    echo "Rolling tag is outside the forward-only transaction states." >&2
    return 1
  fi

  if [ -n "$prior_release_id" ]; then
    prior_tag=$(release_tag_for_id "$prior_release_id") || return 1
    if [ "$prior_tag" = "$RELEASE_TAG" ]; then
      release_is_prior_coherent "$prior_release_id" "$RELEASE_TAG" false || return 1
      delete_release_from_tag "$prior_release_id" "$RELEASE_TAG" || return 1
    elif [ -n "$prior_tag" ]; then
      echo "Recorded prior release changed to unowned tag $prior_tag." >&2
      return 1
    fi
  fi
  if [ "$rolling" != "$candidate_sha" ]; then
    move_rolling_ref_via_api "$prior_sha" "$candidate_sha" || return 1
  fi
  candidate_tag=$(release_tag_for_id "$candidate_release_id") || return 1
  if is_staged_release_tag "$candidate_tag"; then
    promote_stage_release "$candidate_release_id" "$candidate_sha" || return 1
  else
    release_is_coherent "$candidate_release_id" "$RELEASE_TAG" false || return 1
  fi
  delete_stage_ref_via_api "$candidate_sha"
}

recover_unreleased_stage_ref() {
  local stage_sha=$1
  local backup_sha=$2
  local stable_release_id=$3
  local backup_release_id=$4
  local rolling
  if [ "$stage_sha" != "$GITHUB_SHA" ]; then
    echo "Unreleased staging tag belongs to another candidate: $stage_sha" >&2
    return 1
  fi
  if [ -n "$backup_release_id" ]; then
    echo "Unreleased staging tag is accompanied by an unowned backup release." >&2
    return 1
  fi
  rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  if [ -n "$stable_release_id" ]; then
    if [ -z "$rolling" ]; then
      echo "Prior release exists without its rolling tag." >&2
      return 1
    fi
    release_is_prior_coherent "$stable_release_id" "$RELEASE_TAG" false || return 1
  elif [ -n "$rolling" ]; then
    echo "Rolling tag exists without a release while recovering an unreleased stage." >&2
    return 1
  fi
  if [ -n "$backup_sha" ] && [ "$backup_sha" != "$rolling" ]; then
    echo "Unreleased staging tag has an unrelated backup ref." >&2
    return 1
  fi
  delete_stage_ref_via_api "$stage_sha" || return 1
  if [ -n "$backup_sha" ]; then
    delete_backup_ref "$backup_sha" || return 1
  fi
}

recover_existing_transaction() {
  local forward_status
  local stage_sha backup_sha rolling
  local stage_release_id backup_release_id stable_release_id candidate_release_id candidate_tag
  local prior_sha prior_release_id prior_tag stable_marked=0 stage_ref_missing=0
  stage_sha=$(remote_sha "refs/tags/$stage_tag") || return 1
  backup_sha=$(remote_sha "refs/tags/$backup_tag") || return 1
  stage_release_id=$(release_id_for_staged_transaction) || return 1
  backup_release_id=$(release_id_for_tag "$backup_tag") || return 1
  stable_release_id=$(release_id_for_tag "$RELEASE_TAG") || return 1

  if [ -n "$stage_sha" ] && [ -z "$stage_release_id" ]; then
    if [ -n "$stable_release_id" ]; then
      if release_has_marker_start "$stable_release_id"; then
        load_transaction_marker "$stable_release_id" "" || return 1
        if [ "$marker_candidate_sha" = "$stage_sha" ]; then
          stable_marked=1
        else
          stable_marked=0
        fi
      else
        case $? in
          1) stable_marked=0 ;;
          *) return 1 ;;
        esac
      fi
    fi
    if [ "$stable_marked" -eq 0 ]; then
      recover_unreleased_stage_ref "$stage_sha" "$backup_sha" "$stable_release_id" "$backup_release_id"
      return
    fi
  fi

  if [ -n "$stage_release_id" ]; then
    if recover_forward_only_candidate "$stage_release_id"; then
      return 0
    else
      forward_status=$?
      [ "$forward_status" -eq 2 ] || return "$forward_status"
    fi
  elif [ -n "$stage_sha" ] && [ -n "$stable_release_id" ]; then
    if recover_forward_only_candidate "$stable_release_id"; then
      return 0
    else
      forward_status=$?
      [ "$forward_status" -eq 2 ] || return "$forward_status"
    fi
  fi

  if [ -z "$stage_sha" ] && [ -z "$stage_release_id" ]; then
    if [ -z "$backup_sha" ] && [ -z "$backup_release_id" ]; then
      return 0
    fi
    rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
    if [ -n "$backup_sha" ] && [ -z "$backup_release_id" ] && [ "$rolling" = "$backup_sha" ]; then
      if [ -n "$stable_release_id" ]; then
        release_is_prior_coherent "$stable_release_id" "$RELEASE_TAG" false || return 1
      fi
      delete_backup_ref "$backup_sha"
      return
    fi
    if [ -n "$stable_release_id" ]; then
      if release_has_marker_start "$stable_release_id"; then
        stable_marked=1
      else
        case $? in
          1) stable_marked=0 ;;
          *) return 1 ;;
        esac
      fi
    fi
    if [ "$stable_marked" -eq 1 ]; then
      load_transaction_marker "$stable_release_id" "" || return 1
      candidate_release_id=$stable_release_id
      prior_sha=$marker_prior_sha
      [ "$prior_sha" = none ] && prior_sha=
      prior_release_id=$marker_prior_release_id
      [ "$prior_release_id" = none ] && prior_release_id=
      recover_forward "$marker_candidate_sha" "$prior_sha" "$candidate_release_id" "$prior_release_id"
      return
    fi
    if [ -n "$backup_release_id" ]; then
      echo "Orphaned backup release lacks staged ownership evidence." >&2
      return 1
    fi
    if [ -z "$backup_sha" ] || [ "$rolling" != "$backup_sha" ]; then
      echo "Orphaned backup state is ambiguous; refusing a new transaction." >&2
      return 1
    fi
    if [ -n "$stable_release_id" ]; then
      release_is_prior_coherent "$stable_release_id" "$RELEASE_TAG" false || return 1
    fi
    delete_backup_ref "$backup_sha"
    return
  fi

  if [ -z "$stage_sha" ] && [ -n "$stage_release_id" ]; then
    stage_ref_missing=1
    load_transaction_marker "$stage_release_id" "" || return 1
    if [ "$marker_staged_release_id" = pending ]; then
      finalize_transaction_marker "$stage_release_id" "$marker_candidate_sha" || return 1
    elif [ "$marker_staged_release_id" != "$stage_release_id" ]; then
      echo "Staged marker lost its staging ref with an invalid release identity." >&2
      return 1
    fi
    stage_sha=$marker_candidate_sha
  fi

  if [ -z "$stage_sha" ]; then
    echo "Transaction state has no staging ref; refusing recovery." >&2
    return 1
  fi

  if [ -n "$stage_release_id" ]; then
    candidate_release_id=$stage_release_id
    load_transaction_marker "$candidate_release_id" "$stage_sha" || return 1
  elif [ -n "$stable_release_id" ]; then
    if release_has_marker_start "$stable_release_id"; then
      :
    else
      marker_status=$?
      if [ "$marker_status" -eq 1 ]; then
        echo "Staging ref exists without its marked release." >&2
      fi
      return 1
    fi
    candidate_release_id=$stable_release_id
    load_transaction_marker "$candidate_release_id" "$stage_sha" || return 1
  else
    echo "Staging ref exists without a recoverable staged release." >&2
    return 1
  fi

  candidate_tag=$(release_tag_for_id "$candidate_release_id") || return 1
  if [ "$marker_staged_release_id" = pending ]; then
    if ! is_staged_release_tag "$candidate_tag"; then
      echo "Pending transaction marker crossed a public boundary; refusing recovery." >&2
      return 1
    fi
    finalize_transaction_marker "$candidate_release_id" "$stage_sha" || return 1
  fi
  if [ "$marker_staged_release_id" != "$candidate_release_id" ]; then
    return 1
  fi
  if is_staged_release_tag "$candidate_tag"; then
    release_is_owned_staged_transaction "$candidate_release_id" "$stage_sha" || return 1
  elif [ "$candidate_tag" = "$RELEASE_TAG" ]; then
    release_is_coherent "$candidate_release_id" "$RELEASE_TAG" false || return 1
  else
    echo "Marked release changed to unowned tag $candidate_tag." >&2
    return 1
  fi

  prior_sha=$marker_prior_sha
  [ "$prior_sha" = none ] && prior_sha=
  prior_release_id=$marker_prior_release_id
  [ "$prior_release_id" = none ] && prior_release_id=
  if [ -n "$prior_release_id" ]; then
    prior_tag=$(release_tag_for_id "$prior_release_id") || return 1
    if [ -n "$prior_tag" ] && [ "$prior_tag" != "$RELEASE_TAG" ] && [ "$prior_tag" != "$backup_tag" ]; then
      echo "Recorded prior release changed to unowned tag $prior_tag." >&2
      return 1
    fi
  elif [ -n "$backup_release_id" ]; then
    echo "Transaction marker records no prior release, but a backup release exists." >&2
    return 1
  fi

  rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
  if [ "$rolling" != "$stage_sha" ] && [ "$rolling" != "$prior_sha" ]; then
    echo "Rolling tag is outside the recorded transaction states." >&2
    return 1
  fi
  if [ "$backup_sha" != "$prior_sha" ]; then
    if [ "$stage_ref_missing" -eq 1 ] && [ -z "$backup_sha" ] &&
      [ "$rolling" = "$prior_sha" ] &&
      { [ -z "$prior_release_id" ] || [ "${prior_tag:-}" = "$RELEASE_TAG" ]; }; then
      :
    elif [ "$rolling" != "$stage_sha" ] || [ -n "${prior_tag:-}" ]; then
      echo "Backup ref does not match the recorded predecessor." >&2
      return 1
    fi
  fi
  if [ -n "$prior_release_id" ] && [ -z "${prior_tag:-}" ] && { [ "$rolling" != "$stage_sha" ] || [ "$candidate_tag" != "$RELEASE_TAG" ]; }; then
    echo "Recorded prior release disappeared before a forward terminal state was proven." >&2
    return 1
  fi

  if [ "$rolling" = "$stage_sha" ] || [ "$candidate_tag" = "$RELEASE_TAG" ]; then
    recover_forward "$stage_sha" "$prior_sha" "$candidate_release_id" "$prior_release_id"
  elif is_staged_release_tag "$candidate_tag" && [ "$rolling" = "$prior_sha" ]; then
    recover_rollback "$stage_sha" "$prior_sha" "$candidate_release_id" "$prior_release_id"
  else
    echo "Transaction state cannot be classified safely; retaining all artifacts." >&2
    return 1
  fi
}

on_exit() {
  local status=$?
  trap - EXIT
  if [ "$status" -ne 0 ] && [ "$committed" -eq 0 ] && [ "$transaction_started" -eq 1 ]; then
    set +e
    echo "Release transaction failed; reconciling durable channel state." >&2
    if ! recover_existing_transaction; then
      echo "Release reconciliation was incomplete; retained transaction refs and releases for retry." >&2
    fi
  fi
  exit "$status"
}
trap on_exit EXIT

shopt -s nullglob
recover_existing_transaction
validate_local_assets
generate_release_manifest

source_sha=$(remote_sha "refs/heads/$GITHUB_REF_NAME")
if [ "$source_sha" != "$GITHUB_SHA" ]; then
  echo "Refusing stale $RELEASE_CHANNEL publication: $GITHUB_REF_NAME is at $source_sha, not $GITHUB_SHA." >&2
  exit 1
fi

old_tag_sha=$(remote_sha "refs/tags/$RELEASE_TAG")
old_release_id=$(release_id_for_tag "$RELEASE_TAG")
if [ -n "$old_release_id" ]; then
  if [ -z "$old_tag_sha" ]; then
    echo "Rolling release exists without its Git tag: $RELEASE_TAG" >&2
    exit 1
  fi
  if ! release_is_prior_coherent "$old_release_id" "$RELEASE_TAG" false; then
    echo "Existing rolling release is incomplete or not published; refusing replacement." >&2
    exit 1
  fi
fi
if [ "$old_tag_sha" = "$GITHUB_SHA" ] && [ -n "$old_release_id" ]; then
  echo "Release $RELEASE_TAG is already coherent at $GITHUB_SHA."
  exit 0
fi
if [ -n "$old_tag_sha" ] && [ "$old_tag_sha" != "$GITHUB_SHA" ]; then
  "$git_bin" fetch --no-tags "$origin" "refs/tags/$RELEASE_TAG" >/dev/null
  if "$git_bin" merge-base --is-ancestor "$GITHUB_SHA" "$old_tag_sha"; then
    echo "Refusing to move $RELEASE_TAG backwards from $old_tag_sha to stale commit $GITHUB_SHA." >&2
    exit 1
  fi
fi

upload_paths=()
for name in "${expected_asset_names[@]}"; do
  upload_paths+=("$dist_dir/$name")
done
prior_sha_marker=${old_tag_sha:-none}
prior_release_id_marker=${old_release_id:-none}

workflow_change_transaction=0
if [ -n "$old_tag_sha" ] && [ "$old_tag_sha" != "$GITHUB_SHA" ]; then
  if "$git_bin" diff --quiet "$old_tag_sha" "$GITHUB_SHA" -- .github/workflows; then
    :
  else
    diff_status=$?
    if [ "$diff_status" -eq 1 ]; then
      workflow_change_transaction=1
    else
      echo "Could not compare workflow revisions before publication." >&2
      exit 1
    fi
  fi
fi

if [ "$workflow_change_transaction" -eq 1 ]; then
  transaction_started=1
  create_stage_ref_via_api "$GITHUB_SHA"
  transaction_body=$(build_transaction_body "$GITHUB_SHA" "$prior_sha_marker" pending "$prior_release_id_marker" forward-only staged)
  create_args=(
    release create "$stage_tag"
    "${upload_paths[@]}"
    --target "$GITHUB_SHA"
    --verify-tag
    --draft
    --title "$RELEASE_TITLE"
    --notes "$transaction_body"
  )
  if [ "$RELEASE_PRERELEASE" = true ]; then
    create_args+=(--prerelease)
  fi
  "$gh_bin" "${create_args[@]}"
  stage_sha=$(remote_sha "refs/tags/$stage_tag")
  if [ "$stage_sha" != "$GITHUB_SHA" ]; then
    echo "GitHub did not create the forward-only staging tag at the candidate SHA." >&2
    exit 1
  fi
  if ! stage_release_id=$(wait_for_staged_transaction "$GITHUB_SHA"); then
    echo "Forward-only staged release could not be resolved with the exact expected assets." >&2
    exit 1
  fi
  finalize_transaction_marker "$stage_release_id" "$GITHUB_SHA" || {
    echo "Forward-only release ownership marker could not be finalized." >&2
    exit 1
  }

  source_sha=$(remote_sha "refs/heads/$GITHUB_REF_NAME")
  if [ "$source_sha" != "$GITHUB_SHA" ]; then
    echo "Refusing stale $RELEASE_CHANNEL promotion: $GITHUB_REF_NAME is at $source_sha, not $GITHUB_SHA." >&2
    exit 1
  fi
  mark_forward_only_phase "$stage_release_id" "$GITHUB_SHA" || {
    echo "Forward-only publication boundary could not be recorded." >&2
    exit 1
  }
  if [ -n "$old_release_id" ]; then
    release_is_prior_coherent "$old_release_id" "$RELEASE_TAG" false || exit 1
    delete_release_from_tag "$old_release_id" "$RELEASE_TAG" || exit 1
  fi
  move_rolling_ref_via_api "$old_tag_sha" "$GITHUB_SHA" || exit 1
  promote_stage_release "$stage_release_id" "$GITHUB_SHA" || {
    echo "Forward-only staged release did not reach a coherent published state." >&2
    exit 1
  }

  source_sha=$(remote_sha "refs/heads/$GITHUB_REF_NAME")
  published_sha=$(remote_sha "refs/tags/$RELEASE_TAG")
  if [ "$published_sha" != "$GITHUB_SHA" ] || ! release_is_coherent "$stage_release_id" "$RELEASE_TAG" false; then
    echo "Forward-only release state changed during final verification." >&2
    exit 1
  fi
  load_transaction_marker "$stage_release_id" "$GITHUB_SHA" || {
    echo "Forward-only release ownership marker changed during final verification." >&2
    exit 1
  }
  committed=1
  delete_stage_ref_via_api "$GITHUB_SHA" ||
    echo "Warning: retained the forward-only staging ref for cleanup by the next run." >&2
  if [ "$source_sha" != "$GITHUB_SHA" ]; then
    echo "Source branch advanced after the forward-only publication boundary; the candidate remains coherent." >&2
    exit 1
  fi
  echo "Published coherent $RELEASE_CHANNEL release $RELEASE_TAG at $GITHUB_SHA."
  exit 0
fi

transaction_started=1
if [ -n "$old_tag_sha" ]; then
  "$git_bin" push --force-with-lease="refs/tags/$backup_tag:" "$origin" "$old_tag_sha:refs/tags/$backup_tag" >/dev/null
  backup_sha=$(remote_sha "refs/tags/$backup_tag")
  if [ "$backup_sha" != "$old_tag_sha" ]; then
    echo "Backup tag did not reach the recorded prior SHA." >&2
    exit 1
  fi
fi

create_stage_ref_via_api "$GITHUB_SHA"
transaction_body=$(build_transaction_body "$GITHUB_SHA" "$prior_sha_marker" pending "$prior_release_id_marker")
create_args=(
  release create "$stage_tag"
  "${upload_paths[@]}"
  --target "$GITHUB_SHA"
  --verify-tag
  --draft
  --title "$RELEASE_TITLE"
  --notes "$transaction_body"
)
if [ "$RELEASE_PRERELEASE" = true ]; then
  create_args+=(--prerelease)
fi
"$gh_bin" "${create_args[@]}"
stage_sha=$(remote_sha "refs/tags/$stage_tag")
if [ "$stage_sha" != "$GITHUB_SHA" ]; then
  echo "GitHub did not create the staging tag at the candidate SHA." >&2
  exit 1
fi
if ! stage_release_id=$(wait_for_staged_transaction "$GITHUB_SHA"); then
  echo "Staged draft release could not be resolved with the exact expected assets." >&2
  exit 1
fi
finalize_transaction_marker "$stage_release_id" "$GITHUB_SHA" || {
  echo "Staged release ownership marker could not be finalized." >&2
  exit 1
}

source_sha=$(remote_sha "refs/heads/$GITHUB_REF_NAME")
if [ "$source_sha" != "$GITHUB_SHA" ]; then
  echo "Refusing stale $RELEASE_CHANNEL promotion: $GITHUB_REF_NAME is at $source_sha, not $GITHUB_SHA." >&2
  exit 1
fi
if [ -n "$old_release_id" ]; then
  read_exact_release_tag "$old_release_id" "$RELEASE_TAG" "prior stable" || exit 1
  backup_stable_release "$old_release_id" || exit 1
fi

"$git_bin" push --atomic --force-with-lease="refs/heads/$GITHUB_REF_NAME:$GITHUB_SHA" --force-with-lease="refs/tags/$RELEASE_TAG:$old_tag_sha" "$origin" "$GITHUB_SHA:refs/heads/$GITHUB_REF_NAME" "$GITHUB_SHA:refs/tags/$RELEASE_TAG" >/dev/null

source_sha=$(remote_sha "refs/heads/$GITHUB_REF_NAME")
published_sha=$(remote_sha "refs/tags/$RELEASE_TAG")
if [ "$source_sha" != "$GITHUB_SHA" ] || [ "$published_sha" != "$GITHUB_SHA" ]; then
  echo "Release refs changed before staged promotion completed." >&2
  exit 1
fi

promote_stage_release "$stage_release_id" "$GITHUB_SHA" || {
  echo "Staged release promotion did not reach a coherent published state." >&2
  exit 1
}

"$git_bin" push --atomic --force-with-lease="refs/heads/$GITHUB_REF_NAME:$GITHUB_SHA" --force-with-lease="refs/tags/$RELEASE_TAG:$GITHUB_SHA" "$origin" "$GITHUB_SHA:refs/heads/$GITHUB_REF_NAME" "$GITHUB_SHA:refs/tags/$RELEASE_TAG" >/dev/null
source_sha=$(remote_sha "refs/heads/$GITHUB_REF_NAME")
published_sha=$(remote_sha "refs/tags/$RELEASE_TAG")
if [ "$source_sha" != "$GITHUB_SHA" ] || [ "$published_sha" != "$GITHUB_SHA" ] || ! release_is_coherent "$stage_release_id" "$RELEASE_TAG" false; then
  echo "Release state changed during final promotion verification." >&2
  exit 1
fi
load_transaction_marker "$stage_release_id" "$GITHUB_SHA" || {
  echo "Release ownership marker changed during final verification." >&2
  exit 1
}
committed=1

cleanup_forward_state "$GITHUB_SHA" "$old_tag_sha" "$stage_release_id" "$old_release_id" ||
  echo "Warning: retained durable transaction state for cleanup by the next run." >&2

echo "Published coherent $RELEASE_CHANNEL release $RELEASE_TAG at $GITHUB_SHA."

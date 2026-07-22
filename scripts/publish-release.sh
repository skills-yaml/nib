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
if [[ ! "$GITHUB_SHA" =~ ^[0-9A-Fa-f]{40,64}$ ]]; then
  echo "Invalid GITHUB_SHA value." >&2
  exit 2
fi

git_bin=${NIB_RELEASE_GIT_BIN:-git}
gh_bin=${NIB_RELEASE_GH_BIN:-gh}
origin=${NIB_RELEASE_ORIGIN:-origin}
dist_dir=${NIB_RELEASE_DIST_DIR:-dist}
stage_tag="nib-release-stage-$RELEASE_CHANNEL"
backup_tag="nib-release-backup-$RELEASE_CHANNEL"
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
expected_asset_names=("${archive_names[@]}" "${checksum_names[@]}")
expected_asset_listing=$(printf '%s\n' "${expected_asset_names[@]}" | LC_ALL=C sort)
marker_start='<!-- nib-release-transaction-v1'

build_transaction_body() {
  local candidate_sha=$1
  local prior_sha=${2:-none}
  local staged_release_id=${3:-pending}
  local prior_release_id=${4:-none}
  cat <<EOF
$notes

$marker_start
channel=$RELEASE_CHANNEL
candidate_sha=$candidate_sha
prior_sha=$prior_sha
staged_release_id=$staged_release_id
prior_release_id=$prior_release_id
prior_release_draft=false
-->
EOF
}

marker_value() {
  local body=$1
  local key=$2
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
  if [ -z "$first" ] || [ -n "$remainder" ]; then
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
}

remote_sha() {
  local ref=$1
  "$git_bin" ls-remote "$origin" "$ref" |
    awk 'NR == 1 { value = $1 } END { print value }'
}

release_id_for_tag() {
  local tag=$1
  local matches first remainder
  matches=$("$gh_bin" api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100" --jq ".[] | select(.tag_name == \"$tag\") | .id")
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

release_tag_for_id() {
  local release_id=$1
  local matches first remainder
  matches=$("$gh_bin" api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100" --jq ".[] | select((.id | tostring) == \"$release_id\") | .tag_name")
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
  local actual incomplete
  actual=$(release_asset_names "$release_id") || return 1
  if [ "$actual" != "$expected_asset_listing" ]; then
    echo "Release $release_id does not contain the exact expected asset names." >&2
    return 1
  fi
  incomplete=$(release_incomplete_asset_names "$release_id") || return 1
  if [ -n "$incomplete" ]; then
    echo "Release $release_id contains incomplete or empty assets: $incomplete" >&2
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
  load_transaction_marker "$release_id" "$candidate_sha" || return 1
  if [ "$marker_staged_release_id" = "$release_id" ]; then
    return 0
  fi
  if [ "$marker_staged_release_id" != pending ]; then
    return 1
  fi
  finalized_body=${marker_body/staged_release_id=pending/staged_release_id=$release_id}
  patch_release_from_tag "$release_id" "$stage_tag" -f body="$finalized_body" || true
  load_transaction_marker "$release_id" "$candidate_sha" || return 1
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

promote_stage_release() {
  local release_id=$1
  local candidate_sha=$2
  release_has_expected_assets "$release_id" || return 1
  load_transaction_marker "$release_id" "$candidate_sha" || return 1
  if [ "$marker_staged_release_id" != "$release_id" ]; then
    return 1
  fi
  patch_release_from_tag "$release_id" "$stage_tag" -f tag_name="$RELEASE_TAG" -f name="$RELEASE_TITLE" -f body="$marker_body" -F draft=false -F prerelease="$RELEASE_PRERELEASE" -f target_commitish="$candidate_sha" || true
  release_is_coherent "$release_id" "$RELEASE_TAG" false || return 1
  load_transaction_marker "$release_id" "$candidate_sha"
}

detach_promoted_release() {
  local release_id=$1
  local candidate_sha=$2
  load_transaction_marker "$release_id" "$candidate_sha" || return 1
  patch_release_from_tag "$release_id" "$RELEASE_TAG" -f tag_name="$stage_tag" -F draft=true || true
  release_is_coherent "$release_id" "$stage_tag" true || return 1
  load_transaction_marker "$release_id" "$candidate_sha"
}

backup_stable_release() {
  local release_id=$1
  release_is_coherent "$release_id" "$RELEASE_TAG" false || return 1
  patch_release_from_tag "$release_id" "$RELEASE_TAG" -f tag_name="$backup_tag" -F draft=true || true
  release_is_coherent "$release_id" "$backup_tag" true
}

restore_backup_release() {
  local release_id=$1
  release_is_coherent "$release_id" "$backup_tag" true || return 1
  patch_release_from_tag "$release_id" "$backup_tag" -f tag_name="$RELEASE_TAG" -F draft=false || true
  release_is_coherent "$release_id" "$RELEASE_TAG" false
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
      release_is_coherent "$prior_release_id" "$backup_tag" true || return 1
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
    release_is_coherent "$prior_release_id" "$RELEASE_TAG" false || return 1
  elif [ -n "$stable_release_id" ]; then
    echo "Rollback unexpectedly left a stable release." >&2
    return 1
  fi
  release_has_expected_state "$candidate_release_id" "$stage_tag" true || return 1
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
  delete_release_from_tag "$candidate_release_id" "$stage_tag" || return 1
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
    move_rolling_ref "$prior_sha" "$candidate_sha" "refs/tags/$stage_tag" || return 1
  fi

  candidate_tag=$(release_tag_for_id "$candidate_release_id") || return 1
  if [ "$candidate_tag" = "$stage_tag" ]; then
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
  elif [ "$candidate_tag" != "$stage_tag" ]; then
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
        move_rolling_ref "$prior_sha" "$candidate_sha" "refs/tags/$stage_tag" || return 1
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

recover_existing_transaction() {
  local stage_sha backup_sha rolling
  local stage_release_id backup_release_id stable_release_id candidate_release_id candidate_tag
  local prior_sha prior_release_id prior_tag stable_marked=0
  stage_sha=$(remote_sha "refs/tags/$stage_tag") || return 1
  backup_sha=$(remote_sha "refs/tags/$backup_tag") || return 1
  stage_release_id=$(release_id_for_tag "$stage_tag") || return 1
  backup_release_id=$(release_id_for_tag "$backup_tag") || return 1
  stable_release_id=$(release_id_for_tag "$RELEASE_TAG") || return 1

  if [ -z "$stage_sha" ] && [ -z "$stage_release_id" ]; then
    if [ -z "$backup_sha" ] && [ -z "$backup_release_id" ]; then
      return 0
    fi
    rolling=$(remote_sha "refs/tags/$RELEASE_TAG") || return 1
    if [ -n "$backup_sha" ] && [ -z "$backup_release_id" ] && [ "$rolling" = "$backup_sha" ]; then
      if [ -n "$stable_release_id" ]; then
        release_is_coherent "$stable_release_id" "$RELEASE_TAG" false || return 1
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
      release_is_coherent "$stable_release_id" "$RELEASE_TAG" false || return 1
    fi
    delete_backup_ref "$backup_sha"
    return
  fi

  if [ -z "$stage_sha" ] && [ -n "$stage_release_id" ]; then
    load_transaction_marker "$stage_release_id" "" || return 1
    if [ "$marker_staged_release_id" != "$stage_release_id" ]; then
      echo "Unfinalized staged marker lost its staging ref; refusing cleanup." >&2
      return 1
    fi
    prior_sha=$marker_prior_sha
    [ "$prior_sha" = none ] && prior_sha=
    prior_release_id=$marker_prior_release_id
    [ "$prior_release_id" = none ] && prior_release_id=
    cleanup_rollback_state "$marker_candidate_sha" "$prior_sha" "$stage_release_id" "$prior_release_id"
    return
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
    if [ "$candidate_tag" != "$stage_tag" ]; then
      echo "Pending transaction marker crossed a public boundary; refusing recovery." >&2
      return 1
    fi
    finalize_transaction_marker "$candidate_release_id" "$stage_sha" || return 1
  fi
  if [ "$marker_staged_release_id" != "$candidate_release_id" ]; then
    return 1
  fi
  if [ "$candidate_tag" = "$stage_tag" ]; then
    release_has_expected_state "$candidate_release_id" "$stage_tag" true || return 1
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
    if [ "$rolling" != "$stage_sha" ] || [ -n "${prior_tag:-}" ]; then
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
  elif [ "$candidate_tag" = "$stage_tag" ] && [ "$rolling" = "$prior_sha" ]; then
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
  if ! release_is_coherent "$old_release_id" "$RELEASE_TAG" false; then
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

transaction_started=1
if [ -n "$old_tag_sha" ]; then
  "$git_bin" push --force-with-lease="refs/tags/$backup_tag:" "$origin" "$old_tag_sha:refs/tags/$backup_tag" >/dev/null
  backup_sha=$(remote_sha "refs/tags/$backup_tag")
  if [ "$backup_sha" != "$old_tag_sha" ]; then
    echo "Backup tag did not reach the recorded prior SHA." >&2
    exit 1
  fi
fi

upload_paths=()
for name in "${expected_asset_names[@]}"; do
  upload_paths+=("$dist_dir/$name")
done
prior_sha_marker=${old_tag_sha:-none}
prior_release_id_marker=${old_release_id:-none}
transaction_body=$(build_transaction_body "$GITHUB_SHA" "$prior_sha_marker" pending "$prior_release_id_marker")
create_args=(
  release create "$stage_tag"
  "${upload_paths[@]}"
  --target "$GITHUB_SHA"
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
stage_release_id=$(release_id_for_tag "$stage_tag")
if [ -z "$stage_release_id" ] || ! release_is_coherent "$stage_release_id" "$stage_tag" true; then
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

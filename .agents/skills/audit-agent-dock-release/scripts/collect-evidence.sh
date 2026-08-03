#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <previous-tag-or-commit> <target-revision>" >&2
  exit 2
fi

previous_revision=$1
target_revision=$2

repo_root=$(jj root)
cd "$repo_root"

resolve_commit() {
  local revision=$1
  local resolved
  resolved=$(jj log -r "$revision" --no-graph -T 'commit_id ++ "\n"')
  if [[ ! $resolved =~ ^[0-9a-f]{40}$ ]]; then
    echo "revision must resolve to exactly one commit: $revision" >&2
    exit 2
  fi
  printf '%s' "$resolved"
}

version_at() {
  local revision=$1
  jj file show -r "$revision" Cargo.toml |
    awk -F '"' '/^version = "/ { print $2; exit }'
}

collect_tests() {
  local revision=$1
  jj file list -r "$revision" |
    while IFS= read -r source_file; do
      case "$source_file" in
        src/*.rs|examples/*.rs|tests/*.rs|benches/*.rs)
          jj file show -r "$revision" "$source_file" |
            awk -v source="$source_file" '
              /^[[:space:]]*#\[(tokio::)?test\]/ { pending = 1; next }
              pending && /fn[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\(/ {
                line = $0
                sub(/^.*fn[[:space:]]+/, "", line)
                sub(/[[:space:]]*\(.*/, "", line)
                print source "::" line
                pending = 0
              }
            '
          ;;
      esac
    done |
    sort -u
}

previous_commit=$(resolve_commit "$previous_revision")
target_commit=$(resolve_commit "$target_revision")
target_version=$(version_at "$target_commit")
release_document="docs/releases/${target_version}.md"

audit_tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/agent-dock-release-audit.XXXXXX")
previous_tests="$audit_tmp_dir/previous-tests"
target_tests="$audit_tmp_dir/target-tests"

cleanup() {
  case "$audit_tmp_dir" in
    */agent-dock-release-audit.*)
      rm -f "$previous_tests" "$target_tests"
      rmdir "$audit_tmp_dir" 2>/dev/null || true
      ;;
  esac
}
trap cleanup EXIT

collect_tests "$previous_commit" >"$previous_tests"
collect_tests "$target_commit" >"$target_tests"

echo "Previous commit: $previous_commit"
echo "Target commit:   $target_commit"
echo "Target version:  $target_version"
echo "Release document: $release_document"
echo

echo "Changed files"
jj diff --from "$previous_commit" --to "$target_commit" --stat
echo

echo "Document changes"
jj diff --from "$previous_commit" --to "$target_commit" -- DESIGN.md "$release_document"
echo

echo "Tests added"
comm -13 "$previous_tests" "$target_tests"
echo

echo "Tests removed"
comm -23 "$previous_tests" "$target_tests"
echo

echo "Validation commands"
echo "cargo fmt --check"
echo "cargo test --all-targets"
echo "cargo clippy --all-targets --all-features -- -D warnings"

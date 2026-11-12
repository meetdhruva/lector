#!/usr/bin/env bash
# Materializes a real pull request as a local case of the risk-ordering evaluation.
#
#   scripts/risk-eval-import.sh OWNER/REPO PR_NUMBER [--risky PATH]...
#
# Uses the `gh` CLI (REST only: `gh api repos/...`, so it works wherever GraphQL is blocked) to fetch
# the files of the PR and their content at the base and head commits, and writes
# crates/lector-app/tests/risk_eval/local/OWNER__REPO-PR.json in the same format as the committed
# cases. That directory is git-ignored: imported PRs carry their projects' licenses and are never
# committed. Pass `--risky PATH` (repeatable) with the file(s) a reviewer must look at first; a case
# without it is only listed by `cargo run -p lector-app --example risk_eval -- --local` (top-3
# per case), not scored. See crates/lector-app/tests/risk_eval/README.md.
#
# Environment: RISK_EVAL_LOCAL_DIR (output directory), RISK_EVAL_MAX_FILES (default 80, the first
# files of the PR), RISK_EVAL_MAX_BYTES (default 200000, larger files are skipped).
set -euo pipefail

usage() {
  sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-2}"
}

[ $# -ge 2 ] || usage
case "$1" in -h | --help) usage 0 ;; esac
repo="$1"
pr="$2"
shift 2
risky=()
while [ $# -gt 0 ]; do
  case "$1" in
    --risky)
      [ $# -ge 2 ] || usage
      risky+=("$2")
      shift 2
      ;;
    -h | --help) usage 0 ;;
    *) usage ;;
  esac
done
case "$repo" in */*) ;; *) echo "repository must be OWNER/REPO" >&2; exit 2 ;; esac
[[ "$pr" =~ ^[0-9]+$ ]] || { echo "PR number must be numeric" >&2; exit 2; }
for tool in gh jq; do
  command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done

root="$(cd "$(dirname "$0")/.." && pwd)"
out_dir="${RISK_EVAL_LOCAL_DIR:-$root/crates/lector-app/tests/risk_eval/local}"
max_files="${RISK_EVAL_MAX_FILES:-80}"
max_bytes="${RISK_EVAL_MAX_BYTES:-200000}"
mkdir -p "$out_dir"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

encode_path() { jq -rn --arg p "$1" '$p | split("/") | map(@uri) | join("/")'; }

# fetch PATH REF OUTFILE: content of a file at a commit; fails when absent, binary, not UTF-8 or too large.
fetch() {
  local path="$1" ref="$2" out="$3"
  gh api -H "Accept: application/vnd.github.raw+json" \
    "repos/$repo/contents/$(encode_path "$path")?ref=$ref" >"$out" 2>/dev/null || return 1
  [ "$(wc -c <"$out")" -le "$max_bytes" ] || return 1
  # A NUL byte means binary content.
  tr -d '\0' <"$out" | cmp -s - "$out" || return 1
  iconv -f utf-8 -t utf-8 "$out" >/dev/null 2>&1 || return 1
}

meta="$(gh api "repos/$repo/pulls/$pr")"
base_sha="$(jq -r '.base.sha' <<<"$meta")"
head_sha="$(jq -r '.head.sha' <<<"$meta")"
title="$(jq -r '.title' <<<"$meta")"
files_json="$(gh api --paginate "repos/$repo/pulls/$pr/files?per_page=100" | jq -s 'add // []')"

: >"$tmp/files.jsonl"
n=0
total="$(jq 'length' <<<"$files_json")"
while IFS= read -r row; do
  [ "$n" -lt "$max_files" ] || break
  path="$(jq -r '.filename' <<<"$row")"
  status="$(jq -r '.status' <<<"$row")"
  previous="$(jq -r '.previous_filename // .filename' <<<"$row")"
  base_file="$tmp/base" head_file="$tmp/head"
  rm -f "$base_file" "$head_file"
  have_base=0 have_head=0
  case "$status" in
    added) ;;
    *) fetch "$previous" "$base_sha" "$base_file" && have_base=1 ;;
  esac
  case "$status" in
    removed) ;;
    *) fetch "$path" "$head_sha" "$head_file" && have_head=1 ;;
  esac
  # Skip files that cannot be shown as text on a side that should exist.
  if [ "$status" != added ] && [ "$have_base" = 0 ]; then echo "skip $path (base unavailable)" >&2; continue; fi
  if [ "$status" != removed ] && [ "$have_head" = 0 ]; then echo "skip $path (head unavailable)" >&2; continue; fi
  base_json=null head_json=null
  [ "$have_base" = 1 ] && base_json="$(jq -Rs . <"$base_file")"
  [ "$have_head" = 1 ] && head_json="$(jq -Rs . <"$head_file")"
  jq -nc --arg path "$path" --argjson base "$base_json" --argjson head "$head_json" \
    '{path: $path, base: $base, head: $head}' >>"$tmp/files.jsonl"
  n=$((n + 1))
done < <(jq -c '.[]' <<<"$files_json")

risky_json="$(printf '%s\n' "${risky[@]:-}" | jq -R . | jq -s 'map(select(length > 0))')"
id="${repo//\//__}-$pr"
jq -s --arg id "$id" --arg title "$title" --argjson risky "$risky_json" \
  '{id: $id, split: "local", category: "real", language: "mixed",
    description: $title, risky: $risky, files: .}' "$tmp/files.jsonl" >"$out_dir/$id.json"
echo "wrote $out_dir/$id.json ($n of $total files, ${#risky[@]} risky label(s))"

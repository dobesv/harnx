#!/usr/bin/env bash

set -euo pipefail

poll_seconds="${HARNX_WAIT_PR_POLL_SECONDS:-60}"
stall_seconds="${HARNX_WAIT_PR_STALL_SECONDS:-900}"
settle_seconds="${HARNX_WAIT_PR_SETTLE_SECONDS:-300}"
max_errors="${HARNX_WAIT_PR_MAX_ERRORS:-3}"

pr_url="${PR_URL:-}"
repo="${REPO:-}"
branch="${BRANCH:-}"
head_owner="${HEAD_OWNER:-}"
base="${BASE:-}"

# In protocol mode stdout carries only progress lines, so everything meant for
# the model goes to stderr, which becomes the tool result.
jsonl=0
if [[ "${HARNX_TOOL_PROTOCOL:-}" == "jsonl" ]]; then
  jsonl=1
fi

stderr_file=$(mktemp "${TMPDIR:-/tmp}/harnx-wait-pr.XXXXXX")
cleanup() {
  rm -f -- "$stderr_file"
}
trap cleanup EXIT

started_at=$SECONDS
consecutive_errors=0
previous_status=""
previous_activity=""
status_stable_since=0
candidate_reason=""
candidate_activity=""
candidate_since=0
candidate_observations=0

json_string() {
  local s=$1
  s=${s//\\/\\\\}; s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}; s=${s//$'\r'/\\r}; s=${s//$'\t'/\\t}
  printf '"%s"' "$(printf '%s' "$s" | tr -d '\000-\010\013\014\016-\037')"
}

emit_progress() {
  ((jsonl)) || return 0
  printf '{"progress":{"title":%s,"markdown":%s}}\n' \
    "$(json_string "$1")" "$(json_string "$2")"
}

# Progress patches merge, so a title-only patch leaves the last markdown (the
# link) on screen.
emit_title() {
  ((jsonl)) || return 0
  printf '{"progress":{"title":%s}}\n' "$(json_string "$1")"
}

say() {
  if ((jsonl)); then
    printf '%s\n' "$1" >&2
  else
    printf '%s\n' "$1"
  fi
}

# Percent-encodes a branch name for a URL path. Git allows `#`, `%` and `&` in
# branch names, and a bare `#` would cut the link short. `/` stays as it is, so
# the link reads like the ones GitHub shows.
url_path_encode() (
  LC_ALL=C
  s=$1 out=""
  for ((i = 0; i < ${#s}; i++)); do
    c=${s:i:1}
    case $c in
      [A-Za-z0-9._~/-]) out+=$c ;;
      *) out+=$(printf '%%%02X' "'$c") ;;
    esac
  done
  printf '%s' "$out"
)

# Page where the user can open the PR for the branch being waited on. The head
# is owner-qualified only for forks; a branch in the repo itself needs no prefix.
compare_url() {
  local head
  head=$(url_path_encode "$branch")
  if [[ -n "$head_owner" && "$head_owner" != "${repo%%/*}" ]]; then
    head="${head_owner}:${head}"
  fi
  local range="$head"
  if [[ -n "$base" ]]; then
    range="$(url_path_encode "$base")...${head}"
  fi
  printf 'https://%s/%s/compare/%s?expand=1' "${GH_HOST:-github.com}" "$repo" "$range"
}

report_checks() {
  local trimmed="${pr_url%/}"
  local number="${trimmed##*/}"
  local markdown
  markdown=$(printf '[PR #%s](%s): %s passed, %s failed, %s pending.' \
    "$number" "$pr_url" "$checks_pass" "$checks_fail" "$checks_pending")
  if [[ -n "$candidate_reason" ]]; then
    markdown+=" Waiting ${settle_seconds}s for quiet activity."
  fi
  emit_progress \
    "PR #${number} · ${checks_pass} passed · ${checks_fail} failed · ${checks_pending} pending" \
    "$markdown"
}

retry_or_fail() {
  local operation="$1"
  local status="$2"
  local details="$3"
  local first_line="${details%%$'\n'*}"

  consecutive_errors=$((consecutive_errors + 1))
  printf 'GitHub query failed (%s, attempt %s/%s, exit %s): %.240s\n' \
    "$operation" "$consecutive_errors" "$max_errors" "$status" "$first_line" >&2
  emit_title "GitHub query failed, retry ${consecutive_errors}/${max_errors}"
  if ((consecutive_errors >= max_errors)); then
    printf 'Stopping after %s consecutive GitHub query failures.\n' "$consecutive_errors" >&2
    exit 1
  fi
}

sleep_until_next_poll() {
  sleep "$poll_seconds"
}

count_check_bucket() {
  local bucket="$1"
  awk -F '\t' -v expected="$bucket" '$1 == expected { count += 1 } END { print count + 0 }' \
    <<<"$check_rows"
}

print_result() {
  local reason="$1"
  local state="$2"
  local elapsed=$((SECONDS - started_at))

  {
    printf 'PR_STABLE\n'
    printf 'url=%s\n' "$pr_url"
    printf 'reason=%s\n' "$reason"
    printf 'state=%s\n' "$state"
    printf 'head=%s\n' "$head_oid"
    printf 'checks_total=%s\n' "$checks_total"
    printf 'checks_pass=%s\n' "$checks_pass"
    printf 'checks_fail=%s\n' "$checks_fail"
    printf 'checks_pending=%s\n' "$checks_pending"
    printf 'checks_skipping=%s\n' "$checks_skipping"
    printf 'checks_cancel=%s\n' "$checks_cancel"
    printf 'elapsed_secs=%s\n' "$elapsed"
  } | if ((jsonl)); then cat >&2; else cat; fi
}

if ! command -v gh >/dev/null 2>&1; then
  printf 'The GitHub CLI (gh) is required to wait for pull request stability.\n' >&2
  exit 1
fi

if [[ -z "$pr_url" ]]; then
  if [[ -z "$repo" ]]; then
    if repo_output=$(gh repo view --json nameWithOwner --jq '.nameWithOwner' \
      2>"$stderr_file"); then
      :
    else
      status=$?
      repo_error=$(<"$stderr_file")
      printf 'Could not determine the GitHub repository (exit %s): %.240s\n' \
        "$status" "${repo_error%%$'\n'*}" >&2
      exit 1
    fi
    repo="$repo_output"
  fi

  if [[ -z "$branch" ]]; then
    branch=$(git branch --show-current)
  fi
  if [[ -z "$branch" ]]; then
    printf 'A branch is required when no pull request URL is supplied.\n' >&2
    exit 1
  fi

  waiting_link=$(compare_url)
  waiting_note="Waiting for an open pull request for ${repo}:${branch}..."
  if ((jsonl)); then
    waiting_note+=" Open one at ${waiting_link}"
  fi
  say "$waiting_note"
  emit_progress "Waiting for a PR · ${repo}:${branch}" \
    "No pull request yet. [Open pull request](${waiting_link})"
fi

while true; do
  if [[ -z "$pr_url" ]]; then
    if matches=$(gh pr list --repo "$repo" --head "$branch" --state open --limit 20 \
      --json url,headRepositoryOwner \
      --jq '.[] | [(.headRepositoryOwner.login // ""), .url] | @tsv' \
      2>"$stderr_file"); then
      consecutive_errors=0
    else
      status=$?
      matches_error=$(<"$stderr_file")
      retry_or_fail "find pull request" "$status" "$matches_error"
      sleep_until_next_poll
      continue
    fi

    matching_urls=()
    while IFS=$'\t' read -r owner candidate_url; do
      if [[ -n "$candidate_url" && (-z "$head_owner" || "$owner" == "$head_owner") ]]; then
        matching_urls+=("$candidate_url")
      fi
    done <<<"$matches"

    if ((${#matching_urls[@]} == 0)); then
      emit_progress "Waiting for a PR · ${repo}:${branch}" \
        "No pull request yet. [Open pull request](${waiting_link})"
      sleep_until_next_poll
      continue
    fi
    if ((${#matching_urls[@]} > 1)); then
      printf 'Multiple open pull requests match %s:%s; supply pr_url or head_owner.\n' \
        "$repo" "$branch" >&2
      exit 1
    fi

    pr_url="${matching_urls[0]}"
    say "Monitoring $pr_url"
  fi

  if pr_info=$(gh pr view "$pr_url" \
    --json state,url,headRefOid,updatedAt,comments,reviews \
    --jq '[.state, .url, .headRefOid, .updatedAt, ((.comments // []) | map({id, createdAt, updatedAt, body}) | sort_by(.id) | tojson | @base64), ((.reviews // []) | map({id, submittedAt, state, body}) | sort_by(.id) | tojson | @base64)] | @tsv' \
    2>"$stderr_file"); then
    :
  else
    status=$?
    pr_error=$(<"$stderr_file")
    retry_or_fail "read pull request" "$status" "$pr_error"
    sleep_until_next_poll
    continue
  fi

  IFS=$'\t' read -r pr_state canonical_url head_oid updated_at comments_fingerprint \
    reviews_fingerprint <<<"$pr_info"
  if [[ -z "$pr_state" || -z "$canonical_url" || -z "$head_oid" ]]; then
    retry_or_fail "parse pull request" 1 "GitHub returned incomplete pull request data"
    sleep_until_next_poll
    continue
  fi
  pr_url="$canonical_url"

  check_rows=""
  if [[ "$pr_state" == "OPEN" ]]; then
    if check_rows=$(gh pr checks "$pr_url" --json bucket,name,state,workflow \
      --jq 'sort_by(.workflow, .name, .state, .bucket) | .[] | [.bucket, .workflow, .name, .state] | @tsv' \
      2>"$stderr_file"); then
      consecutive_errors=0
    else
      status=$?
      checks_error=$(<"$stderr_file")
      if ((status == 8)) && [[ -n "$check_rows" ]]; then
        consecutive_errors=0
      elif ((status == 1)) && \
        [[ "$check_rows" == *"no checks reported"* || "$checks_error" == *"no checks reported"* ]]; then
        check_rows=""
        consecutive_errors=0
      else
        retry_or_fail "read status checks" "$status" "${checks_error:-$check_rows}"
        sleep_until_next_poll
        continue
      fi
    fi
  fi
  consecutive_errors=0

  checks_total=$(awk -F '\t' 'NF >= 4 { count += 1 } END { print count + 0 }' <<<"$check_rows")
  checks_pass=$(count_check_bucket pass)
  checks_fail=$(count_check_bucket fail)
  checks_pending=$(count_check_bucket pending)
  checks_skipping=$(count_check_bucket skipping)
  checks_cancel=$(count_check_bucket cancel)

  case "$pr_state" in
    CLOSED)
      print_result pr_closed "$pr_state"
      exit 0
      ;;
    MERGED)
      print_result pr_merged "$pr_state"
      exit 0
      ;;
    OPEN) ;;
    *)
      printf 'Unexpected pull request state: %s\n' "$pr_state" >&2
      exit 1
      ;;
  esac

  now=$SECONDS
  status_fingerprint="${head_oid}"$'\n'"${check_rows}"
  activity_fingerprint="${status_fingerprint}"$'\n'"${updated_at}"$'\n'\
"${comments_fingerprint}"$'\n'"${reviews_fingerprint}"

  if [[ -z "$previous_status" || "$status_fingerprint" != "$previous_status" ]]; then
    status_stable_since=$now
    candidate_reason=""
    candidate_activity=""
    candidate_observations=0
  elif [[ "$activity_fingerprint" != "$previous_activity" ]]; then
    candidate_reason=""
    candidate_activity=""
    candidate_observations=0
  fi

  reason=""
  if ((checks_total > 0 && checks_pending == 0)); then
    reason="checks_terminal"
  elif ((now - status_stable_since >= stall_seconds)); then
    reason="activity_stalled"
  fi

  if [[ -n "$reason" ]]; then
    if [[ "$candidate_reason" == "$reason" && "$candidate_activity" == "$activity_fingerprint" ]]; then
      candidate_observations=$((candidate_observations + 1))
      if ((candidate_observations >= 2 && now - candidate_since >= settle_seconds)); then
        print_result "$reason" "$pr_state"
        exit 0
      fi
    else
      candidate_reason="$reason"
      candidate_activity="$activity_fingerprint"
      candidate_since=$now
      candidate_observations=1
      if [[ "$reason" == "checks_terminal" ]]; then
        say "All ${checks_total} status checks are terminal; waiting ${settle_seconds}s for quiet activity."
      else
        say "Head and checks are unchanged; waiting ${settle_seconds}s for quiet activity."
      fi
    fi
  fi

  report_checks
  previous_status="$status_fingerprint"
  previous_activity="$activity_fingerprint"
  sleep_until_next_poll
done

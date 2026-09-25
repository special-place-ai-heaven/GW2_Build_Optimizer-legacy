#!/usr/bin/env bash
# Fail unless every commit's author and Co-authored-by are
# special-place-administrator at an allowlisted email. A person committer
# must be that same identity. GitHub and web-flow <noreply@github.com>
# are allowed (squash merges).
# Usage: check-commit-identity.sh <base...head | sha...>
# CHECK_COMMIT_IDENTITY_SELFTEST=1 proves a Cursor Co-authored-by fails
# and a clean admin commit passes, then checks the arguments.
set -euo pipefail

ALLOW_NAME=special-place-administrator
ALLOW_EMAIL_1=197073597+special-place-administrator@users.noreply.github.com
ALLOW_EMAIL_2=administrator@special-place.online

email_ok() {
  case "$1" in
    "$ALLOW_EMAIL_1"|"$ALLOW_EMAIL_2") return 0 ;;
    *) return 1 ;;
  esac
}

committer_ok() {
  if { [[ "$1" == "GitHub" ]] || [[ "$1" == "web-flow" ]]; } && [[ "$2" == "noreply@github.com" ]]; then
    return 0
  fi
  [[ "$1" == "$ALLOW_NAME" ]] && email_ok "$2"
}

coauthor_ok() {
  local line=$1 name email
  # Stored so [[ does not treat < as a redirect. Unquoted on the right of =~.
  local re='^(.*)[[:space:]]<([^[:space:]>]+)>$'
  if [[ ! "$line" =~ $re ]]; then
    return 1
  fi
  name=${BASH_REMATCH[1]}
  email=${BASH_REMATCH[2]}
  name=${name%"${name##*[![:space:]]}"}
  [[ "$name" == "$ALLOW_NAME" ]] && email_ok "$email"
}

check_commit() {
  local sha=$1 meta an ae cn ce trailers line bad=0
  if ! meta=$(git log -1 --format='%an%x09%ae%x09%cn%x09%ce' "$sha"); then
    echo "$sha: unreadable" >&2
    return 1
  fi
  IFS=$'\t' read -r an ae cn ce <<<"$meta"
  if [[ "$an" != "$ALLOW_NAME" ]]; then
    echo "$sha: author name: $an" >&2
    bad=1
  fi
  if ! email_ok "$ae"; then
    echo "$sha: author email: $ae" >&2
    bad=1
  fi
  if ! committer_ok "$cn" "$ce"; then
    echo "$sha: committer: $cn <$ce>" >&2
    bad=1
  fi
  trailers=$(git log -1 --format='%(trailers:key=Co-authored-by,valueonly,unfold)' "$sha")
  while IFS= read -r line; do
    line=${line%$'\r'}
    line=${line#"${line%%[![:space:]]*}"}
    line=${line%"${line##*[![:space:]]}"}
    [[ -z "$line" ]] && continue
    if ! coauthor_ok "$line"; then
      echo "$sha: co-authored-by: $line" >&2
      bad=1
    fi
  done <<<"$trailers"
  return "$bad"
}

selftest() {
  local tmp script tree good bad out status
  script=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
  tmp=$(mktemp -d)
  git init -q -b main "$tmp"
  git -C "$tmp" config user.name "$ALLOW_NAME"
  git -C "$tmp" config user.email "$ALLOW_EMAIL_1"
  tree=$(git -C "$tmp" mktree </dev/null)
  good=$(printf 'clean admin\n' | git -C "$tmp" commit-tree "$tree" -F -)
  bad=$(printf 'bad\n\nCo-authored-by: Cursor Agent <cursoragent@cursor.com>\n' | git -C "$tmp" commit-tree "$tree" -F -)
  set +e
  (
    set -euo pipefail
    cd "$tmp"
    if out=$(env -u CHECK_COMMIT_IDENTITY_SELFTEST bash "$script" "$bad" 2>&1); then
      echo "selftest: Cursor Co-authored-by was accepted" >&2
      exit 1
    fi
    if [[ "$out" != *"$bad"* || "$out" != *co-authored-by* ]]; then
      echo "selftest: expected SHA and co-authored-by, got: $out" >&2
      exit 1
    fi
    if ! out=$(env -u CHECK_COMMIT_IDENTITY_SELFTEST bash "$script" "$good" 2>&1); then
      echo "selftest: clean admin commit was rejected: $out" >&2
      exit 1
    fi
  )
  status=$?
  set -e
  rm -rf "$tmp"
  if [[ "$status" -ne 0 ]]; then
    exit "$status"
  fi
  echo "selftest ok"
}

if [[ "${CHECK_COMMIT_IDENTITY_SELFTEST:-}" == "1" ]]; then
  selftest
fi

if [[ $# -eq 0 ]]; then
  if [[ "${CHECK_COMMIT_IDENTITY_SELFTEST:-}" == "1" ]]; then
    exit 0
  fi
  echo "usage: check-commit-identity.sh <base...head | sha...>" >&2
  exit 2
fi

list=
if [[ $# -eq 1 && "$1" == *..* ]]; then
  if ! list=$(git rev-list "$1"); then
    echo "check-commit-identity: bad range: $1" >&2
    exit 1
  fi
else
  arg=
  canon=
  for arg in "$@"; do
    if ! canon=$(git rev-parse --verify "${arg}^{commit}"); then
      echo "check-commit-identity: bad sha: $arg" >&2
      exit 1
    fi
    list+="$canon"$'\n'
  done
fi

fail=0
sha=
while IFS= read -r sha; do
  [[ -z "$sha" ]] && continue
  if ! check_commit "$sha"; then
    fail=1
  fi
done <<<"$list"
exit "$fail"

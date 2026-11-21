#!/usr/bin/env bash
# Two-way sync of an Obsidian vault (or any notes folder) through a git remote.
#
# Each run: commit local changes, merge what the remote has, push the result. Run it from a timer
# on every machine that holds a copy. Notes merge line by line and keep both sides' lines when two
# machines edited the same place; for other files this machine's version wins and the other
# version stays in the git history. Nothing is ever deleted without a commit recording it.
#
#   vault-sync.sh init <vault> [<remote-url>]   prepare a vault (safe to repeat)
#   vault-sync.sh <vault>                       sync once
set -euo pipefail

usage() {
  printf 'usage: %s init <vault> [<remote-url>]\n       %s <vault>\n' "${0##*/}" "${0##*/}" >&2
  exit 2
}

log() { printf '[vault-sync] %s\n' "$*" >&2; }

readonly BRANCH=main

init() {
  local vault=$1 remote=${2:-}
  [[ -d $vault ]] || { log "not a folder: $vault"; exit 1; }
  cd -- "$vault"
  [[ -d .git ]] || git init -q -b "$BRANCH"
  git config user.name "vault-sync"
  git config user.email "vault-sync@$(uname -n)"
  # Keep both sides' lines when the same note changed in two places (git's built-in union merge).
  [[ -f .gitattributes ]] || printf '*.md merge=union\n' > .gitattributes
  # Per-machine Obsidian state and sync leftovers are not content.
  [[ -f .gitignore ]] || printf '%s\n' '.obsidian/workspace*.json' '.obsidian/cache' '.trash/' '*.lock' '*.tmp[0-9]*' > .gitignore
  if [[ -n $remote ]]; then
    if git remote get-url origin >/dev/null 2>&1; then git remote set-url origin "$remote"; else git remote add origin "$remote"; fi
  fi
  log "ready: $vault"
}

sync() {
  local vault=$1
  [[ -d $vault/.git ]] || { log "not prepared (run: ${0##*/} init \"$vault\"): $vault"; exit 1; }
  cd -- "$vault"
  # One sync at a time per vault; a second run simply skips.
  exec 9> .git/vault-sync.lock
  flock -n 9 || { log "another sync is running"; exit 0; }

  git add -A
  if ! git diff --cached --quiet; then
    git commit -q -m "sync: $(uname -n) $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  fi
  git remote get-url origin >/dev/null 2>&1 || exit 0

  local attempt
  for attempt in 1 2 3; do
    git fetch -q origin
    if git rev-parse -q --verify "origin/$BRANCH" >/dev/null; then
      if git rev-parse -q --verify HEAD >/dev/null; then
        git merge -q --no-edit --allow-unrelated-histories -X ours "origin/$BRANCH" || {
          log "merge needs attention in $vault; nothing was pushed"
          git merge --abort 2>/dev/null || true
          exit 1
        }
      else
        git reset -q --hard "origin/$BRANCH"
      fi
    fi
    git rev-parse -q --verify HEAD >/dev/null || exit 0
    if git push -q origin "HEAD:$BRANCH" 2>/dev/null; then
      exit 0
    fi
    log "remote changed during sync, retrying ($attempt)"
  done
  log "could not push after 3 attempts"
  exit 1
}

case ${1:-} in
  init) [[ $# -ge 2 ]] || usage; init "$2" "${3:-}" ;;
  -h | --help | '') usage ;;
  *) [[ $# -eq 1 ]] || usage; sync "$1" ;;
esac

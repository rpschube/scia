#!/usr/bin/env bash
# Privacy & information-hygiene scan for a public repository.
#
# Scans, once with the stock gitleaks secret rules (.gitleaks-stock.toml) and
# once with the project rules (.gitleaks.toml) — separate, explicit passes:
# the stock global allowlist would otherwise suppress project findings, and an
# implicit config would be shadowed by whichever file sits in the repo root:
#   1. the full git history reachable from all refs
#   2. the working tree (tracked + untracked, ignored files excluded)
#   3. commit messages, author/committer identities and branch names of
#      every commit that is not yet on the base branch
#   4. optionally, a second private rule set kept outside the repository
#      (SCIA_PRIVATE_RULES, default: $XDG_CONFIG_HOME/scia/gitleaks-private.toml)
#
# Exit status is non-zero if anything is found. Run by `just gate`, by the
# pre-push hook and by CI (`--ci`).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

CI_MODE=0
for arg in "$@"; do
  case "$arg" in
    --ci) CI_MODE=1 ;;
    *) echo "usage: $0 [--ci]" >&2; exit 2 ;;
  esac
done

command -v gitleaks >/dev/null || { echo "privacy-scan: gitleaks not installed" >&2; exit 2; }

STOCK_RULES=".gitleaks-stock.toml"
PUBLIC_RULES=".gitleaks.toml"
PRIVATE_RULES="${SCIA_PRIVATE_RULES:-${XDG_CONFIG_HOME:-$HOME/.config}/scia/gitleaks-private.toml}"
ALLOWED_AUTHOR_NAMES='^(Ryan Schubert|rpschube|GitHub|github-actions\[bot\])$'
ALLOWED_AUTHOR_EMAILS='^(ryan@stomale\.cc|noreply@github\.com|[0-9]+\+[A-Za-z0-9-]+@users\.noreply\.github\.com|41898282\+github-actions\[bot\]@users\.noreply\.github\.com)$'

fail=0
say() { printf '\n== %s\n' "$*"; }
bad() { printf '   FAIL: %s\n' "$*"; fail=1; }

GL=(gitleaks --no-banner --redact=60 --exit-code 1)

# Base for "commits not yet published": explicit > upstream master > everything.
if [[ -n "${SCIA_SCAN_BASE:-}" ]] && git rev-parse -q --verify "${SCIA_SCAN_BASE}^{commit}" >/dev/null 2>&1; then
  BASE="$SCIA_SCAN_BASE"
elif git rev-parse -q --verify origin/master >/dev/null 2>&1; then
  BASE="origin/master"
else
  BASE=""
fi
RANGE="${BASE:+$BASE..}HEAD"

scan_with() {
  local rules="$1" label="$2"
  local cfg=(--config "$rules")

  say "$label: git history (all refs)"
  if [[ "$(git rev-list --all --count)" -gt 0 ]]; then
    "${GL[@]}" git . "${cfg[@]}" --log-opts="--all" -v || bad "history contains findings"
  else
    echo "   (no commits yet)"
  fi

  say "$label: working tree"
  local tmp; tmp="$(mktemp -d)"
  git ls-files -z --cached --others --exclude-standard | tar --null -T - -cf - 2>/dev/null | tar -xf - -C "$tmp"
  "${GL[@]}" dir "$tmp" "${cfg[@]}" -v || bad "working tree contains findings"
  rm -rf "$tmp"

  if git rev-parse -q --verify HEAD >/dev/null 2>&1; then
    say "$label: commit messages + branch names ($RANGE)"
    {
      git log --format='commit %H%n%B' "$RANGE" --
      git for-each-ref --format='branch %(refname:short)' refs/heads
    } | "${GL[@]}" stdin "${cfg[@]}" -v || bad "commit messages or branch names contain findings"
  fi
}

scan_with "$STOCK_RULES" "stock secret rules"
scan_with "$PUBLIC_RULES" "project rules"

if [[ -f "$PRIVATE_RULES" ]]; then
  scan_with "$PRIVATE_RULES" "private rules"
elif [[ "$CI_MODE" -eq 0 ]]; then
  echo
  echo "note: no private rule set at $PRIVATE_RULES (optional)"
fi

if git rev-parse -q --verify HEAD >/dev/null 2>&1; then
  say "author / committer identity ($RANGE)"
  while IFS=$'\t' read -r name email; do
    [[ "$name" =~ $ALLOWED_AUTHOR_NAMES ]] || bad "author/committer name not allowed: $name"
    [[ "$email" =~ $ALLOWED_AUTHOR_EMAILS ]] || bad "author/committer e-mail not allowed: $email"
  done < <(git log --format='%an%x09%ae%n%cn%x09%ce' "$RANGE" -- | sort -u)
fi

echo
if [[ "$fail" -ne 0 ]]; then
  echo "privacy-scan: FAILED — nothing may be pushed until this is clean."
  exit 1
fi
echo "privacy-scan: clean"

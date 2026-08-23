#!/usr/bin/env bash
# Proves that every privacy rule in .gitleaks.toml actually bites.
#
# Every sample is scanned on its own, so a rule with several alternatives
# cannot hide a dead alternative behind a live one. Samples that must be
# flagged are generated at run time (this script never contains the blocked
# strings); samples that must NOT be flagged (the public project identity,
# documentation placeholders) are checked too. A rule that fails to fire is a
# hard failure: a gate that silently passes is worse than no gate.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
command -v gitleaks >/dev/null || { echo "privacy-selftest: gitleaks not installed" >&2; exit 2; }

RULES=".gitleaks.toml"
STOCK_RULES=".gitleaks-stock.toml"
PRIVATE_RULES="${SCIA_PRIVATE_RULES:-${XDG_CONFIG_HOME:-$HOME/.config}/scia/gitleaks-private.toml}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail=0
ok()  { printf '   ok    %s\n' "$*"; }
bad() { printf '   FAIL  %s\n' "$*"; fail=1; }

# rules_for <text> <rules-file> -> space-separated rule ids that fire on the text
rules_for() {
  local d; d="$(mktemp -d -p "$tmp")"
  printf '%s\n' "$1" > "$d/sample.txt"
  gitleaks dir "$d" --config "$2" --no-banner --redact --exit-code 0 \
    --report-format json --report-path "$d/report.json" >/dev/null 2>&1 || true
  python3 - "$d/report.json" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except Exception:
    data = []
print(" ".join(sorted({f["RuleID"] for f in data})))
PY
}

# must_fire <rule-id> <label> <text>
must_fire() {
  local found; found="$(rules_for "$3" "$RULES")"
  if [[ " $found " == *" $1 "* ]]; then ok "$1: $2"; else bad "$1 did NOT fire on: $2 (got: ${found:-nothing})"; fi
}
# must_not_fire <label> <text>
must_not_fire() {
  local found; found="$(rules_for "$2" "$RULES")"
  if [[ -z "$found" ]]; then ok "clean: $1"; else bad "false positive on '$1': $found"; fi
}

echo "== rules that must fire"
must_fire scia-home-path              "linux home path"        "$(printf 'notes at /home/someone/projects/thing')"
must_fire scia-home-path              "macos home path"        "$(printf 'notes at /Users/someone/Desktop')"
must_fire scia-home-path              "windows profile path"   "$(printf 'copied from C:\\Users\\someone\\Desktop')"
must_fire scia-email                  "private e-mail"         "$(printf 'mail me: some.person@gmail.com')"
must_fire scia-private-workspace-ref  "workspace name"         "$(printf 'see the \x62ivio workspace for context')"
must_fire scia-private-workspace-ref  "session directory"      "$(printf 'see .sessions/1234.md')"
must_fire scia-tooling-ref            "assistant product 1"    "$(printf 'generated with \x63laude')"
must_fire scia-tooling-ref            "assistant product 2"    "$(printf 'ask \x63opilot about it')"
must_fire scia-tooling-ref            "assistant product 3"    "$(printf 'ask \x63hatgpt about it')"
must_fire scia-tooling-ref            "assistant vendor"       "$(printf 'an \x61nthropic model')"
must_fire scia-host-identifier        "private ipv4"           "$(printf 'reachable at 192.168.4.20')"
must_fire scia-host-identifier        "private ipv4 (10/8)"    "$(printf 'reachable at 10.77.0.2')"
must_fire scia-host-identifier        "mdns / lan name"        "$(printf 'reachable at devbox.lan')"

echo "== samples that must stay clean"
must_not_fire "public identity"        "$(printf 'Maintainer: Ryan Schubert <ryan@stomale.cc>')"
must_not_fire "placeholder home path"  "$(printf 'config lives in /home/user/.config/scia and ~/.config')"
must_not_fire "ci runner path"         "$(printf 'CI checks out under /home/runner/work')"
must_not_fire "example e-mail"         "$(printf 'bug reports: bugs@example.com')"
must_not_fire "loopback addresses"     "$(printf 'listen on 127.0.0.1:9000 or 0.0.0.0')"
must_not_fire "public ipv4"            "$(printf 'dns at 1.1.1.1')"

echo "== stock secret rules ($STOCK_RULES)"
fake_pat="ghp_$(python3 -c 'import random,string; r=random.Random(20260823); print("".join(r.choices(string.ascii_letters+string.digits,k=36)))')"
found="$(rules_for "token = \"$fake_pat\"" "$STOCK_RULES")"
if [[ " $found " == *" github-pat "* ]]; then ok "github-pat: stock rules fire"; else bad "stock rules did NOT fire on a token sample (got: ${found:-nothing})"; fi

if [[ -f "$PRIVATE_RULES" ]]; then
  sample="${PRIVATE_RULES%.toml}.sample"
  echo "== private rules ($PRIVATE_RULES)"
  if [[ -f "$sample" ]]; then
    found="$(rules_for "$(cat "$sample")" "$PRIVATE_RULES")"
    if [[ -n "$found" ]]; then ok "private rules fire on their sample ($found)"; else bad "private rules did NOT fire on their sample"; fi
  else
    bad "private rules present but no sample file at $sample"
  fi
fi

echo
if [[ "$fail" -ne 0 ]]; then echo "privacy-selftest: FAILED"; exit 1; fi
echo "privacy-selftest: every rule bites"

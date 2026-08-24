set shell := ["bash", "-euo", "pipefail", "-c"]

# Build target: under the main checkout's target/, ONE SUBDIRECTORY PER BRANCH.
# Worktrees must not share a target directory: two checkouts of the same
# package id write the same artifact and fingerprint paths, and cargo will
# happily serve one branch's stale build to another. Per-branch dirs cost a
# cold build per worktree; `just sweep` and `just wt-rm` reclaim them.
target_root := `realpath -m "$(git rev-parse --path-format=absolute --git-common-dir)/../target"`
branch_slug := `git rev-parse --abbrev-ref HEAD | tr '/' '-'`
target_dir := target_root / branch_slug
export CARGO_TARGET_DIR := target_dir

# Tunables (all overridable from the environment).
lock         := env("SCIA_BUILD_LOCK", "/tmp/scia-build.lock")
jobs         := env("SCIA_BUILD_JOBS", "8")
max_load     := env("SCIA_MAX_LOAD", "24")
test_threads := env("SCIA_TEST_THREADS", "8")

# List available recipes.
default:
    @just --list

# Refuse to build when the machine is already under heavy load.
_guard:
    #!/usr/bin/env bash
    set -euo pipefail
    load=$(cut -d ' ' -f1 /proc/loadavg)
    if awk -v l="$load" -v m="{{max_load}}" 'BEGIN { exit !(l > m) }'; then
        echo "build refused: 1-minute load $load exceeds limit {{max_load}} (override with SCIA_MAX_LOAD)" >&2
        exit 75
    fi

# Serialize every cargo invocation behind the shared build lock, niced and
# ionice'd so an interactive machine stays responsive.
_cargo +args: _guard
    #!/usr/bin/env bash
    set -euo pipefail
    if ! flock -n {{lock}} true; then
        echo "waiting for build lock {{lock}} ..." >&2
    fi
    flock {{lock}} nice -n 10 ionice -c3 env CARGO_BUILD_JOBS={{jobs}} cargo {{args}}

# Point git at the tracked hooks and verify the dev toolchain is installed.
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    git config core.hooksPath .githooks
    echo "hooks path: $(git config core.hooksPath)"
    missing=()
    check() {
        local name="$1"; shift
        if command -v "$name" >/dev/null 2>&1; then
            printf '  %-14s %s\n' "$name" "$("$@" 2>&1 | head -n1)"
        else
            missing+=("$name")
        fi
    }
    check gitleaks       gitleaks version
    check cargo-nextest  cargo nextest --version
    check cargo-deny     cargo deny --version
    check cargo-sweep    cargo sweep --version
    check just           just --version
    if (( ${#missing[@]} )); then
        echo "missing tools: ${missing[*]}" >&2
        exit 1
    fi

# Type-check the whole workspace, including tests, benches and examples.
check: (_cargo "check" "--workspace" "--all-targets")

# Build the whole workspace (debug profile).
build: (_cargo "build" "--workspace")

# Run the test suite via nextest.
test: (_cargo "nextest" "run" "--workspace" "--test-threads" test_threads)

# P5 probe: Luau scene-tick benchmark (an optimized build of one small crate,
# still under the shared lock/nice wrapper; the only release build we run).
bench-p5: (_cargo "bench" "-p" "scia-scenes" "--bench" "luau_tick")

# Format all crates in place.
fmt:
    cargo fmt --all

# Verify formatting without changing files.
fmt-check:
    cargo fmt --all --check

# Lint the whole workspace, warnings are errors.
clippy: (_cargo "clippy" "--workspace" "--all-targets" "--" "-D" "warnings")

# Check dependency licenses, bans and sources (offline).
deny: (_cargo "deny" "check" "licenses" "bans" "sources")

# Enforce that scia-core pulls in no UI or scripting dependency.
core-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    crates=$(just _cargo tree -p scia-core -e normal --prefix none | awk '{ print $1 }')
    if echo "$crates" | grep -Eiw 'scia-scenes|scia-meta|scia-tui|ratatui|crossterm|wgpu|winit|mlua'; then
        echo "core-deps: scia-core has a forbidden UI/scripting dependency (see above)" >&2
        exit 1
    fi
    echo "core-deps: scia-core is clean"

# Run the privacy self-test then the full privacy scan.
privacy:
    scripts/privacy-selftest.sh
    scripts/privacy-scan.sh

# THE merge gate: fmt, lint, deny, core isolation, privacy, tests. CI mirrors this.
gate: fmt-check clippy deny core-deps privacy test

# Create a new worktree + branch under ../scia-wt (off `base`) and copy local-only files in.
wt branch base="master":
    #!/usr/bin/env bash
    set -euo pipefail
    common=$(git rev-parse --path-format=absolute --git-common-dir)
    main=$(dirname "$common")
    dest="$main/../scia-wt/{{branch}}"
    git worktree add -b "{{branch}}" "$dest" "{{base}}"
    dest=$(realpath -m "$dest")
    while IFS= read -r f; do
        [[ -z "$f" ]] && continue
        [[ -e "$main/$f" ]] || continue
        mkdir -p "$dest/$(dirname "$f")"
        cp -p "$main/$f" "$dest/$f"
    done < <(git -C "$main" ls-files --others --ignored --exclude-from="$common/info/exclude")
    echo "$dest"

# Remove a worktree created by `wt` and soft-delete its branch.
wt-rm branch:
    #!/usr/bin/env bash
    set -euo pipefail
    common=$(git rev-parse --path-format=absolute --git-common-dir)
    main=$(dirname "$common")
    git worktree remove "$main/../scia-wt/{{branch}}"
    git branch -d "{{branch}}"
    rm -rf "{{target_root}}/$(printf '%s' "{{branch}}" | tr '/' '-')"

# Reclaim build artifacts older than 7 days (every branch's target dir) and prune dead worktrees.
sweep:
    cargo sweep --time 7 --recursive {{target_root}}
    git worktree prune

# Report whether the shared build lock is currently held.
lock-status:
    #!/usr/bin/env bash
    set -euo pipefail
    if flock -n {{lock}} true 2>/dev/null; then
        echo free
    else
        echo held
    fi

# Wind a session down: sweep, report the lock, list worktrees.
session-end: sweep lock-status
    git worktree list

# Regenerate the whole changelog for a release tag (release time only).
changelog tag:
    git-cliff --tag {{tag}} -o CHANGELOG.md

# Show what a release would build (cargo-dist plan; no compilation).
dist-plan:
    dist plan

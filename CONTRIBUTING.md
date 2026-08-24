# Contributing to scia

Thanks for your interest. scia is early and moving fast; this guide describes
the rules every change follows. It will grow as the project does.

## Privacy & information hygiene

This repository is public, and everything pushed to it — including branches
and pull requests — is permanently public. The following never appear in code,
comments, commit messages, branch names, issues, pull requests or documentation:

- Personal details beyond the project's public contact identity
  (maintainer: Ryan Schubert, `ryan@stomale.cc`)
- Home-directory paths, machine or device names, LAN addresses, or any other
  description of a specific local environment ("the dev machine" is enough)
- Logs, transcripts, tool output, session dumps or debug captures pasted
  verbatim; attach the minimal excerpt that demonstrates the point
- References to private notes, workspaces or task systems
- Names of assistant or AI tooling products

Public writing shares the minimum the project needs. When in doubt, leave it
out.

These rules are enforced, not just requested: `.gitleaks.toml` holds the
project-specific rules and runs as its own pass next to the stock gitleaks
secret rules, `scripts/privacy-scan.sh` runs both passes over history, the
working tree, commit messages and author identities — in the pre-push hook
and in CI on every pull request — and
`scripts/privacy-selftest.sh` proves that every rule fires before the scan is
trusted. Anything that reaches the public repository despite the gates is
treated as permanently disclosed: rotate or react accordingly — rewriting
history is a mitigation, not an undo.

## Identity

Commits use the author's public name and e-mail only. Pull requests are
squash-merged, so the pull-request title becomes the commit on `master`.

## Development setup

The toolchain is pinned by `rust-toolchain.toml` (Rust 1.96.1 with `rustfmt`
and `clippy`), so the right compiler and components install automatically the
first time you build. On top of the toolchain you need a few tools:

| Tool             | Used for                                             |
| ---------------- | ---------------------------------------------------- |
| `just`           | every build and workflow command (never bare cargo)  |
| `cargo-nextest`  | the test runner                                      |
| `cargo-deny`     | license, ban and source checks                       |
| `cargo-sweep`    | reclaiming stale build output                        |
| `gitleaks`       | the privacy scan                                     |

Run `just setup` once after cloning. It points git's hooks path at
`.githooks` (so the privacy pre-push hook is active) and checks that the tools
above are present. From then on, drive everything through `just` — run `just`
with no arguments to list the available recipes.

## Building safely

Every build and test goes through `just`; never invoke `cargo` directly. The
wrappers exist because the machine that builds scia is shared with other work,
and unguarded builds have caused real trouble in the past: once a build spike
disrupted other services on the machine, and once stale build directories
filled the disk. The wrappers make those outcomes hard to repeat.

Common recipes:

| Recipe            | What it does                                             |
| ----------------- | ------------------------------------------------------- |
| `just check`      | `cargo check` — the fast inner loop                     |
| `just build`      | a full debug build (only when you need artifacts)       |
| `just test`       | the test suite via nextest                              |
| `just clippy`     | lints, warnings-as-errors                               |
| `just fmt`        | format the tree                                         |
| `just fmt-check`  | verify formatting without writing                       |
| `just deny`       | `cargo deny check licenses bans sources`                |
| `just core-deps`  | assert `scia-core` pulls in no scene/UI crates          |
| `just privacy`    | the privacy self-test and scan                          |
| `just gate`       | the full merge gate (see below)                         |

Every cargo invocation the wrappers run is protected the same way:

- **A global build lock.** At most one build or test runs at a time; parallel
  workers queue behind the lock rather than piling on.
- **Reduced priority.** Builds run niced and with idle I/O priority so they
  yield to everything else on the machine.
- **Bounded parallelism.** Compile jobs and test threads are capped (defaults
  of 8 each; override with `SCIA_BUILD_JOBS` and `SCIA_TEST_THREADS`).
- **A load guard.** A build refuses to start when the one-minute load average
  is above 24 (`SCIA_MAX_LOAD`), so it never kicks off on an already-busy
  machine.

`cargo check` is the inner loop; run full builds only when you actually need
artifacts. Release builds are CI's job — never build a release locally.

Every branch builds into its own subdirectory of the main checkout's
`target/` (the justfile derives it from the branch name). Worktrees never
share build output: two checkouts of the same package would write the same
artifact paths and cargo can serve one branch's stale build to another. A new
worktree therefore costs one cold build; `just sweep` and `just wt-rm` reclaim
the space.

Worktree and cleanup recipes:

- `just wt <branch>` creates a sibling worktree for `<branch>`; `just wt-rm
  <branch>` removes it.
- `just sweep` runs `cargo sweep --time 7` and prunes stale worktree
  metadata.
- `just session-end` runs the sweep, reports build-lock status, and lists
  worktrees. Run it at the end of every working session.

## Branching & commits

The project is trunk-based: short-lived branches off `master`, merged back
quickly. Name a branch `<type>/<slug>`, where `<type>` is one of `feat`,
`fix`, `perf`, `refactor`, `test`, `docs`, `chore`, `ci` or `build`. When
several workers touch similar areas in parallel, disambiguate with
`<type>/<worker>-<slug>`. Branches live days, not weeks; carry a long-running
effort behind a cargo feature and land it in small pull requests.

[Conventional Commits](https://www.conventionalcommits.org/) are enforced on
the **pull-request title**, not on individual commits. CI checks the title
against `^(feat|fix|perf|refactor|test|docs|chore|ci|build|revert)(\([a-z0-9._/-]+\))?!?: [^ ].+`
and requires it to be 72 characters or fewer. Because pull requests are
squash-merged and the title becomes the commit on `master`, work-in-progress
commits on your branch can be named however you like — only the final title
matters. Changelog sections are generated from commit types by git-cliff at
release time, so an accurate title is what ends up in the release notes.

`master` is protected: changes land through pull requests only (admins
included), required checks must pass, history is linear, and force-pushes are
rejected.

## Merge gate & review

Nothing reaches `master` except through a green pull request. The path for
every change is the same:

1. Rebase the branch on current `master`.
2. Run `just gate` on the rebased head.
3. Let CI go green.
4. Squash-merge.

`just gate` is the single source of truth, defined identically for local use
and CI. It runs, in order:

1. `fmt-check` — formatting is correct
2. `clippy` with warnings denied
3. `cargo deny check licenses bans sources`
4. `core-deps` — `scia-core` never depends on scene or UI crates
5. the privacy self-test and scan
6. `cargo nextest run` — the test suite

Run the gate on the rebased head before every merge, not just once when you
open the pull request.

Review expectations:

- Substantive changes get a review pass before merge. Trivial, mechanical
  changes may merge on green checks alone.
- Fold mechanical follow-ups into the reviewed pull request rather than
  pushing them after approval.
- Every review comment is addressed before merge — either fixed, or
  explicitly captured with a reply explaining why not. Approval alone is not
  "done"; merged with a green gate is done.

## Generated files & lockfiles

Generated files and lockfiles (`Cargo.lock` and anything produced by a tool)
are never hand-merged. When a rebase produces a conflict in a generated file,
**regenerate it on the rebased head** rather than resolving the conflict by
hand. After the rebase, re-check every file that conflicted — a botched
generated-file merge can silently corrupt an unrelated file — and run the full
gate on the final rebased head before merging.

## Working in parallel

Several contributors — people and scripted workers alike — often have work in
flight at once. A few rules keep that safe:

- **One contributor, one branch, one worktree.** Never share a working copy,
  and never put two workers on the same branch.
- **Sequence high-collision files.** The root `Cargo.toml`, `Cargo.lock`, and
  anything under `.github/workflows/` are edited by at most one in-flight
  branch at a time. Land such a change before starting another that touches
  the same file.
- **Automated contributors never invent scope.** A scripted worker implements
  exactly what a fully specified item describes. If the specification is
  ambiguous, contradictory or impossible, it stops and reports rather than
  deciding on its own.
- **Clean up at the end.** Prune merged worktrees and sweep stale build output
  when you finish a session (`just session-end`).

## Session checklist

Before you consider a change done:

1. Rebase on current `master`.
2. Run `just gate` on the rebased head and get it green.
3. Address every review comment — fix it or reply.
4. Run `just session-end` to sweep build output and prune merged worktrees.

## Licensing

scia is dual-licensed under **MIT OR Apache-2.0**, and every dependency must
be permissively licensed — `cargo-deny` enforces this in the gate.
GPL/LGPL/AGPL algorithm sources are never vendored or linked; where such an
algorithm is needed it is reimplemented from the published papers, with
attribution in the documentation. Contributions are dual-licensed by default,
as the README states — by submitting a change you agree to release it under
both licenses.

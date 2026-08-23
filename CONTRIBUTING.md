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

These rules are enforced, not just requested: `.gitleaks.toml` extends the
gitleaks defaults with project-specific rules, `scripts/privacy-scan.sh` runs
in the pre-push hook and in CI on every pull request, and
`scripts/privacy-selftest.sh` proves that every rule fires before the scan is
trusted. Anything that reaches the public repository despite the gates is
treated as permanently disclosed: rotate or react accordingly — rewriting
history is a mitigation, not an undo.

## Identity

Commits use the author's public name and e-mail only. Pull requests are
squash-merged, so the pull-request title becomes the commit on `master`.

## Development workflow

The build wrappers, branching and review rules, and the pull-request gate are
documented in the sections below as they land.

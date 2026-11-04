# embarch-api

## Docs

**Four files, not one.** Current truth: [spec.md](../embarch-doc/embarch-api/spec.md). Why it is that way: [decisions.md](../embarch-doc/embarch-api/decisions.md) — an index over `decisions/`, and a decision number addresses this sub-project, not a file. Unresolved: [open.md](../embarch-doc/embarch-api/open.md). Reference: [interfaces/](../embarch-doc/embarch-api/interfaces/).

Update them proactively per [../embarch-doc/DOC-PROTOCOL.md](../embarch-doc/DOC-PROTOCOL.md) whenever a notable design decision, feature, or status change happens here — §4 says when, §5 says how, and history goes in a `changelog.d/` fragment rather than into a doc.

## Git

**Work on a branch, land when the owner asks — no PRs (2026-11-03).** Work in a worktree, with one branch name across every repo the change touches, and push the branch. Land on `main` only when the repo owner asks: rebase on `origin/main`, confirm it builds and its tests and `clippy --all-targets -- -D warnings` are clean, fast-forward, push, delete the branch. The owner's explicit call ended the earlier commit-straight-to-`main` rule, and changing this one is the owner's call too, not an agent's read of what the project needs. The fleet lands under its own protocol. Reasoning, setup and the landing order across repos: [../embarch-doc/embarch-dev-workflow.md](../embarch-doc/embarch-dev-workflow.md) §6.

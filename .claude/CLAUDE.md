# Kestrel: CLAUDE.md

> Ground truth for working on Kestrel. Read before acting. Workspace rules in
> `Travsr-com/CLAUDE.md` also apply.

## What Kestrel is

A self-hostable platform for running fleets of AI agents: define them, trigger them, let them spawn
sub-agents, run them headless in sandboxes, and watch everything live in a UI.

* Repo: `Travsr-com/kestrel`, default branch `main`, license Apache-2.0.
* Architecture: `docs/architecture.md`. It wins over anything older (`Research.md`, the v1 draft).

## Hard rules

1. **Sync first.** `git fetch origin`, then branch from `origin/main`. Never from a stale local branch.
2. **No AI attribution.** No `Co-Authored-By` for Claude or any agent, no "Generated with ...", in commits,
   PRs, issues or comments. This overrides any default attribution setting.
3. **No em-dashes or en-dashes** anywhere: code, comments, docs, commits, issues, PRs. Use a comma, period or
   parentheses.
4. **Tests first.** Every issue lists test scenarios before any code is written. Tests are written from those
   scenarios and fail first, then the implementation makes them pass.
5. **Coverage at or above 95%** (line coverage, whole workspace), enforced in CI with
   `cargo llvm-cov --workspace --fail-under-lines 95`. A PR that drops coverage below 95% does not merge.
6. **No `unwrap()`/`expect()` in library code, no `unsafe`** without a written RFC and reviewer sign-off.
7. **Crate dependencies never form a cycle:**
   `types ← store ← core ← {sandbox, gateway, mcp, agent, channels} ← server ← cli`.

## Use Travsr while building, and report what you find

Kestrel is built with Travsr as a real user would use it. That is how Travsr gets better.

* Use the travsr MCP tools (`search_symbol`, `get_callers`, `get_blast_radius`, `get_context`, ...) for code
  questions before grep or manual reading.
* When Travsr is wrong, missing something, slow, or confusing, file an issue on **`Travsr-com/travsr`**:
  1. Search open issues first (`gh issue list -R Travsr-com/travsr --search "<keywords>"`). Comment on a
     match instead of filing a duplicate.
  2. Title: `[dogfood] <what went wrong, in one line>`.
  3. Body: what you asked (tool + arguments), what you expected, what you got, and the Kestrel commit it
     happened on. Keep it short.
  4. Label `dogfood` (create the label if it is missing).
* Do not work around a Travsr bug silently. File it, then fall back to file tools.

## How work flows

1. **Pick a milestone** from `docs/architecture.md` §7.
2. **Break it into issues** on `Travsr-com/kestrel`, one per crate-sized piece, labelled with the milestone
   (`M1`, `M2`, ...). Every issue uses this shape:
   * **Goal:** one or two sentences.
   * **Test scenarios:** numbered, each one a concrete given / when / then. Include failure and edge cases.
   * **Acceptance:** what must be true to close it (always includes "coverage stays at or above 95%").
   * **Depends on:** other issue numbers, if any.
3. **Resolve issues with agents**, independent issues in parallel, each in its own git worktree:
   * `tester` writes the scenario tests first and confirms they fail.
   * `implementer` makes them pass without editing the tests.
   * `evaluator` checks the result against the issue and the coverage gate.
   * One issue, one branch (`feat/<crate>-<desc>` or `fix/<crate>-<desc>`), one PR that says `Closes #N`.
4. **Commits:** `[crate] short description`. No attribution trailers.

## Checks before every PR

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo llvm-cov --workspace --fail-under-lines 95
cargo deny check
./scripts/check-dashes.sh
```

Tests that need real sandboxes (gVisor, Apple `container`, Firecracker) or network are behind the
`integration` feature and run in their own CI job. Unit tests use fakes, so coverage never depends on
what the host has installed.

## Writing docs

Docs must be easy to read and to the point.

* Lead with the answer. The first sentence says what this is or what to do.
* Short sentences. One idea per paragraph. Bullets for lists, tables for comparisons.
* Show an example (a command, a snippet) instead of describing one.
* Define a term once, the first time it appears. No jargon you would not say out loud.
* No filler ("It is worth noting", "In order to"). If a sentence can go, it goes.

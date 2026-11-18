---
name: orchestrate
description: Run any ShareCtrlScreen feature or fix as parallel agent tasks — design review first, then one builder and one reviewer per task in its own worktree, every build through the build lane, a merge gate on Windows and WSL, merge to main, cleanup, release. Use when asked to orchestrate, build, continue or check on planned work.
---

# Orchestrating work as agent tasks

You are the **orchestrator**. You write no product code and you do not review code yourself
(the one hands-on exception is in §7). You turn a plan into tasks, keep every agent moving,
run the merge gate through the lane, merge, release, reclaim disk, and keep the plan's status
true. The standards every agent works to are in `CLAUDE.md` → "How we work"; this skill is
the process that enforces them.

**Where the work is defined.** Each body of work has a plan under `plans/` (for example
`plans/05-TASKS.md` for the macOS/Linux port, `plans/08-MULTI-SESSION-TABS.md` +
`plans/08a-STRUCTURE-AND-FLOWS.md` for tabs). The plan names its tasks (IDs like `P2`, `D11`,
`M15`), their dependencies, sizes and acceptance lines, and keeps a status table.
`plans/00-DECISIONS.md` and a plan's own "Decisions" section override everything, including
this file. **The trunk is `main`.**

## The process

0. **Design before code.** A new feature gets a plan and a structure document (layering,
   types, contracts, state machines, every flow, invariants) **before** any task starts. The
   structure is reviewed by one independent **adversarial** agent told to break it; the
   orchestrator verifies every finding against the code and folds the accepted ones back in.
   Builders then implement *against* that structure; changing a contract in it needs the
   orchestrator's approval (§8).
1. Every task is **researched, developed, tested, bug-hunted, fixed and committed** by one
   **builder** agent in **its own git worktree on branch `task/<ID>`**, created from the tip
   of `main`. Production grade, long term: no patches "to get it working", no shortcuts, no
   stubs.
2. **Every ready task starts.** A task is ready when its last dependency is merged. Machine
   load is bounded by the **lane** (§2), not by agent counts.
3. **The builder finishes, then idles.** Its final report is the hand-off.
4. **Every task gets its own independent reviewer** (the strongest available model), spawned
   when the builder reports done. It reviews, returns APPROVE or CHANGES, and re-reviews every
   fix round.
5. CHANGES go back to the **same builder**; the fixed work goes back to the **same reviewer**.
   Loop until APPROVE. The orchestrator relays; it does not judge the code.
6. APPROVE → rebase onto `main` → **gate** through the lane on Windows and WSL (§5) → green →
   `git merge --ff-only` → every running builder is told to rebase.
7. A merged task is **cleared**: worktree, branches, Windows and WSL caches, its lane files.
8. Ship: version bump + tag when the owner wants a release (§6).
9. **Mark progress as you go**: every finished task/phase is marked complete in its plan (with
   the commit hash and anything device-pending) in the same commit that finishes it.

**Feature-branch mode.** For a large feature the owner may ask for one long-lived branch
(`feature/<name>`) off `main` instead of merging each task to `main`. Then: tasks branch from
and fast-forward into the feature branch (not `main`); each task is still gated and reviewed;
nothing is released until the end, when a final review of the whole feature runs, the branch
merges to `main`, and the release is cut and tested. **Orchestrator-as-builder:** the owner may
also ask the orchestrator to write the code itself; reviews then still come from an independent
reviewer agent, and the orchestrator verifies every finding against the code before acting.

## 0. Session start

1. `git status --short` clean on `main`; `git fetch`; local `main` equals `origin/main`.
   `git worktree list` shows only live tasks (an unmerged worktree is an unfinished task —
   resume it, §9).
2. Read the plan's status table — the single source of truth for what is done.
3. Lane up: `powershell -NoProfile -ExecutionPolicy Bypass -File .claude\lane\lane-up.ps1`
   (idempotent). The guard hook must be active: a raw `cargo` in any shell call is refused.
4. Base caches: Windows `native/target` and WSL `$HOME/sct/base`. If either is missing, queue
   a `BASE` request (`cargo test --workspace`) on that side first.
5. `rustup target list --installed` includes `aarch64-apple-darwin` (macOS type-checks).

## 1. Picking tasks and models

Ready = every dependency merged. Start design-risk and long tasks first so their reviews land
first. Builders: the strongest model for engine, protocol, platform-api, concurrency, codec,
security or cross-platform work; a lighter model only for mechanical work (docs, CI, simple
UI). Reviewers: always the strongest model. If a lighter model's safety filter refuses code
that looks like input injection or screen capture, re-run on a stronger model with the same
brief. Never reword a brief to evade a filter.

## 2. The lane — the only way cargo runs

`.claude/lane/README.md` is the contract. A request file `queue/<TASK>-<nn>.request` (header:
`task:`, `side: windows|wsl`, `cwd:`, optional `priority: gate`; then `---`; then one
cargo/npm/npx command per line). The drain runs a few requests at a time in the request's
worktree and writes `done/<name>.result` plus step logs under `logs/`. Gate requests run
first. A step silent for 30 min is STUCK; over 3 h is TIMEOUT. Request numbers are **never
reused** (a reused name overwrites the old result).

The orchestrator watches `done/` (a Monitor on new `.result` files, re-armed when it expires)
and resumes the owning agent with the result path. Nobody edits a worktree while its request
is queued or running.

**Gate commands** (both sides unless noted):
`cargo fmt --all` (writing form first, so the lane formats in place), `cargo fmt --all --check`,
`cargo check --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, `cargo test -p engine --test loopback --features test-hooks` ×3.
Windows side adds `cargo deny check` when dependencies changed, `cargo build -p app` when app
or Windows behaviour changed, and `cargo check` + `cargo clippy -p platform-macos --target
aarch64-apple-darwin --all-targets -- -D warnings` when macOS code changed.
`cargo check --workspace` (no `--all-targets`) builds without dev-dependencies, which is what
ships; it is not redundant.

## 3. Launching a builder

**Create the worktree yourself** from the current tip and hand the builder its path — do not
rely on the harness's `isolation: "worktree"`, which can base the worktree on a stale
`origin/main` (a builder once concluded merged work "did not exist" because of that):

```
git fetch && git worktree add -b task/<ID> .claude/worktrees/<id> main
```

Then `Agent`, `subagent_type: "general-purpose"`, model per §1, background. The brief:

```
TASK <ID> — <title>

You are a BUILDER for ShareCtrlScreen. Your worktree is <path> on branch task/<ID>,
based on main @ <hash>. `cd` there; do not create another worktree. Confirm the code the
spec relies on is present (<a file/symbol or two>); if it is not, STOP and report — it is a
base problem, not a missing feature. Commit to task/<ID> only. Never push, merge, rebase or
touch other branches; never edit outside your worktree; never message other agents or
sessions.

BUILDING — the lane. You never run cargo/rustc/npm yourself (a hook refuses them). Write
  C:\Yanivs\shareCtrlScreen\.claude\lane\queue\<ID>-<nn>.request   (nn = 01, 02, … never reused)
in the format of .claude/lane/README.md (cwd: <path>\native), then poll
.claude\lane\done\<ID>-<nn>.result until it appears; read it and the step logs it names; fix;
write <nn+1>. Keep requests small while iterating (`cargo check -p <crate>`), full at the end.
macOS cannot run here: type-check with `--target aarch64-apple-darwin`. Linux: side: wsl.

Read before writing code:
1. CLAUDE.md — "How we work" is binding.
2. plans/00-DECISIONS.md and the plan's Decisions section — they override everything.
3. Your task's entry in <plan>: its Deliverable and Acceptance lines are binding.
4. <structure doc>, if the plan has one: implement against its contracts; a contract change
   is a STOP-and-report.
5. The platform-api trait docs you touch — each invariant there fixed a real bug.
6. The surrounding code, for its conventions.

<task-specific lines: what merged since the plan was written, files to reuse, pitfalls>

Your LAST requests must PASS the full gate (both sides; see .claude/skills/orchestrate §2).
Then update the plan's status row to "in review" with an "As built" block, write
plans/checklists/<ID>.md for anything device-pending, and commit everything as ONE commit
ending with the attribution line in your instructions.

If an acceptance test fails twice for the same reason, stop and report what you tried.

Your final message is the hand-off to an independent reviewer:
- worktree, branch, commit hash
- what you built (≤ 10 lines) and the sources you relied on
- the PASS .result paths
- what is NOT verified and needs a device, and exactly how to verify it there
- contract / platform-api change needed: the exact signature, or "none"
- open risks for the reviewer
```

## 4. Launching a reviewer (when the builder reports done)

`Agent`, strongest model, no isolation, background; one per task until merge. Give it the code
and the spec, **not** your conclusions. The brief:

```
REVIEW <ID> — <title>

You are the independent REVIEWER for task <ID>. Worktree <path>, branch task/<ID>, one
commit <hash>; the builder's report is below. You change no code and run no cargo (the lane
results and step logs are at <paths>). Never message other agents; your final message is your
verdict.

Read CLAUDE.md ("How we work"), the task entry in <plan>, <structure doc>, and the WHOLE
diff: `git -C <path> diff main...task/<ID>`. Verify claims against the code. Focus first on
<the 2–3 riskiest points of this task>.

Reject (CHANGES) for: a contract or platform-api change without approval; a deleted or
weakened invariant comment or test; unwrap/expect on OS or I/O results; TODO/stub/dead code;
code copied from AGPL/GPL projects (RustDesk, scrap, hwcodec); a dependency not allowed by
deny.toml; behaviour outside the task; an Acceptance line not met or not tested; a missing or
wrong `//! ## References` block; error messages a user could not act on; device-pending work
without plans/checklists/<ID>.md.

Verdict: APPROVE | APPROVE-WITH-NITS | CHANGES, then findings most-severe first, each
`file:line — what — concrete failure scenario — what to do`, severity blocker / should-fix /
nit. Blockers and should-fixes make it CHANGES. On "re-review from commit <hash>": check every
earlier finding and the new diff.
```

The orchestrator may review a **small** fix itself (a few lines, mechanical) instead of a new
reviewer; anything touching concurrency, protocol, security or a hot path gets the reviewer.

## 5. Gate and merge (orchestrator, after APPROVE)

1. `git -C <worktree> log main..HEAD --oneline` — only this task.
2. Rebase onto `main`. Conflicts only in `native/Cargo.lock`, a plan status table or another
   task's unrelated additions: resolve keeping both sides; anything else goes back to the
   builder (then the reviewer re-reviews). After resolving, re-read the merged code for
   **semantic** conflicts (two features registering the same hook, two handlers for one event).
3. Queue the gate (§2), `priority: gate`, on both sides, `cwd` = the worktree. A FAIL goes to
   the builder with the result path.
4. `git merge --ff-only task/<ID>` into `main`; status row → done with the commit; commit;
   tell every running builder "main moved to <hash>: rebase before your next request".
5. **Clear the task**: retire builder and reviewer; `git worktree remove --force <path>`
   (retry or `rm -rf` + `git worktree prune` if busy); `git branch -D task/<ID>`; in WSL
   `rm -rf $HOME/sct/<ID>`; delete the task's `done/` and `logs/` files. Do this **per task**,
   not at the end — per-task WSL caches are ~12 GB each.
6. Newly unblocked tasks launch immediately.

**Pull requests from other people** are merged the same way: fetch each PR head, merge them
in a deliberate order into one branch (keeping their commits), resolve conflicts, open one PR
so CI runs on the combination, and only then merge. A PR that edits code the release does not
build (for example the legacy `app/`) is redone where it belongs.

## 6. Releasing

1. Bump `[workspace.package] version` in `native/Cargo.toml` **and** the matching workspace
   entries in `native/Cargo.lock` (the release builds with `--locked`); commit; push `main`.
2. Tag `v<version>` and push the tag; `release.yml` builds and publishes every OS and the
   updater feed. Watch the run to the end and report the result, per job.
3. If the licence gate fails on a yanked crate, `cargo update -p <crate>` through the lane,
   re-verify `cargo deny check`, and cut the next version (never move a pushed tag).
4. If a tag push is refused by the session's permissions, hand the owner the exact command.
5. Server changes deploy on push to `main` when `server/package.json`'s version changes;
   confirm the `server-v<version>` release exists afterwards.

## 7. Small direct work

The orchestrator may do mechanical, low-risk edits itself (a version bump, a status row, a
one-line formatting fix the gate found, a merge-conflict resolution). Anything with design
content goes to a builder.

## 8. Contract changes (`platform-api`, structure-doc contracts)

Frozen during a build. When a builder reports a needed change: pause that task; make it
yourself only if trivial and purely additive (a defaulted trait method), otherwise give it its
own builder + reviewer; gate; merge; tell every running builder to rebase; record it in the
plan's status notes.

## 9. Unsticking and resuming

- A builder that stopped mid-gate ("forced hand-back") is the common case: check `done/` for
  its result; if the request never landed in `queue/`, `running/` or `done/`, **queue it
  yourself** with a new number and resume the builder with the result.
- A builder whose own background watcher could race your merge: stop that watcher and own
  the result yourself.
- A lane result that never arrives: check `running/` and `logs/lane.log`; restart the drain
  with `lane-up.ps1`.
- A gone agent is replaced by a new one on the same branch, with the brief plus "continue from
  commit <hash>; previous report: …". Never two builders on one branch.
- Two rejected rounds on the same finding: escalate the builder's model with the full history.

## 10. Hardware hand-off and reporting

Device behaviour is never "done" until a log from a real device backs it; each such task has
`plans/checklists/<ID>.md`. When the owner reports a problem, read the device's `app.log`
first (Windows `%APPDATA%\ShareCtrlScreen\app.log`; macOS `~/Library/Logs/ShareCtrlScreen/app.log`;
the controlled machine's log is often reachable directly) and diagnose from evidence before
changing anything.

After each merge batch, one message to the owner: what merged and released, what was rejected
and why, what is device-pending and what the tester must do, and any decision needed, with
options and a recommendation. Never call something done that the gate did not pass.

## 11. Pitfalls already paid for

- `cargo check --target …` of the *whole workspace* from Windows fails in `aws-lc-sys`;
  per-crate checks of `platform-macos`/`platform-linux` work.
- A tab/view/hook added by two merged PRs can register twice; Tauri keeps every
  `on_window_event` handler, so check they act on disjoint windows.
- rustfmt orders `mod` lines; a hand-resolved conflict can fail `fmt --check` on every OS.
- The lane guard refuses commands *containing* the word `cargo` — including a `grep` for it or
  a job name like "Licence gate (cargo deny)". Match indirectly.
- PowerShell 5.1 reads BOM-less UTF-8 as ANSI; lane scripts are ASCII + BOM.
- A `wsl` command launched from a harness background job stalls; the lane's detached worker
  runs WSL steps fine.
- WSL `git` leaves phantom changes in Windows `git status` until the next Windows-side
  `git status`; LF→CRLF warnings are harmless.
- Deleting WSL caches frees space inside WSL only; the `ext4.vhdx` keeps its size until
  `wsl --shutdown` + an elevated `diskpart` `compact vdisk` (the owner runs it).
- Launching the debug app re-points the Start-with-Windows entry.
- `service` is Windows-only with a stub `main` elsewhere.

---
name: fold-fixups
description: Fold fix-up commits on an UNPUSHED branch into the commits they correct, so the history a reviewer sees has each change done right the first time — how to decide what folds where (blame-based plan, split mixed fixes, keep pre-existing-bug fixes standalone), how to do it non-interactively in this harness (amend! commits + --autosquash with editor env vars), how the target's message gets rewritten, and the end-tree identity check that proves nothing was lost. Use after a review round produced "fix(x): ..." commits on top of the feature commits they patch, before asking for review.
---

# Folding fix-ups into the commits they fix

A review round leaves a branch shaped like `feat A, feat B, fix A, fix A+B, test A`. A
reviewer reading that has to hold the bug in their head across three commits. The history
they should read is `feat A', feat B'`: each commit as if it had been right from the start.
This skill turns the first shape into the second without changing a byte of the final tree.

**Only for commits nobody else has.** Rewriting pushed history is out of the question; the
first step checks. And per this repo's rules: never push the result (the owner pushes), never
`--no-verify` — but note that `git rebase` runs no pre-commit hook at all, which is why the
final verification below is not optional.

Everything here is non-interactive. `git rebase -i` in this harness cannot open an editor,
so the todo list and every message are supplied through `GIT_SEQUENCE_EDITOR` /
`GIT_EDITOR` scripts (shipped in `scripts/`). Git ≥ 2.32 is needed for `--fixup=amend:`.

## 0. Preconditions

```bash
git status --short | grep -v '^??'          # must be empty: clean tree, nothing staged
test -d "$(git rev-parse --git-path rebase-merge)" && echo "rebase in progress — abort or finish first"
# (not REBASE_HEAD: that ref lingers after a finished rebase and says nothing)
BASE=main                                     # the commit the branch grows from
git branch -r --contains "$(git rev-list --reverse $BASE..HEAD | head -1)"   # MUST print nothing
```

If the last line prints a remote branch, the oldest commit is already pushed: stop. Folding
is only for the unpushed part; pick a later `BASE` that excludes what was pushed, or do
nothing.

## 1. Backup ref, always

```bash
git branch "backup/$(git branch --show-current)-pre-fold-$(date +%Y%m%d)"
```

The whole procedure is judged against this ref at the end (`git diff` must be empty), and
`git reset --hard backup/...` is the way out of anything that goes wrong halfway.

## 2. Plan: what folds where

```bash
python3 .claude/skills/fold-fixups/scripts/fold-plan.py $BASE          # summary per commit
python3 .claude/skills/fold-fixups/scripts/fold-plan.py $BASE --hunks  # per-hunk provenance
```

For every commit on the branch it blames the lines each hunk touches at the commit's
parent and says which earlier commits they came from. Read it with the commit's INTENT in
mind — blame is evidence, not the verdict:

- **A fix whose lines come from branch commits** folds into those commits. Several targets
  → split it (step 3b).
- **A fix whose lines predate the branch** corrects a bug that was on `main` already. It
  stays a **standalone commit**, and this repo orders such fixes BEFORE the feature commits
  (they are independently useful and revertable). If one commit carries both kinds of hunk,
  split it: fold the branch-fixing hunks, keep the rest as the standalone fix.
- **The question is "whose claim does this correct?", not only "whose lines".** A fix that
  edits old lines because a branch commit made them load-bearing (a gate a feature commit
  started relying on, a doc a feature commit made wrong) folds into that feature commit. A
  commit that fixes a latent `main` bug the feature merely exposed stays standalone.
- **Tests pinning a branch commit's behaviour** fold into that commit; a test-infrastructure
  change (a fixture cache, a proxy) folds into the commit that first needed it.
- **A feature commit** touching old lines is just a feature commit. Nothing to do.
- **Do not fold across a merge commit**, and do not fold a fix into a commit that is itself
  being kept as a reviewable standalone fix of `main` unless the fix is of that fix.

Write the plan down before touching git: `fix F → target T (whole)`, `fix G → T1 (hunks
a,b) + T2 (hunk c)`, `fix H → standalone, moves before the features`. Then write the NEW
full message for every target that changes (step 3c) — that is most of the real work.

## 3. Turn each fix into `amend!` commits

`git rebase --autosquash` folds a commit whose subject is `amend! <target subject>` into
its target and **replaces the target's message with the amend! commit's body** — no
concatenated "fix:" paragraphs, no editor. So every fix becomes one or more `amend!`
commits, then one non-interactive autosquash does the rest, in the right order, whatever
sits between fix and target.

### 3a. A fix that folds whole into one target

Stop the rebase at the fix, re-commit it as an amend! of the target, continue:

```bash
S=.claude/skills/fold-fixups/scripts
FIX=<sha>; TARGET=<sha>
printf '%s\n' "<new full message for TARGET>" > /tmp/msg-$TARGET     # subject line, blank, body

GIT_SEQUENCE_EDITOR="$S/todo-edit.sh edit $FIX" GIT_EDITOR=true git rebase -i $BASE
git reset -q --soft HEAD^
AMEND_SUBJECT="$(git log -1 --format=%s $TARGET)" AMEND_MESSAGE=/tmp/msg-$TARGET \
	GIT_EDITOR=$S/amend-editor.sh git commit --fixup=amend:$TARGET
GIT_EDITOR=true git rebase --continue          # no -q here; replays what followed the fix
```

(`--fixup=amend:` refuses `-m`; the editor script writes `amend! <subject>` plus your
message, which is the shape autosquash expects.)

### 3b. A fix spanning several targets

Same stop, but stage per target between the reset and each commit:

```bash
GIT_SEQUENCE_EDITOR="$S/todo-edit.sh edit $FIX" GIT_EDITOR=true git rebase -i $BASE
git reset -q --mixed HEAD^                      # changes back in the working tree, unstaged
git add <files that belong to T1>               # whole files when the split is by file …
printf 'y\nn\ny\n' | git add -p -- <mixed-file>  # … hunk answers when one file serves two targets
git diff --cached --stat                        # look before you commit
AMEND_SUBJECT="$(git log -1 --format=%s $T1)" AMEND_MESSAGE=/tmp/msg-$T1 \
	GIT_EDITOR=$S/amend-editor.sh git commit --fixup=amend:$T1
git add -A                                      # the rest
AMEND_SUBJECT="$(git log -1 --format=%s $T2)" AMEND_MESSAGE=/tmp/msg-$T2 \
	GIT_EDITOR=$S/amend-editor.sh git commit --fixup=amend:$T2
GIT_EDITOR=true git rebase --continue
```

`git add -p` takes piped answers (`y`/`n` per hunk in the order `git diff` lists them, `s`
to split a hunk with context inside it); list the hunks first with
`git diff -U3 -- <file> | grep '^@@'` and check `git diff --cached` before each commit. A
hunk that belongs to a standalone fix is left unstaged and committed last with an ordinary
message (`git commit -F`), not as an amend!.

Two fixes for the same target: two `amend!` commits, or one — the LAST amend! body wins
the message, so put the final message on the last one.

### 3c. The message is the deliverable

The target's new message describes the commit as it now is. Delete the narrative of the
bug that no longer exists ("the previous version wiped every thumbnail" — there was no
previous version). Keep rationale that is still true — the fix's commit message usually
holds a sentence or two of *why* that belongs in the target's body. Conventional Commits,
no agent metadata, same as any commit here.

## 4. Fold

```bash
GIT_SEQUENCE_EDITOR=true GIT_EDITOR=true git rebase -i --autosquash $BASE
```

`true` accepts the generated todo (autosquash has already moved each `amend!` behind its
target as `fixup -C`) and every message as given. A conflict means a commit BETWEEN the
fix and its target changed the same lines: resolve it as the TARGET should read (the
in-between commit's additions do not belong here yet), `git add`, `GIT_EDITOR=true git
rebase --continue` — and expect the in-between commit to conflict on the same lines when
it is replayed next, where the resolution is the other way round (its additions on top
of the new text). Two resolutions per shared region is normal; the identity check at the
end proves they cancelled out. If the conflict is in code the fix was not about, the plan
was wrong — `git rebase --abort`, `git reset --hard backup/...`, re-plan.

The commits made at the `edit` stops in step 3 are ordinary commits and DO run the
pre-commit hook — budget a hook run per amend! commit; it is the folding rebase itself
that runs none.

Standalone fixes that should move before the features: one more pass with a todo script
that reorders lines, or simply `git rebase -i` with `GIT_SEQUENCE_EDITOR` set to a `sed`
that moves the line; verify by end-tree identity as below.

## 5. Verify — every time

```bash
git diff --stat backup/<branch>-pre-fold-<date> HEAD | cat   # MUST be empty
git log --format='%h %s' $BASE..HEAD | grep -E '^\S+ (amend|fixup|squash)!' # MUST be empty
git log --format='%h %s' $BASE..HEAD | cat                    # the shape you planned
git show --stat <each rewritten commit>                       # each one is one thing
```

An empty diff against the backup is what makes this a history edit and not a code change.
Then, because the folding rebase ran no hooks on the commits it produced:

```bash
./scripts/git-hooks/pre-commit         # the commit-time battery, on the final tree
```

Optional, for a stack where every commit should build on its own (a reviewer stepping
through it): `GIT_SEQUENCE_EDITOR=true git rebase -i -x 'cargo clippy -p <crate> --
-D warnings' $BASE` — costly, so only when the stack is meant to be bisectable.

## 6. Recovery

Anything wrong: `git rebase --abort` if one is in progress, then
`git reset --hard backup/<branch>-pre-fold-<date>`. The backup ref stays until the owner
deletes it; say which ref it is in your report.

## scripts/

- `fold-plan.py [BASE] [--hunks]` — the provenance summary above. Read-only.
- `todo-edit.sh <action> <sha>[,<sha>] <todo>` — `GIT_SEQUENCE_EDITOR` helper turning
  `pick <sha>` into `edit`/`reword`/`drop` for the listed commits.
- `amend-editor.sh` — `GIT_EDITOR` helper for `--fixup=amend:`; reads `AMEND_SUBJECT` and
  `AMEND_MESSAGE`.

All three were proven on a scratch repo before being written down: a fix split across two
targets, a fix in mid-history with a feature commit after it, both folded with rewritten
messages and a byte-identical end tree.

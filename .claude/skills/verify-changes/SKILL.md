---
name: verify-changes
description: >
    Run the project's own format / lint / typecheck / test commands after a code change and
    before calling it done. Detects the toolchain from the repo's manifest — Cargo.toml
    (cargo fmt --check, clippy, cargo test --lib), package.json (lint / tsc / test scripts),
    pyproject.toml (ruff, pytest), go.mod, Package.swift — runs what exists scoped to the
    files that changed, and fixes failures at the root cause instead of suppressing them.
    Use after every edit, refactor or single-line fix; skip only when the repo has no
    configured checks, and say so.
---

# Verify Changes

A change is not done until the project's own checks pass on it. Run them after **every**
modification, not once at the end of a multi-step task.

## Step 1: Detect the toolchain (once per repo)

List the manifests, then `Read` the one nearest the file you changed:

```bash
git ls-files 'Cargo.toml' 'package.json' 'pyproject.toml' 'go.mod' 'Package.swift' \
  '*/Cargo.toml' '*/package.json' '*/pyproject.toml'
```

```
Read(file_path: "/abs/path/to/<manifest>")   # cargo aliases / "scripts" / [tool.*] sections
```

| Manifest | Format | Lint | Types | Tests |
|-|-|-|-|-|
| `Cargo.toml` | `cargo fmt --check` | `cargo clippy --all-targets` | (clippy covers it) | `cargo test --lib` |
| `package.json` | `prettier --check` | `eslint --max-warnings=0` | `tsc --noEmit` | `jest` / `vitest` / `bun test` |
| `pyproject.toml` | `ruff format --check` | `ruff check` | `mypy` / `pyright` if configured | `pytest` |
| `go.mod` | `gofmt -l` | `go vet ./...` | `go build ./...` | `go test ./...` |
| `Package.swift` | `swift-format lint` if configured | — | `swift build` | `swift test` |

Only run a tool the repo configures (`rustfmt.toml`/`clippy.toml`, `eslint.config.*`,
`tsconfig.json`, `[tool.ruff]`, a matching script). Nothing configured → skip verification and
say so; do not invent a command.

For JS the lockfile picks the runner: `bun.lock*` → `bun`, `pnpm-lock.yaml` → `pnpm`,
`yarn.lock` → `yarn`, else `npm`. Script names worth checking for in `"scripts"`:

| Check | Names seen in the wild |
|-|-|
| Lint | `lint`, `lint:check`, `eslint` |
| Types | `typecheck`, `type-check`, `tsc`, `check` |
| Test | `test`, `test:unit`, `test:run` |
| All-in-one | `check`, `verify`, `ci`, `validate` |

**If a combined `check`/`ci`/`verify` script or `justfile`/`Makefile` target runs the whole
battery, prefer it** over the pieces by hand.

## Step 2: Run the checks

Order: format → lint/types → tests. Scope to what changed, widen if the output points elsewhere.

```bash
git diff --name-only                      # what you touched
cargo clippy -p <crate> --all-targets     # narrow, then drop -p if it flags other crates
cargo fmt -p <crate> --check
cargo test -p <crate> --lib
npx eslint /abs/path/to/changed.ts --max-warnings=0
npx tsc --noEmit
npx vitest run <basename>
```

`cargo clippy`, never `cargo check`. `cargo test --lib` runs unit tests without dragging in
network/integration suites; name an integration target explicitly when the change warrants it.

No `cd`: use `cargo --manifest-path /abs/Cargo.toml`, `npm --prefix /abs run lint`, `git -C /abs`.
Never read or pass `.env*` to anything; if a check needs a secret from one, stop and ask.

## Step 3: Handle failures

- **Format** — run the repo's formatter for real (`cargo fmt -p <crate>`, `prettier --write`); an
  editor/agent auto-formatter is a different tool and loses to the repo's config.
- **Lint / clippy** — fix each finding. `#[allow(...)]`, `// eslint-disable`, `# noqa` only when
  the lint is provably wrong here, with a comment saying why.
- **Types** — fix the root cause. No `@ts-ignore` / `@ts-expect-error` / `# type: ignore`. An
  error in a file you did not touch usually means you changed a contract — follow it back.
- **Tests** — decide whether *you* broke it before touching anything:

  ```bash
  git worktree add /abs/tmp/verify-base <base-ref>
  cargo test --manifest-path /abs/tmp/verify-base/Cargo.toml --lib
  # or: npm --prefix /abs/tmp/verify-base test   (needs deps installed there)
  git worktree remove /abs/tmp/verify-base
  ```

  A worktree leaves your tree untouched — do **not** `git stash && test && git stash pop`, a
  conflicting pop strands it. A failure that pre-dates your change gets reported plainly, not
  masked. Never delete, skip or weaken a test to get green.

## Step 4: Report

```
✅ cargo fmt: clean
✅ cargo clippy: no warnings
✅ cargo test --lib: 128 passed
⏭️ typecheck: skipped (no TS in this repo)
```

`⏭️` for skipped with the reason, `❌` for failed. Do not call the task complete while any
check shows `❌`, and do not report a check as passing that you did not actually run.

## What not to do

- Do not claim "all checks pass" from a partial or scoped run — say what you ran.
- Do not suppress an error to make a check green.
- Do not stay scoped to one crate/file once the output implicates others.
- Do not skip verification because the edit was one line.
- Do not silently no-op in a repo whose toolchain you did not recognise — report that instead.

(`Grep` / `Glob` tools, if this session has them, can replace the `git ls-files` call.)

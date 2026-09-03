---
name: codebase-search
description: >
    Use before writing or reviewing code, calling any function/method you have not read, using any
    type you have not seen defined, adding an import, or implementing a pattern that may already
    exist. Also when asked how something works, or whether something exists. Default posture:
    search first, then write. Never invent paths, names, or APIs.
---

# Codebase Search — Search First, Then Write

## When to Search

Search before proceeding if:

- You're about to call a function or method you haven't read
- You're about to use a type, trait, interface, or schema you haven't seen defined
- You need an import/`use` path you're not certain about
- You're implementing something that likely has existing related code
- You're adding to a pattern (route, event, endpoint, component, test) and want to match existing ones
- You're unsure whether something already exists, or you're asked "how does X work?"

One search is always cheaper than code written against a wrong assumption.

## Search Toolkit — Use in Priority Order

Most precise tool first. Symbol-aware tools resolve through re-exports, barrel files, path aliases,
`pub use`, macros and renames — text search can't. Fall back to text search for what LSP doesn't
index (string literals, comments, config values, docs, SQL, generated code).

### 1. LSP — symbol navigation (highest priority for code)

If an LSP server exists for the file's language (rust-analyzer, tsserver, pyright, sourcekit-lsp,
gopls…), use the `LSP` tool for any symbol question:

| Question | LSP operation |
| --- | --- |
| Where is X defined? | `goToDefinition` |
| Where is X used? / Who calls X? | `findReferences`, `incomingCalls` |
| What does X call? | `outgoingCalls` |
| Type / signature of X? | `hover` (no need to open the file) |
| Symbols in this file / in the repo? | `documentSymbol` / `workspaceSymbol` |
| Implementations / impls of X? | `goToImplementation` |

After writing or editing code, check LSP diagnostics on the changed file. Don't ignore type errors
or missing imports.

### 2. Read a known file

For a single file whose path you know, use the `Read` tool — never `cat`/`head`/`sed -n`; it never
trips the permission rules below. Read the file you're about to edit top to bottom.

### 3. Text search — `git grep` with a pathspec

`git grep` searches tracked files only, so `node_modules`, `dist`, `target`, `build`, `.venv` and
any ignored `.env` are skipped for free. **Always give it an extension pathspec after `--`.**

```bash
# Definition of a symbol (mixed-language repo)
git grep -nE "fn my_function|export .*myFunction|def my_function" -- '*.rs' '*.ts' '*.py'

# All usages (':!' excludes a pathspec)
git grep -n "MyType" -- '*.rs' '*.ts' ':!*.d.ts'

# A type / struct / interface definition
git grep -nE "^(pub )?(struct|enum|trait) MyType|^(export )?(type|interface) MyType" -- '*.rs' '*.ts'

# A config key or constant, in code and in config files
git grep -niE "MY_CONSTANT|my_constant" -- '*.rs' '*.ts' '*.toml' '*.json' '*.yaml'

# Another repo: -C with an absolute path, never `cd`
git -C /abs/other/repo grep -n "PATTERN" -- '*.rs'

# Files by name (tracked)
git ls-files '*.rs' | grep -i auth
```

If the dedicated `Grep`/`Glob` tools are available in this session, they're equivalent and
sidestep the shell entirely — but never depend on them; `git grep` with a pathspec is the default.

### 4. Explore subagent — broad / open-ended

Use an `Explore` subagent (thorough setting) when 3 different patterns came up empty, when you need
to understand a feature spanning many files, or when the thing may be named nothing like you expect.

## Never Sweep a Directory That Contains a `.env`

The permission checker resolves a shell command's **file arguments** and matches them against the
`Read(.env)` deny rule. Flags do nothing — `--include`, `--exclude-dir`, `-t rust` are all ignored.
A directory argument with a `.env` anywhere under it (repo root, `web/`, `sandbox/`, …) turns the
command into a permission prompt every single time.

- ❌ `grep -rn X --include="*.rs" .` — directory `.` contains `.env` → prompt
- ❌ `rg -n X -t rust .` — same
- ❌ `git grep -n X` with no pathspec — treated as `.` → prompt (even `git grep --help`)
- ❌ any command that writes `.env` literally, even as `--exclude=.env` → denied outright
- ❌ `cd /abs/dir && git log abc..HEAD` / `cd /repo && grep -rn X crate/src` — after any `cd`, even
  to an absolute directory, the checker can't determine the working directory and treats every
  non-flag argument (a revision range included) as a path → prompt, whatever the target
- ✅ `git grep -n X -- '*.rs' '*.toml'` — an extension pathspec cannot match `.env`; tracked files only
- ✅ `grep -rn X --include="*.rs" /abs/repo/crate/src` — absolute path to a subtree with no `.env` under it
- ✅ one known file → the `Read` tool, not `cat`

Plain `grep -r` is only for **untracked** (not yet `git add`ed) files, and only against an absolute
subtree path — never the repo root, never `.`. Same for `find /abs/repo/<subtree> -name '*.rs'`.
Never `cd` inside a command; the Bash cwd persists between calls anyway. Never read `.env`,
`.env.*`, `.env.example` — if you need a value from one, stop and ask.

## Know the Repo — Read the Manifest

Which extensions to search and which tools exist come from the root manifest. `Read` it:

| Manifest | Pathspecs | Build · test · lint · format |
| --- | --- | --- |
| `Cargo.toml` | `'*.rs'` `'*.toml'` | `cargo build` · `test --lib` · `clippy` · `fmt --check` |
| `package.json` | `'*.ts'` `'*.tsx'` `':!*.d.ts'` | its `scripts` block, via npm/yarn/pnpm/bun run |
| `pyproject.toml` | `'*.py'` | `pytest` · `ruff check` · `ruff format --check` |
| `Package.swift` | `'*.swift'` | `swift build` · `swift test` · `swiftformat` |
| `go.mod` | `'*.go'` | `go build ./...` · `go test ./...` · `go vet` |

In a workspace (Cargo `[workspace] members`, npm `workspaces`, Go modules), read the member list
from that manifest rather than guessing directory names — and check sibling members before writing
something one of them may already have.

## Search Depth

**A first search not finding it does NOT mean it doesn't exist.** Things hide behind re-exports,
`pub use`, barrel files, aliases, macros, feature flags, or unexpected directories.

| Situation | Minimum searches |
| --- | --- |
| Using a function already read this session | 0 |
| Using a type/function not yet read | 2–3 searches with different patterns |
| Implementing a new feature | 5–8 across patterns, names, and dirs |
| Refactoring existing behavior | read every affected file + `findReferences` per symbol |
| "I don't think this exists" | 3+ varied terms before concluding |

### Strategies, in order

Exhaust LSP first (`goToDefinition` → `workspaceSymbol` → `findReferences`). Then, for text search:

1. **Exact name** — `git grep -n "my_function" -- '*.rs'`
2. **Fuzzy** — `git grep -niE "myfunc|my_func" -- '*.rs' '*.ts'` (casing, prefixes)
3. **Semantic variants** — `format|render|display`, `cache|store|persist`, `delete|remove|destroy`
4. **File name** — `git ls-files | grep -i foo` — maybe it's a whole file
5. **Directory scan** — `git ls-files '<member>/src/*.rs'` — browse the likely module
6. **Re-exports** — `mod.rs` / `lib.rs` / `index.ts` for `pub use` / `export … from`
7. **Usage search** — find how others consume it, then read one real call site

## Per-Scenario Cookbook

- **Using a function or type** — `goToDefinition` / `hover`, else `git grep`. Read the real signature, return type and error cases; never reconstruct one from context.
- **Adding an instance of a pattern** — find 1–2 existing ones and match their shape (a route handler, an API wrapper, a `#[test]`, a migration).
- **Creating something** — first check it doesn't exist: `git grep -niE "format_date|formatDate" -- '*.rs' '*.ts'`.
- **Importing** — verify the exported/`pub` item and its exact path (`workspaceSymbol`, or read the module's `lib.rs`/`mod.rs`/`index.ts`).
- **Third-party symbol** — read it in the dependency source (`~/.cargo/registry/src/…`, `node_modules/…`, the venv) instead of guessing its API.

## Never Do These

- ❌ Invent a function signature because it "seems right"
- ❌ Assume an import/`use` path without verifying it exists
- ❌ Reconstruct a type from context instead of reading its definition
- ❌ Assume a pattern is consistent without checking a real example
- ❌ Write code that calls into modules you haven't read
- ❌ Answer "does X exist?" — or conclude it doesn't — after one search
- ❌ Point `grep -r`, `rg`, `find | xargs grep`, or a bare `git grep` at a repo root
- ❌ Read or name any `.env*` file in a command

Code written without reading the codebase first is a guess. The codebase is the source of truth.

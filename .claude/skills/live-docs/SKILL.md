---
name: live-docs
description: >
    Use before writing or reviewing code that calls into any external library, API, or
    framework. Training data goes stale — identify the exact installed version from the
    project's manifest/lockfile (Cargo.toml, package.json, pyproject.toml, go.mod,
    Package.swift, ...), then fetch docs for that version before writing.
---

# Live Documentation Lookup

Training data goes stale. APIs change, options get renamed, majors break things. Look the
API up **before** writing the call, not after it fails to compile.

## Step 1 — Identify the ecosystem and the installed version

Which manifest exists tells you the ecosystem and the toolchain:

| Manifest | Ecosystem / tooling | Lockfile with exact versions |
| --- | --- | --- |
| `Cargo.toml` | Rust — `cargo build/test/clippy` | `Cargo.lock` |
| `package.json` | JS/TS — npm / yarn / pnpm / bun scripts | `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `bun.lock` |
| `pyproject.toml`, `requirements.txt` | Python — ruff / pytest / uv / poetry | `uv.lock`, `poetry.lock`, pinned requirements |
| `go.mod` | Go — `go build/test` | `go.sum` |
| `Package.swift`, `Podfile` | Swift — SwiftPM / CocoaPods | `Package.resolved`, `Podfile.lock` |
| `Gemfile` | Ruby | `Gemfile.lock` |
| `composer.json` | PHP | `composer.lock` |
| `pubspec.yaml` | Dart / Flutter | `pubspec.lock` |
| `*.csproj` | .NET / C# | `packages.lock.json` |
| `build.gradle(.kts)`, `pom.xml` | Java / Kotlin | `gradle.lockfile`, resolved pom |
| `mix.exs` | Elixir | `mix.lock` |

Read the manifest with the `Read` tool — one named file, absolute path, never `cat`:

```
Read(file_path: "/abs/repo/Cargo.toml")
Read(file_path: "/abs/repo/package.json")
```

Lockfiles are large; pull just the entry you need, with an explicit pathspec (never a bare
`git grep`, never `grep -r` at the repo root, never `cd`):

```bash
git grep -n -A 2 'name = "<crate>"' -- 'Cargo.lock'
git grep -n -A 2 '"<package>"' -- 'package-lock.json' 'yarn.lock' 'pnpm-lock.yaml'
```

Untracked or gitignored lockfile? Use an absolute path to that one file:
`grep -n -A 2 'name = "<crate>"' /abs/repo/Cargo.lock`. (The `Grep`/`Glob` tools are fine
here if this harness has them, but do not depend on them.)

**The version number matters.** The same library at v1 and v2 can be a different API. Look
up docs for the *installed* version, not "latest".

## Step 2 — Find the right documentation source

| Ecosystem | Registry | Version-pinned docs |
| --- | --- | --- |
| Rust | `crates.io/crates/<n>` | `docs.rs/<n>/<version>/` — always pinned, prefer over README |
| JS/TS | `npmjs.com/package/<n>` | docs linked from the package page; source at `unpkg.com/browse/<n>@<version>/` |
| Python | `pypi.org/project/<n>/<version>/` | project docs; stdlib at `docs.python.org/<major>.<minor>/` |
| Go | `pkg.go.dev/<module>` | `pkg.go.dev/<module>@<version>` |
| Swift | `swiftpackageindex.com/<org>/<repo>` | GitHub tag; Apple SDKs at `developer.apple.com/documentation/` |
| Ruby | `rubygems.org/gems/<n>` | `rubydoc.info/gems/<n>/<version>` |
| PHP | `packagist.org/packages/<vendor>/<n>` | linked GitHub/docs |
| Dart | `pub.dev/packages/<n>` | `pub.dev/documentation/<n>/<version>/` |
| .NET | `nuget.org/packages/<n>/<version>` | `learn.microsoft.com` |
| Java/Kotlin | `mvnrepository.com/artifact/<group>/<artifact>` | `javadoc.io/doc/<group>/<artifact>/<version>` |
| Elixir | `hex.pm/packages/<n>` | `hexdocs.pm/<n>/<version>` |

Universal fallbacks that are always version-addressable:
`github.com/<org>/<repo>/releases`, `github.com/<org>/<repo>/tree/v<version>`.

When version-pinned docs do not exist, verify the page you found matches the installed
version before trusting it.

## Step 3 — Fetch before you write

Search specifically: library name + version + the exact API or concept.

```
tokio 1.x spawn_blocking cancellation
serde 1.0 flatten with deny_unknown_fields
sqlalchemy 2.0 async session
django 5.0 middleware configuration
swift 6 sendable actor isolation
```

Unsure which version's behaviour you remember? Fetch the changelog or migration guide
first: `<library> migration guide v2 to v3`, `<library> breaking changes`, `<library> changelog`.

Fetch the specific page, not the homepage:

```
https://docs.rs/<crate>/<version>/<crate>/struct.<Name>.html
https://docs.djangoproject.com/en/5.0/topics/http/middleware/
https://pkg.go.dev/<module>@<version>#section-documentation
```

### What to look for

- **Signatures** — parameter names, types, order, required vs optional
- **Return types** — especially `Result`/`Option`, async, and error-carrying APIs
- **Breaking changes** since the version you remember — renamed params, changed defaults
- **Required setup** — imports, initialization, config files, **feature flags** (Rust
  crates hide entire modules behind them), build settings
- **Platform / runtime constraints** — OS support, minimum language version, wasm/no_std
- **Deprecations** — the old way may still compile and still be wrong
- **Official examples** — they beat inferred usage every time

## Step 4 — Apply what you found, not what you remember

1. Use the exact signatures from the docs; do not interpolate missing parameters.
2. Match the version — v3 docs for a v2 dependency is a bug waiting to happen.
3. Do the required setup steps (registration, feature flag, initialization).
4. Flag a significantly outdated or EOL dependency instead of silently coding against it.
5. State platform constraints you found (Linux-only, async-only, min toolchain).

## Always fetch for these — they churn

- **AI / LLM SDKs** — Anthropic, OpenAI, Gemini, LangChain: models and params move constantly
- **Cloud SDKs** — AWS, GCP, Azure: auth flows and service APIs
- **Mobile / platform APIs** — iOS, Android, and their cross-platform wrappers
- **Web framework routing & middleware** — majors break conventions
- **ORM / query APIs** — SQLAlchemy, Prisma, Diesel, sqlx, ActiveRecord, GORM
- **Auth libraries** — security-driven changes land fast
- **Build tooling & config formats** — Cargo, Gradle, Vite, Bazel, Webpack
- **Database drivers**, especially async ones — pool APIs evolve
- **Container / infra tooling** — Docker, Kubernetes, Terraform resource specs
- **Anything that recently shipped a major** — assume breakage until docs say otherwise

## When docs are unavailable or behind a login

1. `github.com/<org>/<repo>#readme`, then `/releases`.
2. Search with a site filter: `<library> <symbol> example site:github.com`.
3. **Read the installed source** — always authoritative. Use LSP `goToDefinition` /
   `hover` on the symbol; it lands in the vendored source (Rust:
   `~/.cargo/registry/src/...`; JS: `node_modules/<pkg>`; Python: `site-packages`).
   Read those files with the `Read` tool.
4. Still unverifiable — say so, and leave a comment naming the best URL you found:

```rust
// NOTE: could not verify the current API for X in <crate> v<version>.
// Confirm against docs before shipping: <url>
```

## What NOT to do

- **Don't skip the lookup because you're confident** — confidence is how stale APIs ship.
- **Don't fetch the homepage and call it done** — fetch the page for the API you're calling.
- **Don't trust the first search result** — check it documents the installed version.
- **Don't write first and look up docs to confirm** — look up first, write after.
- **Don't assume defaults are stable across majors** — defaults and behaviours change.
- **Don't assume a library behaves the same across languages** — `redis-py`, `ioredis`,
  and `redis-rs` expose the same Redis differently.

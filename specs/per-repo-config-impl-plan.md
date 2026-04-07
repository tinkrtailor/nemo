# Implementation Plan: Per-Repo Server Config

**Spec:** `specs/per-repo-config.md`
**Branch:** `per-repo-config`
**Status:** In Progress
**Created:** 2026-04-07

## Codebase Analysis

### Existing implementations found

| Component | Location | Status |
|-----------|----------|--------|
| Global engineer config | `cli/src/config.rs` (`EngineerConfig`, `load_config`, `save_config`) | Complete — needs wrap into new resolver |
| `nemo config` command | `cli/src/commands/config.rs` | Needs `--local`/`--global` + provenance display |
| `nemo init` command | `cli/src/commands/init.rs` | Needs `.nemo/.gitignore` seeding + root `.gitignore` append |
| Main resolver ad-hoc | `cli/src/main.rs:183-198` | Replace with `ResolvedConfig` |
| HTTP client | `cli/src/client.rs` | Unchanged — still reads API key from string |
| Control-plane runtime `nemo.toml` parser | `control-plane/src/config/mod.rs::NautiloopConfig` | Already tolerates unknown sections (no `deny_unknown_fields`) |
| Control-plane V1.5 `nemo.toml` parser | `control-plane/src/config/repo.rs::RepoConfig` | Has `deny_unknown_fields` — must be made tolerant of `[server]` |
| Terraform outputs | `terraform/modules/nautiloop/outputs.tf` + `main.tf` locals | Existing `server_url`/`api_key` outputs preserved; add `nemo_setup_instructions` |
| CLI integration tests | `cli/tests/` | Directory does not exist — will create |

### Patterns to follow

| Pattern | Location | Description |
|---------|----------|-------------|
| Atomic 0600 write | `cli/src/config.rs:62-93` (`save_config`) | Write `.tmp` with `OpenOptionsExt::mode(0o600)`, then rename |
| Masking API key | `cli/src/commands/config.rs:32-42` | `chars()`-based prefix/suffix mask |
| Inline `#[cfg(test)] mod tests` | Throughout | Unit tests alongside code |
| Serde default structs | `control-plane/src/config/repo.rs` | `#[serde(default)]` per field, Option for absent sections |
| Env var override | `cli/src/main.rs:187` (`NEMO_INSECURE`) | `std::env::var(...).as_deref()` matched against a set |

### Files to create

| File | Purpose |
|------|---------|
| `cli/src/config/mod.rs` | Convert `config.rs` → module; declare submodules |
| `cli/src/config/engineer.rs` | Existing `EngineerConfig`, `load_config`, `save_config` (moved) |
| `cli/src/config/sources.rs` | `ConfigSource`, `Resolved<T>`, `ResolvedConfig`, `resolve()`, repo detection |
| `cli/src/config/repo_toml.rs` | Minimal TOML reader for `[server] url` (CLI-side, does not import control-plane) |
| `cli/src/config/credentials.rs` | Read/write `<repo>/.nemo/credentials` (mode 0600 atomic, permission warning) |
| `cli/tests/per_repo_config.rs` | Integration test (`assert_cmd`-style) for resolution precedence |

### Files to modify

| File | Change |
|------|--------|
| `cli/src/config.rs` | Delete (moved to `cli/src/config/engineer.rs`) |
| `cli/src/main.rs` | Replace ad-hoc resolution with `ResolvedConfig::resolve()`; pass `--server` only |
| `cli/src/commands/config.rs` | Add `--local`/`--global` flags; use `ResolvedConfig` for display with sources; handle per-repo writes |
| `cli/src/commands/init.rs` | Seed `.nemo/.gitignore`, append `.nemo/credentials` to root `.gitignore` idempotently |
| `cli/Cargo.toml` | Add `tempfile` (dev-dependency) and `assert_cmd`/`predicates` for integration test |
| `control-plane/src/config/repo.rs` | Remove `deny_unknown_fields` from `RepoConfig` (or add tolerant `server` field); add test proving `[server]` parses without error |
| `terraform/modules/nautiloop/outputs.tf` | Add `nemo_setup_instructions` output (sensitive, human-readable copy-paste block) |

### Risks & considerations

1. **Backwards compatibility:** Existing users with global `~/.nemo/config.toml` only must see zero behavior change. Global becomes lowest-priority, but since no other layer exists yet for them, it still wins.
2. **Env beats CLI flag for URL?** Spec FR-4 says `--server` beats env. Must not swap order.
3. **`nemo init` is local-only** (runs before config loading): safe — only touches files in the cwd.
4. **Credentials file mode check** uses `std::os::unix::fs::MetadataExt::mode()` — unix only, so `#[cfg(unix)]`.
5. **First-invocation per-process warning (FR-15):** use a `std::sync::OnceLock<bool>` or a `once_cell`-style lazy; the CLI is a short-lived process so "per-process" means "once per resolution."
6. **Clap 4 migration of `config` subcommand:** currently has `--set` and `--get` only. Must add `--local` and `--global` as bool flags and keep backwards-compatible call shape (`nemo config --set key=value`).
7. **`tempfile` crate:** already in use transitively? Need to add explicitly to dev-deps.
8. **CLI integration test:** `assert_cmd` is a separate crate. Add as dev-dependency. Alternative: call `nemo`-as-library API directly — but binary-only crate makes this awkward. Use `assert_cmd::Command::cargo_bin("nemo")`.
9. **Running `nemo init` in an existing repo** with a populated root `.gitignore` must not duplicate the `.nemo/credentials` line — idempotent append.
10. **Spec text says "follow existing SSH private-key pattern"** for credentials: one file, one secret, mode-enforced.
11. **Identity-only-global enforcement:** `--local engineer=alice` must error cleanly without touching any file.
12. **Test isolation:** unit tests must not read real `$HOME` or cwd — use `tempfile::tempdir` + env var overrides (`HOME`, `NEMO_*`).

### Out of scope reminders (do NOT implement)

- Profiles (`[profiles.foo]`)
- Secrets backends (1Password, vault, keychain)
- Pre-commit hook to block committing credentials
- Deprecation warnings for global fallback
- Control-plane config resolution (only forward-compat tolerance change)
- Multi-engineer-per-repo config

## Plan

### Step 1: Control-plane forward-compat tolerance for `[server]` section

**Why this first:** Lowest-risk isolated change. Must land before anyone writes a `nemo.toml` with `[server]` or the control-plane V1.5 path will reject it. Also unblocks: the next steps introduce an integration test that expects a `nemo.toml` containing `[server]` to parse without error when loaded by the control plane.

**Files:** `control-plane/src/config/repo.rs`

**Approach:**
- Remove `#[serde(deny_unknown_fields)]` from `RepoConfig` struct, OR add an explicit ignored `#[serde(default, skip_serializing)] _server: Option<toml::Value>` style field. The cleaner option: remove `deny_unknown_fields` on `RepoConfig` (keep it on `RepoMeta`).
- Keep `deny_unknown_fields` on `RepoMeta` (that inner table is still strict) — this means unknown top-level sections are tolerated but unknown keys inside known sections still fail.
- Update `test_repo_config_unknown_fields` test: add a new test `test_repo_config_tolerates_server_section` that passes TOML with `[server] url = "..."` and asserts it parses successfully. Keep the existing strict-inner-field test for `RepoMeta`.

**Tests:** `cargo test -p nautiloop-control-plane config::repo`

**Depends on:** nothing
**Blocks:** Step 2

### Step 2: Convert `cli/src/config.rs` → `cli/src/config/` module with `engineer.rs`

**Why this second:** Foundation for the new source files. Zero behavior change but every later step adds files into `cli/src/config/`.

**Files:**
- Delete `cli/src/config.rs`
- Create `cli/src/config/mod.rs` (re-exports `engineer::{EngineerConfig, load_config, save_config, config_path}`)
- Create `cli/src/config/engineer.rs` (moved contents verbatim)

**Approach:** Mechanical move. `cli/src/main.rs` continues to use `use crate::config::...` identically.

**Tests:** `cargo build -p nemo-cli && cargo test -p nemo-cli` — confirms the move compiles.

**Depends on:** nothing
**Blocks:** Step 3

### Step 3: Add `cli/src/config/credentials.rs` — atomic 0600 read/write

**Why:** Independent testable unit. Used by resolver (Step 5) and `nemo config --local --set api_key=` (Step 7). Before the resolver lands so the resolver can import it.

**Files:** `cli/src/config/credentials.rs`

**Approach:**
```rust
pub fn read_credentials(repo_root: &Path) -> Result<Option<String>>;
// - returns Ok(None) if `.nemo/credentials` does not exist
// - returns Ok(Some(trimmed_line)) if present
// - on unix, stats file; if mode & 0o077 != 0, print stderr warning (still returns the key)

pub fn write_credentials(repo_root: &Path, api_key: &str) -> Result<()>;
// - creates `.nemo/` if missing
// - writes to `.nemo/credentials.tmp` with mode 0o600 via OpenOptionsExt (unix) / default (non-unix)
// - fsync-friendly: File::sync_all()
// - rename to `.nemo/credentials`
// - rejects empty api_key (anyhow::bail!)
```

Inline `#[cfg(test)] mod tests`:
- `test_write_then_read_roundtrip` (uses `tempfile::tempdir`)
- `test_write_rejects_empty_key`
- `test_read_missing_returns_none`
- `test_write_is_mode_0600` (unix-only, `#[cfg(unix)]`)
- `test_read_warns_on_loose_mode` (unix-only, captures stderr or skips assertion and just exercises the path)
- `test_write_trims_newline` (actually: write stores raw bytes; read trims. test is on read.)

**Tests:** `cargo test -p nemo-cli config::credentials`

**Depends on:** Step 2
**Blocks:** Step 5, Step 7

### Step 4: Add `cli/src/config/repo_toml.rs` — minimal `[server]` reader

**Why:** Small, isolated TOML parser for the new `[server] url` section. CLI-side only; does NOT depend on control-plane. Comes before the resolver so the resolver can consume it.

**Files:** `cli/src/config/repo_toml.rs`

**Approach:**
```rust
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RepoToml {
    #[serde(default)]
    pub server: Option<ServerSection>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerSection {
    pub url: Option<String>,
}

pub fn read_repo_toml(repo_root: &Path) -> Result<Option<RepoToml>>;
// - returns Ok(None) if nemo.toml absent
// - parses with toml::from_str, tolerates ALL other sections (no deny_unknown_fields)
// - on parse error: print stderr warning, return Ok(None)
// - returns Ok(Some(RepoToml)) otherwise

pub fn server_url_from_repo_toml(repo_root: &Path) -> Option<String>;
// convenience: read_repo_toml(...).ok().flatten().and_then(|t| t.server?.url)
```

Also add a `write_server_url` helper for Step 7:
```rust
pub fn write_server_url(repo_root: &Path, url: &str) -> Result<()>;
// - reads existing nemo.toml as toml::Value (preserving all other sections)
// - sets/creates [server] url = url
// - writes atomically (via .tmp + rename) with normal file mode
// - if nemo.toml does not exist, creates a minimal one containing only [server]
```

Inline tests:
- `test_read_missing_returns_none`
- `test_read_with_server_section_parses`
- `test_read_tolerates_unrelated_sections` (full nemo.toml with `[repo]`, `[services.x]`, `[server]`)
- `test_read_missing_server_url_returns_none`
- `test_write_creates_minimal_nemo_toml`
- `test_write_preserves_existing_sections` (write nemo.toml with `[repo]` first, then `write_server_url`, re-read and verify both sections present)
- `test_write_updates_existing_server_url`
- `test_read_returns_none_on_parse_error` (malformed TOML)

**Tests:** `cargo test -p nemo-cli config::repo_toml`

**Depends on:** Step 2
**Blocks:** Step 5, Step 7

### Step 5: Add `cli/src/config/sources.rs` — resolver with provenance

**Why:** Core of the spec. Consumes Steps 3 & 4; consumed by Step 6 (main.rs) and Step 7 (nemo config command).

**Files:** `cli/src/config/sources.rs`

**Approach:**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    CliFlag,
    EnvVar,
    RepoToml,        // <repo>/nemo.toml
    RepoCredentials, // <repo>/.nemo/credentials
    GlobalFile,      // ~/.nemo/config.toml
    Default,
}

impl ConfigSource {
    pub fn label(self) -> &'static str { /* "cli flag", "env var", "nemo.toml", ".nemo/credentials", "~/.nemo/config.toml", "default" */ }
}

#[derive(Debug, Clone)]
pub struct Resolved<T> {
    pub value: T,
    pub source: ConfigSource,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub server_url: Resolved<String>,
    pub api_key: Option<Resolved<String>>,
    pub engineer: Resolved<String>,
    pub name: Resolved<String>,
    pub email: Resolved<String>,
    pub repo_root: Option<PathBuf>,
}

/// Walks up from `start` looking for first ancestor containing `nemo.toml` OR `.git`.
/// Returns the repo root (dir that contains the marker), or None.
pub fn find_repo_root(start: &Path) -> Option<PathBuf>;

/// Resolves all config values with provenance. Reads env, repo root, files.
/// `cli_server` is the value of `--server` if given.
pub fn resolve(cli_server: Option<&str>) -> Result<ResolvedConfig>;
```

Resolution order (matches spec FR-4, FR-5, FR-6):
- **server_url:** CLI flag > `NEMO_SERVER_URL` > `<repo>/nemo.toml [server].url` > `~/.nemo/config.toml server_url` > default (`https://localhost:8080`)
- **api_key:** `NEMO_API_KEY` > `<repo>/.nemo/credentials` > `~/.nemo/config.toml api_key` > None
- **engineer / name / email:** `~/.nemo/config.toml` only

FR-15 implementation: track whether env OR repo source supplied the value AND global also had a different value, print a single stderr warning per process. Use a `std::sync::OnceLock<()>` or `AtomicBool` gate.

Repo detection (spec "Repo detection" section):
- `start = env::current_dir()?`
- Walk ancestors, stop at first dir containing `nemo.toml` OR `.git` (file OR directory — worktree support)
- Cache result by passing `repo_root` through `ResolvedConfig` (do not re-walk per field)
- Nested repo: nearest `nemo.toml` wins. If only `.git` found, repo_root is set but repo-scoped `nemo.toml` is absent → no repo-sourced server_url (still read `.nemo/credentials` if present? spec says `.nemo/credentials` is also repo-root-scoped; so yes, it's consulted)

Inline tests (use `tempfile::tempdir` and manipulate `HOME`, `NEMO_*` env vars; use `serial_test` or just avoid env mutation by passing resolution inputs explicitly via a test-only variant):
- `test_resolve_server_url_env_beats_repo_toml`
- `test_resolve_server_url_repo_toml_beats_global`
- `test_resolve_server_url_cli_flag_beats_env`
- `test_resolve_api_key_env_beats_repo_credentials`
- `test_resolve_api_key_repo_credentials_beats_global`
- `test_resolve_identity_only_from_global`
- `test_repo_detection_walks_up_from_subdir`
- `test_repo_detection_handles_worktree` (`.git` as a file)
- `test_repo_detection_returns_none_outside_repo`
- `test_resolve_no_repo_falls_back_to_env_plus_global`

**Test isolation strategy:** Extract a testable inner function `resolve_from(
    cli_server: Option<&str>,
    env_server: Option<String>,
    env_api_key: Option<String>,
    repo_root: Option<&Path>,
    global: &EngineerConfig,
) -> ResolvedConfig` — pure, no env/fs access. The public `resolve()` wraps it. This avoids env-var mutation in tests and makes precedence tests deterministic.

For `find_repo_root` tests, write real `nemo.toml` / `.git` markers in tempdirs.

**Tests:** `cargo test -p nemo-cli config::sources`

**Depends on:** Step 3, Step 4
**Blocks:** Step 6, Step 7

### Step 6: Wire `ResolvedConfig` into `cli/src/main.rs`

**Why:** Replaces the ad-hoc resolution at lines 183-198. Once this compiles and tests pass, all commands use the new resolver.

**Files:** `cli/src/main.rs`

**Approach:**
- Replace `let eng_config = config::load_config()?;` + `let server_url = cli.server.unwrap_or(eng_config.server_url.clone());` with `let resolved = config::sources::resolve(cli.server.as_deref())?;`
- Build `NemoClient::new(&resolved.server_url.value, resolved.api_key.as_ref().map(|r| r.value.as_str()), insecure)`
- For the `api_key.is_none()` bail check, consult `resolved.api_key.is_none()` — keep the same error message, updated to mention new options: `"API key not configured. Set NEMO_API_KEY, create <repo>/.nemo/credentials, or run: nemo config --set api_key=<your-key>"`.
- For commands needing engineer, check `resolved.engineer.value.is_empty()` — same logic.
- `Commands::Auth` currently reads `eng_config.engineer`, `name`, `email` — route through `resolved.engineer.value`, `resolved.name.value`, `resolved.email.value`.
- Keep the `NEMO_INSECURE` env var behavior unchanged.

**Tests:** `cargo test -p nemo-cli` plus a quick smoke: `cargo run -p nemo-cli -- --help` (just ensures it compiles).

**Depends on:** Step 5
**Blocks:** Step 7, Step 8, Step 10

### Step 7: Extend `nemo config` with `--local`/`--global` + provenance

**Why:** Spec requirements FR-7..FR-11 + FR-14.

**Files:**
- `cli/src/main.rs` — extend `Commands::Config { set, get, local, global }`
- `cli/src/commands/config.rs` — major rewrite

**Approach:**

Add new CLI flags:
```rust
Config {
    #[arg(long)]
    set: Option<String>,
    #[arg(long)]
    get: Option<String>,
    /// Write to the per-repo config (<repo>/nemo.toml or <repo>/.nemo/credentials)
    #[arg(long)]
    local: bool,
    /// Write to the global config (~/.nemo/config.toml)
    #[arg(long)]
    global: bool,
}
```

`commands::config::run`:
- If `local && global` → bail with "cannot use --local and --global together"
- Load `ResolvedConfig` via `sources::resolve(None)` (purely for display/get; for set, also derive the repo_root)
- `--get`:
  - `server_url` → `resolved.server_url.value`
  - `api_key` → masked value if present, `(not set)` otherwise
  - others → from `resolved.*`
- `--set key=value`:
  - Determine scope per spec table:
    - `--local` explicit:
      - `engineer|name|email` → error "identity is per-user; use --global or omit the flag"
      - `server_url` → `repo_toml::write_server_url(repo_root, &value)`, print `"Wrote [server].url to <repo_root>/nemo.toml"`
      - `api_key` → `credentials::write_credentials(repo_root, &value)`, print `"Wrote api_key to <repo_root>/.nemo/credentials (mode 0600)"`
    - `--global` explicit: forward to existing `save_config` logic. If key is `server_url` or `api_key`, print warning "note: server_url/api_key in the global file is the legacy fallback. Prefer `nemo config --local --set ...` in your repo."
    - Neither flag: auto-scope rules (per spec table)
      - Inside repo AND key ∈ {`server_url`, `api_key`} → local behavior
      - Otherwise → global behavior
  - If `--local` used but `find_repo_root` returns None → error "not in a repo; run from inside a repo or use --global"
- No flags (display mode): print each field with source
  ```
  Nemo CLI Configuration
    server_url: http://... (from nemo.toml)
    api_key:    abcd...wxyz (from .nemo/credentials)
    engineer:   alice (from ~/.nemo/config.toml)
    name:       ...
    email:      ...
  ```
  Uses the existing masking logic.

Inline tests (in `commands/config.rs`):
- Use tempdirs + env overrides (HOME = tempdir). For set-tests, use the testable-inner-function pattern; expose a `fn run_with_env(...)` so tests don't mutate global `HOME`.
- `test_set_local_server_url_writes_to_nemo_toml`
- `test_set_local_api_key_writes_credentials_file_mode_0600`
- `test_set_local_identity_key_fails_with_clear_error`
- `test_set_global_server_url_prints_warning` (capture via writer injection)
- `test_auto_scope_inside_repo` (server_url → local, engineer → global)
- `test_auto_scope_outside_repo` (all → global)
- `test_display_shows_source_per_field`

For the warning-capture test: refactor `run` so the inner helpers take `&mut impl Write` for stdout and stderr, or use a closure. Alternative: skip fine-grained stderr assertion and test the scope decision function directly (pure: `fn resolve_scope(key: &str, local: bool, global: bool, inside_repo: bool) -> Result<Scope>`). Prefer the pure decision function — easier and still covers FR-7..FR-11.

**Tests:** `cargo test -p nemo-cli commands::config`

**Depends on:** Step 5, Step 6
**Blocks:** nothing (independent of Step 8+)

### Step 8: Extend `nemo init` to seed gitignore entries

**Why:** Spec FR-12, FR-13. Protects users from accidentally committing credentials.

**Files:** `cli/src/commands/init.rs`

**Approach:**
After writing `nemo.toml`:
1. Create `<cwd>/.nemo/` if missing.
2. Write `.nemo/.gitignore` with contents `credentials\n` (only if missing — don't overwrite). This matches FR-12.
3. Read `<cwd>/.gitignore` if exists; otherwise empty string. If no line equals `.nemo/credentials` (trim & exact match), append `.nemo/credentials\n`. Write atomically. This matches FR-13.

Factor into helpers:
```rust
fn seed_nemo_gitignore(cwd: &Path) -> Result<()>;
fn append_root_gitignore_entry(cwd: &Path, entry: &str) -> Result<()>;
```

Inline tests:
- `test_init_seeds_nemo_gitignore`
- `test_init_appends_to_root_gitignore_when_missing`
- `test_init_does_not_duplicate_gitignore_entry` (pre-seed `.gitignore` with `.nemo/credentials`, run append, verify only one line)
- `test_init_preserves_existing_gitignore_lines`
- `test_init_nemo_gitignore_not_overwritten` (pre-create `.nemo/.gitignore` with custom content, run seed, verify unchanged)

For testability: factor helpers take `cwd: &Path` so they can be tested in a tempdir without chdir.

**Tests:** `cargo test -p nemo-cli commands::init`

**Depends on:** nothing (independent of resolver)
**Blocks:** nothing

### Step 9: Integration test — end-to-end resolution precedence

**Why:** Spec Test Plan "Integration test" section. Runs the built binary in a tempdir with realistic file layout.

**Files:**
- `cli/tests/per_repo_config.rs`
- `cli/Cargo.toml` (add `assert_cmd`, `predicates`, `tempfile` as dev-dependencies)

**Approach:**
Using `assert_cmd::Command::cargo_bin("nemo")`:
1. `let tmp = tempfile::tempdir()?;`
2. Write `tmp/nemo.toml` with `[server] url = "http://fake:1"`
3. `std::fs::create_dir(tmp/".nemo")?; fs::write(tmp/".nemo/credentials", "test-key-123\n")?; chmod 0600`
4. Also create a fake HOME with `~/.nemo/config.toml` containing an `engineer = "alice"` so identity is populated
5. Run `nemo config` with `HOME=<fake>` and `cwd=<tmp>` — assert stdout contains `http://fake:1` with source `nemo.toml` and `test-k...-123` with source `.nemo/credentials`.
6. Run again with `NEMO_SERVER_URL=http://env:2` — assert `http://env:2` with source `env var`.

This exercises the entire resolution pipeline including the binary entrypoint.

**Tests:** `cargo test -p nemo-cli --test per_repo_config`

**Depends on:** Step 6, Step 7
**Blocks:** nothing

### Step 10: Terraform module `nemo_setup_instructions` output

**Why:** Spec FR-16. Operator copy-paste block. FR-17: do not write to arbitrary paths — outputs only.

**Files:** `terraform/modules/nautiloop/outputs.tf`

**Approach:**
Add:
```hcl
output "nemo_setup_instructions" {
  description = "Copy-paste instructions to point your CLI at this nautiloop (per-repo config)"
  value = <<-EOT
    # Add to <your-repo>/nemo.toml:
    [server]
    url = "${local.server_url}"

    # Then, from your repo root:
    mkdir -p .nemo
    echo "${random_password.api_key.result}" > .nemo/credentials
    chmod 600 .nemo/credentials
    grep -qxF ".nemo/credentials" .gitignore 2>/dev/null || echo ".nemo/credentials" >> .gitignore
  EOT
  sensitive = true
}
```

Keep existing `server_url` and `api_key` outputs untouched (FR-16 note: back-compat).

Also update `post_apply_instructions_no_key`/`post_apply_instructions_with_key` in `main.tf` to mention the per-repo setup path so `nemo harden` guidance reflects the new flow.

**Tests:** `terraform -chdir=terraform/modules/nautiloop validate` if a terraform binary is available locally — if not, `terraform fmt` is the minimum we can run. Skip if unavailable and rely on PR CI.

**Depends on:** nothing
**Blocks:** nothing

### Step 11: Final checks and plan update

**Why:** Close the loop — run the full workspace suite, confirm clippy clean, update impl-plan status.

**Files:** `specs/per-repo-config-impl-plan.md`

**Approach:**
1. `cargo fmt --all`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace`
4. Mark all steps [x], update Progress Log, set Status = Complete.

**Depends on:** all previous steps
**Blocks:** nothing

## Acceptance Criteria Status

Based on the spec Requirements:

| Criterion | Status |
|-----------|--------|
| FR-1: CLI reads `[server].url` from `<repo>/nemo.toml` | ⬜ |
| FR-2: CLI reads API key from `<repo>/.nemo/credentials` (trimmed, one line) | ⬜ |
| FR-3: CLI honors `NEMO_SERVER_URL` and `NEMO_API_KEY` env vars | ⬜ |
| FR-4: server_url precedence: flag > env > repo > global | ⬜ |
| FR-5: api_key precedence: env > repo credentials > global | ⬜ |
| FR-6: identity fields only from `~/.nemo/config.toml` | ⬜ |
| FR-7: `nemo config --set` accepts `--local`/`--global`, auto-scopes when omitted | ⬜ |
| FR-8: `--local --set server_url` writes `[server].url` in `nemo.toml` | ⬜ |
| FR-9: `--local --set api_key` writes `.nemo/credentials` mode 0600 | ⬜ |
| FR-10: `--local --set engineer` fails with clear error | ⬜ |
| FR-11: `--global --set server_url` prints legacy warning | ⬜ |
| FR-12: `nemo init` seeds `.nemo/.gitignore` with `credentials` | ⬜ |
| FR-13: `nemo init` appends `.nemo/credentials` to root `.gitignore` idempotently | ⬜ |
| FR-14: `nemo config` with no flags prints values + source per field | ⬜ |
| FR-15: first-invocation per-process warning when per-repo overrides global | ⬜ |
| FR-16: terraform module adds `nemo_setup_instructions` output | ⬜ |
| FR-17: terraform module writes no arbitrary filesystem paths | ⬜ |
| NFR-1: existing global-only users see zero behavior change | ⬜ |
| NFR-2: no control-plane rebuild required | ⬜ |
| NFR-3: credentials file created 0600 atomically via .tmp + rename | ⬜ |
| NFR-4: config resolution <5ms | ⬜ (not measured; single file reads only, trivially <5ms) |
| Control-plane tolerates `[server]` in `nemo.toml` | ⬜ |

## Open Questions

None blocking. All design decisions are pinned in the spec.

## Review Checkpoints

- After Step 1: Control-plane tests pass; `RepoConfig` parses a nemo.toml with `[server]`.
- After Step 5: Unit tests for resolution precedence all pass.
- After Step 6: Existing CLI smoke test still passes (`nemo --help`).
- After Step 7: `nemo config`, `nemo config --local --set ...`, `nemo config --global --set ...` all exercised by unit tests.
- After Step 9: Integration test proves end-to-end binary behavior.
- After Step 11: `cargo test --workspace` + `cargo clippy --workspace -- -D warnings` both green.

## Progress Log

| Date | Step | Status | Notes |
|------|------|--------|-------|
| 2026-04-07 | — | Started | Plan created; branch `per-repo-config` already exists |

## Learnings

(To be populated during execution.)

## Bugs Found

(To be populated during execution.)

## Blockers / Notes

None.

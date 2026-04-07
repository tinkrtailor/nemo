# Per-Repo Server Config

## Overview

Move `server_url` and `api_key` out of the global `~/.nemo/config.toml` and into per-repo sources. Identity fields (`engineer`, `name`, `email`) stay global. Add env-var overrides for everything. Keep the global file as a lowest-priority fallback so existing installs keep working unchanged.

Resolves tinkrtailor/nautiloop#42.

## Problem Statement

Nautiloop is 1:1 with a repo by construction: the terraform module takes `git_repo_url` as required input, the bare repo PVC holds exactly one repo, and the SSH deploy key is bound to one GitHub repo. So `server_url` and `api_key` are per-repo coordinates by definition.

But `~/.nemo/config.toml` stores them globally. The moment an engineer has two nautiloop projects on the same workstation, they must manually swap server URL + API key in the global file every time they switch repos. `--server` only overrides the URL, not the key; there is no `NEMO_API_KEY` env var; there is no per-repo config file; there is no profile mechanism.

Three of five global-config fields are wrong-scoped today. Identity is the only thing that genuinely wants to be global.

### Current state (grounded)

- `cli/src/config.rs` defines `EngineerConfig { server_url, engineer, name, email, api_key }`, loaded only from `~/.nemo/config.toml`.
- `cli/src/main.rs:183-198` loads the global config and uses `cli.server.unwrap_or(eng_config.server_url)` for the URL; `api_key` comes solely from the global file.
- `cli/src/commands/config.rs` writes to the global file via `save_config`.
- The CLI does not currently read `nemo.toml` at all. Only `control-plane/src/config/repo.rs` parses `nemo.toml`, and only for repo-wide policy (`[ship]`, `[harden]`, `[models]`, `[repo]`, `[services]`).
- `cli/src/commands/init.rs` generates `nemo.toml` in the repo root by scanning the monorepo.

The CLI needs to learn to read `nemo.toml` for the new `[server]` section, and to know when it is "inside a repo."

## Dependencies

- Requires: nothing — purely CLI and terraform module work. No control-plane changes.
- Enables: cleaner install docs, `terraform apply → nemo harden` two-command flow, multi-repo workstation UX.

## Requirements

### Functional Requirements

- FR-1: The CLI shall read `[server] url` from `<repo>/nemo.toml` when run inside a repo.
- FR-2: The CLI shall read the API key from `<repo>/.nemo/credentials` (raw key, trimmed, one line) when the file exists.
- FR-3: The CLI shall honor `NEMO_SERVER_URL` and `NEMO_API_KEY` environment variables as higher-priority sources than any file.
- FR-4: The CLI shall resolve `server_url` in this order, first match wins:
  1. `--server` CLI flag
  2. `NEMO_SERVER_URL` env var
  3. `<repo>/nemo.toml` `[server].url`
  4. `~/.nemo/config.toml` `server_url` (legacy fallback)
- FR-5: The CLI shall resolve `api_key` in this order, first match wins:
  1. `NEMO_API_KEY` env var
  2. `<repo>/.nemo/credentials`
  3. `~/.nemo/config.toml` `api_key` (legacy fallback)
- FR-6: `engineer`, `name`, `email` shall continue to be read only from `~/.nemo/config.toml`.
- FR-7: `nemo config --set <key>=<value>` shall accept `--global` and `--local` flags. Default when `--global`/`--local` not given: `--local` if run inside a repo AND the key is per-repo-scoped (`server_url`, `api_key`); `--global` otherwise (identity keys).
- FR-8: `--local` + `server_url` shall write `[server] url` in `<repo>/nemo.toml`, creating the file if missing.
- FR-9: `--local` + `api_key` shall write `<repo>/.nemo/credentials` with mode 0600, creating the `.nemo/` directory if missing.
- FR-10: `--local` + any identity key (`engineer`, `name`, `email`) shall fail with a clear error: "identity is per-user; use --global or omit the flag."
- FR-11: `--global` + any per-repo key shall succeed but print a warning: "server_url/api_key in the global file is the legacy fallback. Prefer `nemo config --local --set ...` in your repo."
- FR-12: `nemo init` shall seed `<repo>/.nemo/` with a `.gitignore` file containing `credentials` (the credentials file must never be committed).
- FR-13: `nemo init` shall also append `.nemo/credentials` to the repo's top-level `.gitignore` if not already present.
- FR-14: `nemo config` with no flags shall print all sources and which one supplied each resolved value (e.g. `server_url: http://... (from nemo.toml)`).
- FR-15: When both per-repo and global sources define the same key, the CLI shall print a one-line warning on the first command invocation that uses the override, then be quiet thereafter within the same process. (No persistent "seen" state — per-process is enough.)
- FR-16: The terraform module `terraform/modules/nautiloop/` shall add outputs that print a copy-pasteable block for the operator to run in their consumer repo:
  ```
  # Add to <your-repo>/nemo.toml:
  [server]
  url = "http://<ip>:8080"

  # Then, from your repo:
  mkdir -p .nemo && echo "<api-key>" > .nemo/credentials && chmod 600 .nemo/credentials
  ```
- FR-17: The terraform module shall NOT write to arbitrary filesystem paths outside its own state — the operator may be applying from anywhere.

### Non-Functional Requirements

- NFR-1: Existing users with only `~/.nemo/config.toml` set shall see zero behavior change. The global file remains a valid source for all five fields.
- NFR-2: Per-repo config changes shall require no control-plane rebuild or redeploy — CLI-only change.
- NFR-3: The credentials file shall be created with mode 0600 atomically (write to `.nemo/credentials.tmp`, chmod, rename) to avoid a window where it exists world-readable.
- NFR-4: Config resolution shall add no measurable latency to CLI startup (target: <5ms total for all source checks).

## Architecture

### Source abstraction

Introduce `cli/src/config/sources.rs` with a `ConfigSource` enum tagging where each value came from, so `nemo config` can display provenance:

```rust
#[derive(Debug, Clone, Copy)]
pub enum ConfigSource {
    CliFlag,
    EnvVar,
    RepoToml,           // <repo>/nemo.toml
    RepoCredentials,    // <repo>/.nemo/credentials
    GlobalFile,         // ~/.nemo/config.toml
    Default,
}

pub struct Resolved<T> {
    pub value: T,
    pub source: ConfigSource,
}
```

`ResolvedConfig` replaces the current ad-hoc resolution in `cli/src/main.rs:183-198`:

```rust
pub struct ResolvedConfig {
    pub server_url: Resolved<String>,
    pub api_key: Option<Resolved<String>>,
    pub engineer: Resolved<String>,
    pub name: Resolved<String>,
    pub email: Resolved<String>,
}

pub fn resolve(cli_server: Option<&str>) -> Result<ResolvedConfig>;
```

### Repo detection

Walk up from `std::env::current_dir()` looking for the first ancestor containing `nemo.toml` OR `.git/`. If found, that's the repo root. Cache the result in `resolve()`; do not re-walk per field.

Edge cases:
- Worktrees: `.git` is a file, not a directory. Detect both.
- Nested repos (submodules): use the nearest `nemo.toml` match; if only `.git` is found, no repo-scoped `nemo.toml` is read (treat as "no repo config").
- Running outside any repo (e.g. `nemo status` from `$HOME`): no repo sources, only env + global.

### `nemo.toml` `[server]` section

Extend the CLI's new parser (not the control plane's — keep the control plane out of this):

```toml
# nemo.toml
[server]
url = "http://100.110.72.64:8080"
# api_key intentionally not here — sensitive, lives in .nemo/credentials or env
```

The section is optional. If present but `url` is missing or not a string, print a warning and ignore.

**Important:** the existing control-plane parser at `control-plane/src/config/repo.rs` must not break when it encounters the new `[server]` section. Today it uses strict `toml::from_str` into a typed struct. Verify the struct tolerates unknown sections (`#[serde(default)]` on each field, no `deny_unknown_fields`). If it doesn't, add `#[serde(default)] server: Option<toml::Value>` or equivalent to make it forward-compatible. **This is the one control-plane change — purely to tolerate the new section.**

### Credentials file

- Path: `<repo>/.nemo/credentials`
- Format: raw API key, trimmed, single line. No JSON, no TOML. Mirror how SSH private keys work: one file, one secret, mode-enforced.
- Mode: 0600. Enforced on write; checked on read (warn if readable by group/world, still use it).
- `.nemo/.gitignore` seeded by `nemo init` to contain `credentials`.

### `nemo config` command changes

```
nemo config                              # print all values with sources
nemo config --get <key>                  # print resolved value
nemo config --set key=value              # write to appropriate file (auto-scope)
nemo config --local --set key=value      # force per-repo
nemo config --global --set key=value     # force ~/.nemo/config.toml
```

Auto-scope rules (when neither `--local` nor `--global` given):
| Key          | Inside repo | Outside repo |
|--------------|-------------|--------------|
| `server_url` | local       | global       |
| `api_key`    | local       | global       |
| `engineer`   | global      | global       |
| `name`       | global      | global       |
| `email`      | global      | global       |

When writing to local, print the path touched: `Wrote [server].url to ./nemo.toml`.

### Terraform module outputs

In `terraform/modules/nautiloop/outputs.tf` (or wherever outputs live today), add:

```hcl
output "nemo_setup_instructions" {
  description = "Copy-paste instructions to point your CLI at this nautiloop"
  value = <<-EOT
    # Add to <your-repo>/nemo.toml:
    [server]
    url = "${local.nemo_server_url}"

    # Then, from your repo root:
    mkdir -p .nemo
    echo "${local.nemo_api_key}" > .nemo/credentials
    chmod 600 .nemo/credentials
    echo ".nemo/credentials" >> .gitignore
  EOT
  sensitive = true
}
```

Keep existing `nemo_server_url` and `nemo_api_key` outputs for back-compat and scripting.

## Security Considerations

- **Credentials file must never be committed.** Enforced by (1) `nemo init` seeding `.gitignore`, (2) `nemo init` seeding `.nemo/.gitignore`, (3) documentation. A pre-commit hook is out of scope for this spec but tracked as followup.
- **Mode 0600 atomic write:** use the same pattern as `save_config` in `cli/src/config.rs:62-93` (write to `.tmp` with `OpenOptions::mode(0o600)`, then rename).
- **Warn on loose permissions:** when reading `.nemo/credentials`, stat the file; if mode is broader than 0600, print a warning to stderr but still use it. Do not auto-fix (principle of least surprise).
- **No leakage in logs:** `nemo config` display must mask the API key regardless of source, using the existing masking logic in `cli/src/commands/config.rs:32-42`.
- **Env var precedence is correct:** env beats files because CI/direnv is the main use case, and explicit shell state should always win over ambient files.

## Migration Plan

Zero-downtime, zero-breaking. Four phases, all merged in this PR:

1. **Add resolution chain** — new sources become readable, nothing is written to new locations yet except by explicit `--local`. Existing global file continues to work as top-priority source... **wait, no** — global becomes *lowest* priority. This is technically a behavior change for anyone who has both set, but (a) no one currently can have per-repo set because the code to read it doesn't exist, and (b) env vars didn't exist either. So in practice: no one's behavior changes.
2. **Update `nemo config`** with `--local`/`--global` flags and provenance display.
3. **Update `nemo init`** to seed `.nemo/.gitignore` and update root `.gitignore`.
4. **Update terraform module** with copy-paste output and docs.

No deprecation warnings in this spec. File a followup issue: "print deprecation hint when `server_url`/`api_key` are read from the global file" — defer until we have telemetry to know how many users still rely on it.

## Test Plan

### Unit tests (`cli/src/config/sources.rs`)

- `test_resolve_server_url_env_beats_repo_toml`
- `test_resolve_server_url_repo_toml_beats_global`
- `test_resolve_server_url_cli_flag_beats_env`
- `test_resolve_api_key_env_beats_repo_credentials`
- `test_resolve_api_key_repo_credentials_beats_global`
- `test_resolve_identity_only_from_global`
- `test_repo_detection_walks_up_from_subdir`
- `test_repo_detection_handles_worktree` (`.git` as file)
- `test_repo_detection_returns_none_outside_repo`
- `test_resolve_no_repo_falls_back_to_env_plus_global`

### Unit tests (`cli/src/commands/config.rs`)

- `test_set_local_server_url_writes_to_nemo_toml`
- `test_set_local_api_key_writes_credentials_file_mode_0600`
- `test_set_local_identity_key_fails_with_clear_error`
- `test_set_global_server_url_prints_warning`
- `test_auto_scope_inside_repo` (server_url → local, engineer → global)
- `test_auto_scope_outside_repo` (everything → global)
- `test_display_shows_source_per_field`

### Unit tests (`cli/src/commands/init.rs`)

- `test_init_seeds_nemo_gitignore`
- `test_init_appends_to_root_gitignore_when_missing`
- `test_init_does_not_duplicate_gitignore_entry`

### Integration test

One end-to-end test using `assert_cmd` in `cli/tests/`:
1. Create tempdir, write minimal `nemo.toml` with `[server] url = "http://fake:1"`
2. Write `.nemo/credentials` with `test-key-123`
3. Run `nemo --help`-equivalent command that loads config
4. Assert resolved `server_url == "http://fake:1"` and `api_key == "test-key-123"`
5. Set `NEMO_SERVER_URL=http://env:2` and re-run, assert env wins

### Manual verification

- [ ] Fresh clone of a test repo, set per-repo config via `nemo config --local --set`, verify `nemo.toml` and `.nemo/credentials` contents and mode
- [ ] Run from a subdirectory two levels deep, verify repo root is found
- [ ] Run from `$HOME` (outside any repo), verify only env + global are consulted
- [ ] Verify the control-plane does NOT break when it parses a `nemo.toml` containing `[server]` (run `cargo test -p nautiloop-control-plane` after the forward-compat change)
- [ ] Apply the terraform module against a throwaway test project, copy-paste the output instructions, run `nemo harden` end-to-end

## Out of Scope

- **Profiles** (`[profiles.foo]` AWS-style). Nautiloop is 1:1 with a repo; profiles are overkill until that invariant breaks. If it ever does, profiles can layer on without breaking this design.
- **Secrets backends** (1Password, vault, macOS keychain). The credentials file can be a symlink to anything the user wants. Native backend integration is a separate spec.
- **Multi-engineer-per-repo** config. `engineer` stays per-user globally.
- **Pre-commit hook** to block committing `.nemo/credentials`. Seeding `.gitignore` is sufficient for v1; a hook is a separable hardening.
- **Deprecation warnings** for the legacy global-file fallback. Defer until we have usage signal.
- **Control-plane config resolution.** The control plane's `nemo.toml` parsing is unchanged except for the forward-compat tolerance of the new `[server]` section.

## Open Questions

- **None blocking.** Design is pinned. File followups for: pre-commit hook, deprecation warnings, secrets backend integration.

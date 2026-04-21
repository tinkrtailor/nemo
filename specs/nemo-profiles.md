# Nemo Profiles

## Overview

Support multiple nautiloop clusters per engineer via named profiles in `~/.nemo/config.toml`. Switch active cluster with `nemo use-profile <name>`, override per-command with `--profile <name>`. Same conceptual model as `kubectl` contexts, `aws` profiles, `gcloud configurations`.

Before: one `server_url` + one `api_key` in the CLI config; switching clusters means hand-editing the file or setting env vars for every command.

After: `nemo use-profile work` flips the active cluster in one command. Every other nemo invocation uses it.

## Baseline

Main at PR #185 (v0.7.3) merge.

Current `~/.nemo/config.toml` shape (`cli/src/config.rs`):
```toml
server_url = "http://localhost:18080"
engineer = "dev"
name = "Dev User"
email = "dev@example.com"
api_key = "dev-api-key-..."

[models]        # optional engineer-level model overrides
# implementor = "..."
# reviewer = "..."

[helm]
desktop_notifications = false
```

Commands read this file once at startup via `config::load_config()`. `nemo config --set key=value` writes scalar fields in-place. `--server` flag on each command overrides `server_url`.

## Problem Statement

### Problem 1: One engineer, multiple clusters is common

Operators realistically have:
- A local dev cluster (`http://localhost:18080`)
- A personal self-hosted production cluster (`http://<tailscale-ip>:8080`)
- A team production cluster at the company's nautiloop deployment

Today they manage this via:
- Hand-editing `~/.nemo/config.toml` when they switch
- OR typing `--server http://...` on every command (there is no `--api-key` flag and the CLI does not read API keys from environment variables; the only source is the config file)

Both are bad UX. Kubectl solved this in 2015 with contexts; nemo should inherit the pattern.

### Problem 2: No "which cluster am I on" answer

Engineers lose track. The closest today is `nemo config` which prints the single set of fields. A profile-aware CLI prints the active profile name prominently and makes mistakes obvious.

### Problem 3: Scripts have to reset state

Automation that touches multiple clusters has to save + restore `~/.nemo/config.toml`. Profiles let a script run `NEMO_PROFILE=staging nemo status` cleanly.

## Functional Requirements

### FR-1: Config schema

**FR-1a.** New `~/.nemo/config.toml` shape:

```toml
current_profile = "personal"   # which profile is active

[profiles.personal]
server_url = "http://100.64.1.10:8080"
api_key = "abc123..."
engineer = "gunnar"
name = "Gunnar"
email = "gunnar@reitun.is"

[profiles.work]
server_url = "https://nautiloop.work.internal"
api_key = "xyz789..."
engineer = "ggylfason"
name = "Gunnar"
email = "gunnar@work.example.com"

[profiles.dev]
server_url = "http://localhost:18080"
api_key = "dev-api-key-..."
engineer = "dev"

# Non-profile sections persist at the top level:
[helm]
desktop_notifications = false

[models]
# empty; kept at root as engineer-global preference
```

**FR-1b.** Profile names MUST match `^[a-zA-Z0-9][a-zA-Z0-9-]*$` (letters, digits, hyphens; start alphanumeric; minimum 1 character — single-char names like `d` are valid). Reserved name `default` is allowed. Profile names are **case-sensitive**: `Work` and `work` are distinct profiles. No normalization is performed.

**FR-1c.** `current_profile` MUST point at a defined profile. If missing or invalid, CLI errors on any command that needs a server URL. **Cold-start case (no config file):** When no `~/.nemo/config.toml` exists, `NemoConfig::default()` returns an empty `profiles` map and empty `current_profile`. Commands that require a server URL error with: `No profiles configured. Run 'nemo profile add <name> --server <url> --api-key <key> --engineer <id>' to get started.` This matches current behavior where an unconfigured CLI errors when hitting the server. Commands that don't need config (`help`, `capabilities`, `init`) work normally.

### FR-2: Backward-compatible migration

**FR-2a.** If the current `~/.nemo/config.toml` has flat `server_url`/`api_key`/etc. at the root (pre-profile shape), migration triggers automatically:
1. Reads the flat values
2. Creates a profile `default` under `[profiles.default]` containing them
3. Sets `current_profile = "default"`
4. Strips the flat fields from the root
5. Writes the file back
6. Prints: `Migrated config to profile 'default'. Create additional profiles with 'nemo profile add <name>'.`

Migration is idempotent: no-op if already in profile shape.

**Which commands trigger migration:** Any command that calls `load_config()` in the normal flow — i.e., all commands except `help`, `capabilities`, and `init` (which don't need config). The `nemo config` command, despite being dispatched before normal config loading (see FR-6e), MUST also perform migration before processing `--set` or `--get`.

**FR-2b.** ~~Removed.~~ The CLI has never read `NAUTILOOP_API_KEY` from the environment — that variable is server-side only (control-plane, terraform, k8s manifests). No env-var override behavior exists to preserve. Adding `NAUTILOOP_API_KEY` support to the CLI is out of scope for this spec; if desired, it should be a separate feature request with its own acceptance criteria.

### FR-3: `nemo profile` subcommands

**FR-3a.** New top-level command group:

| Command | Behavior |
|---|---|
| `nemo profile ls` (alias `list`) | Print all profiles, one per line, active one marked `*`. Include server_url and engineer for context. |
| `nemo profile show [<name>]` | Print the full profile detail. Omit name = current profile. Redacts api_key (`***` or first/last 4 chars). |
| `nemo profile add <name>` | All connection fields supplied via flags (see FR-3b). Defaults for `--name` and `--email` are copied from the current profile (same person, different cluster); `--server` and `--api-key` have no default and are required. Writes new profile. Does NOT switch to it; use `use-profile` separately. `--switch` flag combines both. |
| `nemo profile rm <name>` | Remove profile. Errors if `<name>` is the active one (at least one profile must always exist; to remove the last remaining profile, create a new one, switch to it, then remove the old one). |
| `nemo profile rename <old> <new>` | Rename. `<new>` must satisfy FR-1b name regex. Errors if a profile named `<new>` already exists. If `<old>` was active, update `current_profile` accordingly. |
| `nemo use-profile <name>` (alias `nemo profile use`) | Set `current_profile = <name>` in config. Prints new active state. Errors if `<name>` doesn't exist. |

**FR-3b.** `nemo profile add` with `--server`, `--api-key`, `--engineer`, `--name`, `--email` flags creates the profile non-interactively. `--server` and `--api-key` are required (no default); `--api-key` is validated non-empty (empty string is rejected). `--engineer` is required. `--name` and `--email` default to the current profile's values if omitted (if the current profile's `name`/`email` are empty, the defaults are also empty — this is not an error). If any required flag is missing, the command errors with a usage message — no interactive prompting. This avoids introducing a prompting dependency (see NFR-2).

**FR-3c.** Tab completion for profile names on the `profile` / `use-profile` subcommands is deferred to a follow-up. The CLI does not currently depend on `clap_complete`, and dynamic completions (reading profile names from the config file at completion time) are more complex than static completions. A follow-up may add `clap_complete` as a dependency for this purpose.

### FR-4: `--profile` global flag + env var

**FR-4a.** Every command accepts `--profile <name>` (global flag, same level as `--server`). Override applies only to that invocation; doesn't modify `current_profile`.

**FR-4b.** Environment variable `NAUTILOOP_PROFILE=<name>` has the same effect. Precedence: `--profile` > `NAUTILOOP_PROFILE` > `current_profile`. Edge cases: empty string `NAUTILOOP_PROFILE=""` is treated as unset. When `--profile` is provided, `NAUTILOOP_PROFILE` is ignored entirely (including for error messages — only the flag value is reported).

**FR-4c.** If a specified profile doesn't exist (via flag or env), CLI errors with the full list: `Profile 'staging' not found. Available: dev, personal, work.`

### FR-5: `nemo status` / `nemo helm` profile indicator

**FR-5a.** `nemo status` output gains a header line naming the effective profile (after precedence resolution per FR-4b — whether it came from `--profile` flag, `NAUTILOOP_PROFILE` env var, or `current_profile`):

```
# Profile: work · https://nautiloop.work.internal
LOOP ID ...
```

**FR-5b.** `nemo helm` TUI header (existing "nautiloop" top-left) appends the profile in muted color: `nautiloop · work`. Tells the operator at a glance which cluster they're driving.

**FR-5c.** `nemo config` (with no args) now prints the active profile + all profile names, not just flat fields. Output format:

```
Active profile: work
Profiles: default, personal, work*

  server_url: https://nautiloop.work.internal
  api_key:    ****789
  engineer:   ggylfason
  name:       Gunnar
  email:      gunnar@work.example.com

[helm]
  desktop_notifications: false
  theme: dark

[models]
  implementor: (not set)
  reviewer: (not set)
```

The active profile's fields are shown in full (with `api_key` redacted per NFR-3). Root-level sections (`[helm]`, `[models]`) are shown after the profile fields. Fields that are unset show `(not set)`.

### FR-6: `nemo config --set` aware of profiles

**FR-6a.** `nemo config --set server_url=<...>` writes to the ACTIVE profile's `server_url`. If the config is still in flat (pre-migration) format, migration (FR-2a) runs first, then the `--set` applies to the resulting profile.

**FR-6b.** `nemo config --set --profile=<name> server_url=<...>` writes to the named profile (useful for scripting).

**FR-6c.** `nemo config --set helm.desktop_notifications=true` writes to the root (non-profile) section. **Note:** support for `helm.*` and `models.*` keys in `--set` is NEW behavior — the current implementation only supports flat profile-scoped keys. Key scoping is determined by an explicit allow-list:

- **Profile-scoped keys** (written to the active profile): `server_url`, `api_key`, `engineer`, `name`, `email`.
- **Root-scoped keys** (written to top-level, using dot notation): `helm.desktop_notifications` (boolean), `helm.theme` (string, one of `dark`, `light`, `high-contrast`), `models.implementor` (string), `models.reviewer` (string). Dot notation maps to TOML table nesting (e.g., `helm.desktop_notifications=true` writes `desktop_notifications = true` under the `[helm]` table). `helm.theme` is validated against the allowed values; invalid values are rejected with: `Invalid value for helm.theme: '<value>'. Must be one of: dark, light, high-contrast`.
- **Value type coercion**: `true`/`false` (case-insensitive) are parsed as booleans; values that parse as integers are stored as integers; everything else is stored as a string.
- **Unrecognized keys** are rejected with an error: `Unknown config key '<key>'. Profile keys: server_url, api_key, engineer, name, email. Root keys: helm.desktop_notifications, helm.theme, models.implementor, models.reviewer`.

### FR-6g: `nemo config --get` aware of profiles

**FR-6g.** `nemo config --get <key>` reads from the active profile for profile-scoped keys (`server_url`, `api_key`, `engineer`, `name`, `email`) and from the root for root-scoped keys (`helm.desktop_notifications`, `helm.theme`, `models.implementor`, `models.reviewer`). The same allow-list from FR-6c applies. `--profile` flag overrides which profile to read from, following the same precedence as FR-4b. `api_key` is printed redacted unless `--unmask` is passed (consistent with `nemo profile show` redaction in NFR-3). Unrecognized keys are rejected with the same error message as FR-6c. If a key is unset (e.g., `name` is `None`), print nothing and exit with code 1.

### FR-6d: Internal struct layout (implementation note)

The current `EngineerConfig` struct holds all fields flat. After this change, the config layer splits into:

```rust
struct NemoConfig {
    current_profile: String,
    profiles: HashMap<String, ProfileConfig>,
    helm: HelmConfig,        // root-level, engineer-global
    models: ModelsSection,   // root-level, engineer-global
}

struct ProfileConfig {
    server_url: String,
    api_key: Option<String>,  // Optional to match current behavior (can be absent)
    engineer: String,
    name: Option<String>,
    email: Option<String>,
}
```

`api_key` is `Option<String>` to match current behavior where `api_key` can be absent in the config file. Migration preserves whatever value (or absence) was in the flat config. Commands that require an API key (e.g., those hitting the server) bail at runtime if `api_key` is `None`, matching the current behavior. `nemo profile add --api-key` requires a non-empty value (FR-3b), but profiles created via migration may have `api_key = None`.

A resolved accessor (e.g., `NemoConfig::active_profile(&self) -> &ProfileConfig`) returns the active profile's fields after applying the precedence chain (`--profile` > `NAUTILOOP_PROFILE` > `current_profile`). Code that currently reads `config.server_url` changes to `config.active_profile().server_url`. Code that reads `config.helm` or `config.models` continues reading from the root struct unchanged.

### FR-6e: Config command early-dispatch (implementation note)

The `nemo config` command is dispatched before normal config loading (`main.rs`) to allow repairing broken configs. This creates two constraints for profile support:

1. **Migration before `--set`**: The config command must perform FR-2a migration before processing any `--set` or `--get` operation. If the config is in flat format, migrate first, then apply the operation to the resulting profile structure.
2. **`--profile` flag resolution**: Since the config command runs before the main flag resolution flow, it must independently parse and resolve the `--profile` flag (or `NAUTILOOP_PROFILE` env var) to determine which profile to target for `--set` operations. The precedence chain is the same as FR-4b.
3. **Error on missing profile**: If `--profile=<name>` is specified and the profile doesn't exist (after migration), error per FR-4c.

### FR-6f: `use-profile` dual registration (implementation note)

`use-profile` is registered as both a top-level `Commands` variant (`Commands::UseProfile`) and as a `Profile` subcommand variant (`ProfileCommand::Use`), both routing to the same handler function. This provides ergonomic access via `nemo use-profile <name>` while keeping `nemo profile use <name>` discoverable within the profile command group.

### FR-7: Non-profile config persists across migration

**FR-7a.** After migration, `[helm]`, `[models]` at the root persist unchanged. They're engineer-global preferences, not cluster-scoped.

**FR-7b.** Per-profile `[models]` / `[helm]` overrides are OUT OF SCOPE for v1. Don't introduce per-profile sub-tables unless requested. Simple model: profiles contain connection info only.

## Non-Functional Requirements

### NFR-1: Zero breakage for existing users

Any existing `~/.nemo/config.toml` works via FR-2 migration. First command after upgrading to this version silently migrates and continues. If a user downgrades later, the profile shape is unreadable to old CLI — documented but accepted. (Binary has `version` field optional; not requiring it for v1.)

### NFR-2: No new dependencies

`toml` crate already used. No new crates for this spec. (`clap_complete` may be added in a follow-up for shell completions — see FR-3c.)

### NFR-3: Secret handling

API keys in the file stay at 0600 permissions (matches current behavior). `nemo profile show` redacts by default. No logging of keys.

### NFR-4: Tests

- **Unit** (`cli/src/config.rs`): migration from flat → profiles is idempotent; adds a profile; removes a profile; renames; switches active.
- **Unit**: precedence chain `--profile` > `NAUTILOOP_PROFILE` > `current_profile`.
- **Integration** (`cli/tests/profiles.rs`): spawn nemo with a test HOME, run migration, run `profile ls`, `use-profile`, `profile rm`.

## Acceptance Criteria

A reviewer can verify by:

1. **Migration**: start with a pre-profile `~/.nemo/config.toml` (flat fields). Run `nemo status`. File is rewritten with `[profiles.default]` and `current_profile = "default"`. Status still works.
2. **Add + switch**: `nemo profile add work --server https://... --api-key xyz --engineer gunnar`. Then `nemo use-profile work`. `nemo status` uses the new server.
3. **Per-command override**: `nemo --profile dev status` uses dev's server; `nemo status` right after still uses work (current_profile unchanged).
4. **Env var**: `NAUTILOOP_PROFILE=dev nemo status` uses dev without flag.
5. **List + show**: `nemo profile ls` marks active with `*`. `nemo profile show work` prints the work profile with `api_key` redacted.
6. **Cannot remove active**: `nemo profile rm <active>` errors clearly; no accidental lockout.
7. **Helm indicator**: `nemo helm` top-left shows `nautiloop · work` when work is active.
8. **Config get**: `nemo config --get server_url` returns the active profile's server URL. `nemo config --get --profile dev server_url` returns dev's server URL. `nemo config --get helm.theme` returns the root-level theme.

## Out of Scope

- **Per-profile model overrides** (engineer wants different defaults per cluster). Profiles are connection-only in v1.
- **Profile import/export** (sharing profiles between engineers). Everyone configures their own; API keys are per-engineer anyway.
- **Sync profiles from a central source** (company rolls out profiles via a script). Operators write their own bootstrap if they want this.
- **Encrypt profile file**. 0600 permissions + keychain integration is a bigger spec; current plaintext matches today's behavior.
- **Auto-detect cluster from URL probe**. Explicit config only.

## Files Likely Touched

- `cli/src/config.rs` — new profile-aware schema; migration function; accessor helpers.
- `cli/src/commands/profile.rs` — new module with `ls`, `show`, `add`, `rm`, `rename`, `use_profile`.
- `cli/src/commands/config.rs` — route `--set` through the active profile resolver.
- `cli/src/main.rs` — `--profile` global flag; profile command group; `NAUTILOOP_PROFILE` env.
- `cli/src/commands/status.rs` — header line with active profile.
- `cli/src/commands/helm.rs` — TUI title append.
- Tests per NFR-4.
- `docs/local-dev-quickstart.md` — brief mention of profiles.

## Baseline Branch

`main` at PR #185 (v0.7.3) merge.

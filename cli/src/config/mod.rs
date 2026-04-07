//! CLI config subsystem.
//!
//! Per `specs/per-repo-config.md`, configuration values come from multiple
//! sources. Resolution order (first match wins):
//!
//! * `server_url`: `--server` CLI flag > `NEMO_SERVER_URL` > `<repo>/nemo.toml [server].url` > `~/.nemo/config.toml`
//! * `api_key`: `NEMO_API_KEY` > `<repo>/.nemo/credentials` > `~/.nemo/config.toml`
//! * `engineer`, `name`, `email`: `~/.nemo/config.toml` only
//!
//! The [`sources`] submodule implements the resolver and owns the `Resolved<T>`
//! / `ConfigSource` / `ResolvedConfig` types. The [`engineer`] submodule holds
//! the legacy global-file reader/writer. [`repo_toml`] and [`credentials`]
//! handle the two per-repo sources.

pub mod credentials;
pub mod engineer;
pub mod repo_toml;

// Backwards-compatible re-exports so existing imports (`crate::config::load_config`,
// `crate::config::EngineerConfig`) continue to work without ripple-churn edits.
pub use engineer::{EngineerConfig, load_config, save_config};

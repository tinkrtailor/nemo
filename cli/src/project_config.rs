//! Per-project config resolution.
//!
//! Reads `[models]` from the nearest `./nemo.toml` (walking up from $PWD
//! like git does for .git) and from `~/.nemo/config.toml`. The model
//! resolver layers these with CLI flags and env vars so `nemo harden`
//! respects project intent without requiring flags on every invocation.
//!
//! Precedence (first match wins):
//!   1. CLI flag (--model-impl / --model-review)
//!   2. Env var (NEMO_MODEL_IMPLEMENTOR / NEMO_MODEL_REVIEWER)
//!   3. ~/.nemo/config.toml [models]          (engineer override)
//!   4. ./nemo.toml [models] (walking up)     (repo default)
//!   5. None (control plane uses its own default)
//!
//! Engineer wins over repo to match the control plane's existing merge
//! contract (engineer > repo > cluster, see control-plane/src/config/merged.rs
//! and docs/architecture.md). An engineer who wants a per-repo pin can still
//! use an env var in a direnv/.envrc or pass the flag explicitly.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct ModelsSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectTomlShape {
    #[serde(default)]
    models: ModelsSection,
}

/// Walk up from `start` looking for `nemo.toml`. Returns its directory
/// entry path if found, otherwise None.
pub fn find_project_toml(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join("nemo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

/// Load `[pricing]` from the nearest `./nemo.toml`, walking up from `start`.
/// Returns the raw TOML value for the pricing section if found.
pub fn load_project_pricing(start: &Path) -> Option<toml::Value> {
    let path = find_project_toml(start)?;
    let contents = std::fs::read_to_string(&path).ok()?;
    let parsed: toml::Value = toml::from_str(&contents).ok()?;
    parsed.get("pricing").cloned()
}

/// Load `[models]` from the nearest `./nemo.toml`, walking up from `start`.
/// Returns an empty section if no file is found or the section is absent.
pub fn load_project_models(start: &Path) -> Result<ModelsSection> {
    let Some(path) = find_project_toml(start) else {
        return Ok(ModelsSection::default());
    };
    let contents = std::fs::read_to_string(&path)?;
    let parsed: ProjectTomlShape = toml::from_str(&contents)?;
    Ok(parsed.models)
}

/// Resolve the effective (implementor, reviewer) model pair using the
/// documented precedence chain. Any layer may contribute only one of the
/// two fields; a missing field falls through to the next layer.
/// Treat empty / whitespace-only strings as absent. An empty env var or
/// `implementor = ""` in a config file is a common way to "unset" a layer,
/// and the control plane also filters empty strings when merging its own
/// config — without this, an empty override would bypass every fallback
/// and ship a blank model name to the loop stages.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

pub fn resolve_models(
    flag_impl: Option<String>,
    flag_review: Option<String>,
    user_models: &ModelsSection,
) -> Result<(Option<String>, Option<String>)> {
    let cwd = std::env::current_dir()?;
    let project = load_project_models(&cwd)?;

    let env_impl = std::env::var("NEMO_MODEL_IMPLEMENTOR").ok();
    let env_review = std::env::var("NEMO_MODEL_REVIEWER").ok();

    let implementor = non_empty(flag_impl)
        .or_else(|| non_empty(env_impl))
        .or_else(|| non_empty(user_models.implementor.clone()))
        .or_else(|| non_empty(project.implementor.clone()));

    let reviewer = non_empty(flag_review)
        .or_else(|| non_empty(env_review))
        .or_else(|| non_empty(user_models.reviewer.clone()))
        .or_else(|| non_empty(project.reviewer.clone()));

    Ok((implementor, reviewer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> tempdir_lite::TempDir {
        tempdir_lite::TempDir::new("nemo-cfg").unwrap()
    }

    #[test]
    fn find_walks_up() {
        let td = tmpdir();
        let root = td.path();
        fs::write(root.join("nemo.toml"), "[models]\n").unwrap();
        let nested = root.join("a").join("b").join("c");
        fs::create_dir_all(&nested).unwrap();
        let found = find_project_toml(&nested).unwrap();
        assert_eq!(found, root.join("nemo.toml"));
    }

    #[test]
    fn find_none_when_absent() {
        let td = tmpdir();
        assert!(find_project_toml(td.path()).is_none());
    }

    #[test]
    fn load_models_parses_section() {
        let td = tmpdir();
        fs::write(
            td.path().join("nemo.toml"),
            "[models]\nimplementor = \"opus\"\nreviewer = \"openai/gpt-5.4\"\n",
        )
        .unwrap();
        let m = load_project_models(td.path()).unwrap();
        assert_eq!(m.implementor.as_deref(), Some("opus"));
        assert_eq!(m.reviewer.as_deref(), Some("openai/gpt-5.4"));
    }

    #[test]
    fn load_models_empty_when_missing_file() {
        let td = tmpdir();
        let m = load_project_models(td.path()).unwrap();
        assert!(m.implementor.is_none());
        assert!(m.reviewer.is_none());
    }

    #[test]
    fn load_models_empty_when_section_absent() {
        let td = tmpdir();
        fs::write(td.path().join("nemo.toml"), "[repo]\nname = \"x\"\n").unwrap();
        let m = load_project_models(td.path()).unwrap();
        assert!(m.implementor.is_none());
        assert!(m.reviewer.is_none());
    }

    // resolve_models touches $PWD + env, which are process-global.
    // These tests run serially within a single #[test] to avoid cross-test
    // interference from parallel test runners.
    #[test]
    fn resolve_precedence_layers() {
        let td = tmpdir();
        let project_dir = td.path().join("proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("nemo.toml"),
            "[models]\nimplementor = \"proj-impl\"\nreviewer = \"proj-review\"\n",
        )
        .unwrap();

        let prev_cwd = std::env::current_dir().unwrap();
        let prev_env_impl = std::env::var("NEMO_MODEL_IMPLEMENTOR").ok();
        let prev_env_review = std::env::var("NEMO_MODEL_REVIEWER").ok();
        std::env::set_current_dir(&project_dir).unwrap();
        unsafe {
            std::env::remove_var("NEMO_MODEL_IMPLEMENTOR");
            std::env::remove_var("NEMO_MODEL_REVIEWER");
        }

        let user = ModelsSection {
            implementor: Some("user-impl".into()),
            reviewer: Some("user-review".into()),
        };

        // Layer 3: user (engineer) wins over project (repo).
        // Mirrors the control plane merge contract: engineer > repo > cluster.
        let (i, r) = resolve_models(None, None, &user).unwrap();
        assert_eq!(i.as_deref(), Some("user-impl"));
        assert_eq!(r.as_deref(), Some("user-review"));

        // With user empty, project fills in (layer 4).
        let empty_user = ModelsSection::default();
        let (i, r) = resolve_models(None, None, &empty_user).unwrap();
        assert_eq!(i.as_deref(), Some("proj-impl"));
        assert_eq!(r.as_deref(), Some("proj-review"));

        // Layer 2: env wins over user.
        unsafe {
            std::env::set_var("NEMO_MODEL_IMPLEMENTOR", "env-impl");
            std::env::set_var("NEMO_MODEL_REVIEWER", "env-review");
        }
        let (i, r) = resolve_models(None, None, &user).unwrap();
        assert_eq!(i.as_deref(), Some("env-impl"));
        assert_eq!(r.as_deref(), Some("env-review"));

        // Layer 1: flag wins over env.
        let (i, r) =
            resolve_models(Some("flag-impl".into()), Some("flag-review".into()), &user).unwrap();
        assert_eq!(i.as_deref(), Some("flag-impl"));
        assert_eq!(r.as_deref(), Some("flag-review"));

        // Mixed: flag-impl only, reviewer falls through env.
        let (i, r) = resolve_models(Some("flag-impl".into()), None, &user).unwrap();
        assert_eq!(i.as_deref(), Some("flag-impl"));
        assert_eq!(r.as_deref(), Some("env-review"));

        // Layer 4: clear env + user-empty-reviewer, reviewer falls to project.
        unsafe {
            std::env::remove_var("NEMO_MODEL_IMPLEMENTOR");
            std::env::remove_var("NEMO_MODEL_REVIEWER");
        }
        fs::write(
            project_dir.join("nemo.toml"),
            "[models]\nimplementor = \"proj-impl\"\nreviewer = \"proj-review\"\n",
        )
        .unwrap();
        let user_impl_only = ModelsSection {
            implementor: Some("user-impl".into()),
            reviewer: None,
        };
        let (i, r) = resolve_models(None, None, &user_impl_only).unwrap();
        assert_eq!(i.as_deref(), Some("user-impl"));
        assert_eq!(r.as_deref(), Some("proj-review"));

        // Layer 5: no project file, no env, no user -> all None.
        std::fs::remove_file(project_dir.join("nemo.toml")).unwrap();
        let empty = ModelsSection::default();
        let (i, r) = resolve_models(None, None, &empty).unwrap();
        assert!(i.is_none());
        assert!(r.is_none());

        // Regression for the earlier codex review: empty strings at any
        // layer must be treated as absent so they fall through instead
        // of shipping a blank model name to the loop. With user > project,
        // an empty user-impl should fall through to project-impl.
        fs::write(
            project_dir.join("nemo.toml"),
            "[models]\nimplementor = \"proj-impl\"\nreviewer = \"proj-review\"\n",
        )
        .unwrap();
        let user_empty_impl = ModelsSection {
            implementor: Some(String::new()),
            reviewer: Some("user-review".into()),
        };
        let (i, r) = resolve_models(None, None, &user_empty_impl).unwrap();
        assert_eq!(i.as_deref(), Some("proj-impl"));
        assert_eq!(r.as_deref(), Some("user-review"));

        // Empty env var also falls through.
        unsafe {
            std::env::set_var("NEMO_MODEL_IMPLEMENTOR", "");
        }
        let (i, _) = resolve_models(None, None, &user).unwrap();
        assert_eq!(i.as_deref(), Some("user-impl"));
        unsafe {
            std::env::remove_var("NEMO_MODEL_IMPLEMENTOR");
        }

        // Empty CLI flag also falls through.
        let (i, _) = resolve_models(Some(String::new()), None, &user).unwrap();
        assert_eq!(i.as_deref(), Some("user-impl"));

        // Restore.
        std::env::set_current_dir(&prev_cwd).unwrap();
        unsafe {
            if let Some(v) = prev_env_impl {
                std::env::set_var("NEMO_MODEL_IMPLEMENTOR", v);
            }
            if let Some(v) = prev_env_review {
                std::env::set_var("NEMO_MODEL_REVIEWER", v);
            }
        }
    }
}

// Tiny in-crate temp-dir helper so we don't pull tempfile into prod deps.
#[cfg(test)]
mod tempdir_lite {
    use std::path::{Path, PathBuf};

    pub struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub fn new(prefix: &str) -> std::io::Result<Self> {
            let mut base = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            base.push(format!("{prefix}-{nanos}-{}", std::process::id()));
            std::fs::create_dir_all(&base)?;
            Ok(Self { path: base })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

//! End-to-end integration test for per-repo configuration resolution.
//!
//! Exercises the full resolution pipeline through the built `nemo` binary,
//! verifying that:
//!
//! 1. A per-repo `nemo.toml` with `[server].url` is read.
//! 2. A per-repo `.nemo/credentials` file is read and masked in the display.
//! 3. `NEMO_SERVER_URL` env var wins over `nemo.toml`.
//! 4. `NEMO_API_KEY` env var wins over `.nemo/credentials`.
//! 5. Provenance labels are correct for each source.
//!
//! See `specs/per-repo-config.md` Test Plan → Integration test.

use assert_cmd::Command;
use predicates::str::contains;
use std::fs;
use std::path::Path;

/// Write a minimal `~/.nemo/config.toml` in `home` so `engineer` is non-empty
/// and the display has all identity fields. Mirrors what `nemo config --global
/// --set engineer=alice` would produce.
fn write_fake_home_config(home: &Path) {
    let dir = home.join(".nemo");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        r#"server_url = "https://from-global:8888"
engineer = "alice"
name = "Alice Example"
email = "alice@example.com"
"#,
    )
    .unwrap();
}

/// Write a repo layout with `nemo.toml` and `.nemo/credentials`.
fn write_fake_repo(repo: &Path) {
    fs::write(
        repo.join("nemo.toml"),
        r#"[repo]
name = "fakerepo"
default_branch = "main"

[server]
url = "http://from-nemo-toml:1"
"#,
    )
    .unwrap();
    fs::create_dir(repo.join(".nemo")).unwrap();
    fs::write(repo.join(".nemo").join("credentials"), "repo-key-xyz-123\n").unwrap();
}

/// Run `nemo config` with an isolated HOME and cwd. Returns the assertion
/// builder for further assertions.
fn run_nemo_config_display(home: &Path, cwd: &Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("nemo")
        .unwrap()
        .arg("config")
        .env("HOME", home)
        .env_remove("NEMO_SERVER_URL")
        .env_remove("NEMO_API_KEY")
        .current_dir(cwd)
        .assert()
}

#[test]
fn repo_sources_are_read_and_display_shows_provenance() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_fake_home_config(home.path());
    write_fake_repo(repo.path());

    let out = run_nemo_config_display(home.path(), repo.path());
    out.success()
        .stdout(contains("http://from-nemo-toml:1"))
        .stdout(contains("nemo.toml"))
        .stdout(contains(".nemo/credentials"))
        // API key "repo-key-xyz-123" (16 chars) masks to "repo...-123"
        .stdout(contains("repo...-123"))
        .stdout(contains("alice"));
}

#[test]
fn env_var_server_url_wins_over_nemo_toml() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_fake_home_config(home.path());
    write_fake_repo(repo.path());

    Command::cargo_bin("nemo")
        .unwrap()
        .arg("config")
        .env("HOME", home.path())
        .env("NEMO_SERVER_URL", "http://from-env:2")
        .env_remove("NEMO_API_KEY")
        .current_dir(repo.path())
        .assert()
        .success()
        .stdout(contains("http://from-env:2"))
        .stdout(contains("env var"));
}

#[test]
fn env_var_api_key_wins_over_credentials_file() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_fake_home_config(home.path());
    write_fake_repo(repo.path());

    Command::cargo_bin("nemo")
        .unwrap()
        .arg("config")
        .env("HOME", home.path())
        .env_remove("NEMO_SERVER_URL")
        .env("NEMO_API_KEY", "env-key-abcdefghij")
        .current_dir(repo.path())
        .assert()
        .success()
        // API key source line must say "env var"
        .stdout(contains("env var"));
}

#[test]
fn get_api_key_prints_masked_value_from_credentials_file() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_fake_home_config(home.path());
    write_fake_repo(repo.path());

    Command::cargo_bin("nemo")
        .unwrap()
        .arg("config")
        .args(["--get", "api_key"])
        .env("HOME", home.path())
        .env_remove("NEMO_SERVER_URL")
        .env_remove("NEMO_API_KEY")
        .current_dir(repo.path())
        .assert()
        .success()
        // Key is "repo-key-xyz-123" — 16 chars, so mask to "repo...-123"
        .stdout(contains("repo...-123"));
}

#[test]
fn no_repo_falls_back_to_global_file() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write_fake_home_config(home.path());
    // No nemo.toml, no .git — outside.path() is not a repo.
    // Note: tempdir on this host may live under /tmp; `find_repo_root` walks
    // up from there looking for `nemo.toml` or `.git`. If a parent directory
    // contains one, the test would find it. To be safe, create a
    // `.git` marker at the filesystem root is impossible, so we rely on the
    // fact that /tmp on CI and typical dev boxes has no nemo.toml above it.

    Command::cargo_bin("nemo")
        .unwrap()
        .arg("config")
        .env("HOME", home.path())
        .env_remove("NEMO_SERVER_URL")
        .env_remove("NEMO_API_KEY")
        .current_dir(outside.path())
        .assert()
        .success()
        .stdout(contains("https://from-global:8888"));
}

#[test]
fn cli_flag_server_beats_env_var() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_fake_home_config(home.path());
    write_fake_repo(repo.path());

    // --server is a global flag, but `config` doesn't hit the server, so
    // the display should still pick it up as the server_url source.
    Command::cargo_bin("nemo")
        .unwrap()
        .args(["--server", "http://from-flag:9"])
        .arg("config")
        .env("HOME", home.path())
        .env("NEMO_SERVER_URL", "http://from-env:2")
        .env_remove("NEMO_API_KEY")
        .current_dir(repo.path())
        .assert()
        .success()
        .stdout(contains("http://from-flag:9"))
        .stdout(contains("--server flag"));
}

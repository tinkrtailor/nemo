//! Integration tests for the nemo profiles feature.
//!
//! These tests spawn the `nemo` binary as a subprocess with a test HOME
//! directory to verify migration, profile management, and config operations.

use std::fs;
use std::process::Command;

/// Get the path to the nemo binary (built by `cargo test`).
fn nemo_bin() -> String {
    // Use the binary from the target directory
    let mut path = std::env::current_exe()
        .expect("current exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("parent of deps")
        .to_path_buf();
    path.push("nemo");
    path.to_string_lossy().to_string()
}

/// Create a temp HOME with a config file.
fn setup_home(config_content: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let nemo_dir = tmp.path().join(".nemo");
    fs::create_dir_all(&nemo_dir).unwrap();
    fs::write(nemo_dir.join("config.toml"), config_content).unwrap();
    tmp
}

/// Create a temp HOME with no config file.
fn setup_empty_home() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let nemo_dir = tmp.path().join(".nemo");
    fs::create_dir_all(&nemo_dir).unwrap();
    tmp
}

/// Run nemo with the given HOME and args, returning (stdout, stderr, exit_code).
fn run_nemo(home: &std::path::Path, args: &[&str]) -> (String, String, i32) {
    let output = Command::new(nemo_bin())
        .args(args)
        .env("HOME", home.to_str().unwrap())
        .env_remove("NAUTILOOP_PROFILE")
        .output()
        .expect("failed to execute nemo");

    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code().unwrap_or(-1),
    )
}

fn read_config(home: &std::path::Path) -> String {
    fs::read_to_string(home.join(".nemo/config.toml")).unwrap_or_default()
}

// ─── Migration Tests ──────────────────────────────────────────────────

#[test]
fn migration_flat_to_profiles() {
    let flat = r#"
server_url = "http://localhost:18080"
engineer = "dev"
name = "Dev User"
email = "dev@example.com"
api_key = "dev-api-key-12345678"

[helm]
desktop_notifications = false
"#;
    let home = setup_home(flat);

    // Run profile ls to trigger migration
    let (stdout, stderr, code) = run_nemo(home.path(), &["profile", "ls"]);

    assert_eq!(code, 0, "profile ls failed: {stderr}");
    assert!(
        stderr.contains("Migrated config to profile 'default'"),
        "Expected migration message, got stderr: {stderr}"
    );
    assert!(stdout.contains("default"), "Expected default profile in list, got: {stdout}");
    assert!(stdout.contains("http://localhost:18080"), "Expected server url, got: {stdout}");

    // Verify the file was rewritten with profile shape
    let config = read_config(home.path());
    assert!(config.contains("[profiles.default]"), "Config should have profiles section: {config}");
    assert!(
        config.contains("current_profile"),
        "Config should have current_profile: {config}"
    );
}

#[test]
fn migration_idempotent() {
    let profile_shape = r#"
current_profile = "default"

[profiles.default]
server_url = "http://localhost:18080"
engineer = "dev"
api_key = "dev-api-key-12345678"

[helm]
desktop_notifications = false
"#;
    let home = setup_home(profile_shape);

    let (_, stderr, code) = run_nemo(home.path(), &["profile", "ls"]);
    assert_eq!(code, 0, "profile ls failed: {stderr}");
    assert!(
        !stderr.contains("Migrated"),
        "Should not migrate already-migrated config, stderr: {stderr}"
    );
}

// ─── Profile CRUD Tests ──────────────────────────────────────────────

#[test]
fn profile_add_and_list() {
    let home = setup_empty_home();

    // Add first profile
    let (_, stderr, code) = run_nemo(
        home.path(),
        &[
            "profile", "add", "work",
            "--server", "https://work.example.com",
            "--api-key", "work-key-1234567890",
            "--engineer", "alice",
        ],
    );
    assert_eq!(code, 0, "profile add failed: {stderr}");
    assert!(stderr.contains("Added profile 'work'"), "stderr: {stderr}");
    // First profile should be auto-activated
    assert!(stderr.contains("Active profile: work"), "stderr: {stderr}");

    // Add second profile
    let (_, stderr, code) = run_nemo(
        home.path(),
        &[
            "profile", "add", "dev",
            "--server", "http://localhost:18080",
            "--api-key", "dev-key-1234567890",
            "--engineer", "dev",
        ],
    );
    assert_eq!(code, 0, "profile add dev failed: {stderr}");

    // List profiles
    let (stdout, _, code) = run_nemo(home.path(), &["profile", "ls"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("work"), "stdout: {stdout}");
    assert!(stdout.contains("dev"), "stdout: {stdout}");
    // work should be active (marked with *)
    assert!(stdout.contains("*"), "Expected active marker, stdout: {stdout}");
}

#[test]
fn profile_add_duplicate_errors() {
    let home = setup_empty_home();

    run_nemo(
        home.path(),
        &[
            "profile", "add", "work",
            "--server", "https://work.example.com",
            "--api-key", "key-1234567890123",
            "--engineer", "alice",
        ],
    );

    let (_, stderr, code) = run_nemo(
        home.path(),
        &[
            "profile", "add", "work",
            "--server", "https://other.example.com",
            "--api-key", "key-1234567890123",
            "--engineer", "bob",
        ],
    );
    assert_ne!(code, 0, "Should error on duplicate");
    assert!(stderr.contains("already exists"), "stderr: {stderr}");
}

#[test]
fn profile_remove() {
    let home = setup_empty_home();

    // Add two profiles
    run_nemo(home.path(), &[
        "profile", "add", "work",
        "--server", "https://work.example.com",
        "--api-key", "key-1234567890123",
        "--engineer", "alice",
    ]);
    run_nemo(home.path(), &[
        "profile", "add", "dev",
        "--server", "http://localhost:18080",
        "--api-key", "key-1234567890123",
        "--engineer", "dev",
    ]);

    // Cannot remove active profile
    let (_, stderr, code) = run_nemo(home.path(), &["profile", "rm", "work"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("Cannot remove the active profile"), "stderr: {stderr}");

    // Can remove non-active profile
    let (_, stderr, code) = run_nemo(home.path(), &["profile", "rm", "dev"]);
    assert_eq!(code, 0, "rm dev failed: {stderr}");
    assert!(stderr.contains("Removed profile 'dev'"), "stderr: {stderr}");
}

#[test]
fn profile_cannot_remove_last() {
    let home = setup_empty_home();

    run_nemo(home.path(), &[
        "profile", "add", "only",
        "--server", "https://example.com",
        "--api-key", "key-1234567890123",
        "--engineer", "alice",
    ]);

    // Switch to make a new profile active, then try to remove the last one
    // Actually with only one profile, it's the active one, so "cannot remove active" fires first.
    // Let's add a second, switch to it, remove the first, then try to remove the last.
    run_nemo(home.path(), &[
        "profile", "add", "second",
        "--server", "https://other.com",
        "--api-key", "key-1234567890123",
        "--engineer", "bob",
    ]);
    run_nemo(home.path(), &["use-profile", "second"]);
    run_nemo(home.path(), &["profile", "rm", "only"]);

    // Now only "second" remains — cannot remove last
    let (_, stderr, code) = run_nemo(home.path(), &["profile", "rm", "second"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("Cannot remove the active profile") || stderr.contains("Cannot remove the last profile"),
        "stderr: {stderr}"
    );
}

#[test]
fn use_profile_switches_active() {
    let home = setup_empty_home();

    run_nemo(home.path(), &[
        "profile", "add", "work",
        "--server", "https://work.example.com",
        "--api-key", "key-1234567890123",
        "--engineer", "alice",
    ]);
    run_nemo(home.path(), &[
        "profile", "add", "dev",
        "--server", "http://localhost:18080",
        "--api-key", "key-1234567890123",
        "--engineer", "dev",
    ]);

    // Switch to dev
    let (_, stderr, code) = run_nemo(home.path(), &["use-profile", "dev"]);
    assert_eq!(code, 0, "use-profile failed: {stderr}");
    assert!(stderr.contains("Active profile: dev"), "stderr: {stderr}");

    // Verify dev is now active in config --get
    let (stdout, _, code) = run_nemo(home.path(), &["config", "--get", "current_profile"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "dev");
}

#[test]
fn profile_rename() {
    let home = setup_empty_home();

    run_nemo(home.path(), &[
        "profile", "add", "old-name",
        "--server", "https://example.com",
        "--api-key", "key-1234567890123",
        "--engineer", "alice",
    ]);

    let (_, stderr, code) = run_nemo(home.path(), &["profile", "rename", "old-name", "new-name"]);
    assert_eq!(code, 0, "rename failed: {stderr}");
    assert!(stderr.contains("Renamed"), "stderr: {stderr}");

    // Current profile should be updated
    let (stdout, _, _) = run_nemo(home.path(), &["config", "--get", "current_profile"]);
    assert_eq!(stdout.trim(), "new-name");
}

// ─── Config --get/--set Tests ─────────────────────────────────────────

#[test]
fn config_get_profile_scoped() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"
name = "Alice"
email = "alice@example.com"
"#;
    let home = setup_home(config);

    let (stdout, _, code) = run_nemo(home.path(), &["config", "--get", "server_url"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "https://work.example.com");

    let (stdout, _, code) = run_nemo(home.path(), &["config", "--get", "engineer"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "alice");

    // api_key should be redacted
    let (stdout, _, code) = run_nemo(home.path(), &["config", "--get", "api_key"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("..."), "Should be redacted: {stdout}");
    assert!(!stdout.contains("1234567890123456"), "Should not show full key: {stdout}");

    // api_key with --unmask
    let (stdout, _, code) = run_nemo(home.path(), &["config", "--get", "api_key", "--unmask"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "key-1234567890123456");
}

#[test]
fn config_set_profile_scoped() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"
"#;
    let home = setup_home(config);

    let (_, _, code) = run_nemo(home.path(), &["config", "--set", "engineer=bob"]);
    assert_eq!(code, 0);

    let (stdout, _, code) = run_nemo(home.path(), &["config", "--get", "engineer"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "bob");
}

#[test]
fn config_set_root_scoped() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"

[helm]
desktop_notifications = false
"#;
    let home = setup_home(config);

    let (_, _, code) = run_nemo(home.path(), &["config", "--set", "helm.desktop_notifications=true"]);
    assert_eq!(code, 0);

    let (stdout, _, code) = run_nemo(home.path(), &["config", "--get", "helm.desktop_notifications"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "true");
}

#[test]
fn config_set_rejects_unknown_key() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"
"#;
    let home = setup_home(config);

    let (_, stderr, code) = run_nemo(home.path(), &["config", "--set", "bogus_key=value"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("Unknown config key"), "stderr: {stderr}");
}

#[test]
fn config_set_current_profile_rejected() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"
"#;
    let home = setup_home(config);

    let (_, stderr, code) = run_nemo(home.path(), &["config", "--set", "current_profile=other"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("use-profile"), "stderr: {stderr}");
}

// ─── --profile Flag Tests ─────────────────────────────────────────────

#[test]
fn profile_flag_overrides_config_get() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"

[profiles.dev]
server_url = "http://localhost:18080"
api_key = "key-dev-1234567890123456"
engineer = "dev"
"#;
    let home = setup_home(config);

    // Without --profile: returns work's server
    let (stdout, _, code) = run_nemo(home.path(), &["config", "--get", "server_url"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "https://work.example.com");

    // With --profile dev: returns dev's server
    let (stdout, _, code) = run_nemo(home.path(), &["--profile", "dev", "config", "--get", "server_url"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "http://localhost:18080");
}

#[test]
fn env_var_overrides_current_profile() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"

[profiles.dev]
server_url = "http://localhost:18080"
api_key = "key-dev-1234567890123456"
engineer = "dev"
"#;
    let home = setup_home(config);

    let output = Command::new(nemo_bin())
        .args(["config", "--get", "server_url"])
        .env("HOME", home.path().to_str().unwrap())
        .env("NAUTILOOP_PROFILE", "dev")
        .output()
        .expect("failed to execute nemo");

    assert_eq!(output.status.code().unwrap(), 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "http://localhost:18080");
}

// ─── Full Precedence Chain Test ───────────────────────────────────────

#[test]
fn profile_flag_overrides_env_var() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"

[profiles.dev]
server_url = "http://localhost:18080"
api_key = "key-dev-1234567890123456"
engineer = "dev"

[profiles.staging]
server_url = "https://staging.example.com"
api_key = "key-staging-1234567890"
engineer = "stager"
"#;
    let home = setup_home(config);

    // Set NAUTILOOP_PROFILE=staging but --profile=dev; flag should win
    let output = Command::new(nemo_bin())
        .args(["--profile", "dev", "config", "--get", "server_url"])
        .env("HOME", home.path().to_str().unwrap())
        .env("NAUTILOOP_PROFILE", "staging")
        .output()
        .expect("failed to execute nemo");

    assert_eq!(output.status.code().unwrap(), 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "http://localhost:18080",
        "Flag --profile=dev should override NAUTILOOP_PROFILE=staging"
    );
}

// ─── Profile Show Tests ───────────────────────────────────────────────

#[test]
fn profile_show_redacts_key() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"
name = "Alice"
email = "alice@example.com"
"#;
    let home = setup_home(config);

    let (stdout, _, code) = run_nemo(home.path(), &["profile", "show"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("work"), "stdout: {stdout}");
    assert!(stdout.contains("..."), "Key should be redacted: {stdout}");
    assert!(!stdout.contains("1234567890123456"), "Full key should not appear: {stdout}");

    // With --unmask
    let (stdout, _, code) = run_nemo(home.path(), &["profile", "show", "--unmask"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("key-1234567890123456"), "Full key should appear with --unmask: {stdout}");
}

// ─── Config Display Test ──────────────────────────────────────────────

#[test]
fn config_display_shows_profile_info() {
    let config = r#"
current_profile = "work"

[profiles.work]
server_url = "https://work.example.com"
api_key = "key-1234567890123456"
engineer = "alice"

[profiles.dev]
server_url = "http://localhost:18080"
api_key = "key-dev-1234567890123456"
engineer = "dev"

[helm]
desktop_notifications = true
"#;
    let home = setup_home(config);

    let (stdout, _, code) = run_nemo(home.path(), &["config"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Active profile: work"), "stdout: {stdout}");
    assert!(stdout.contains("work*"), "Active should be marked with *: {stdout}");
    assert!(stdout.contains("dev"), "Should list all profiles: {stdout}");
    assert!(stdout.contains("[helm]"), "Should show helm section: {stdout}");
}

// ─── Profile Use via subcommand alias ─────────────────────────────────

#[test]
fn profile_use_subcommand_works() {
    let home = setup_empty_home();

    run_nemo(home.path(), &[
        "profile", "add", "work",
        "--server", "https://work.example.com",
        "--api-key", "key-1234567890123",
        "--engineer", "alice",
    ]);
    run_nemo(home.path(), &[
        "profile", "add", "dev",
        "--server", "http://localhost:18080",
        "--api-key", "key-1234567890123",
        "--engineer", "dev",
    ]);

    // Use the `profile use` subcommand variant
    let (_, stderr, code) = run_nemo(home.path(), &["profile", "use", "dev"]);
    assert_eq!(code, 0, "profile use failed: {stderr}");
    assert!(stderr.contains("Active profile: dev"), "stderr: {stderr}");

    let (stdout, _, _) = run_nemo(home.path(), &["config", "--get", "current_profile"]);
    assert_eq!(stdout.trim(), "dev");
}

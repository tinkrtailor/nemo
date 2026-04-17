use std::io::{self, Write};
use std::time::Duration;

use anyhow::Result;

use crate::api_types::PodIntrospectResponse;
use crate::client::NemoClient;

/// Fetch the introspection snapshot for a loop.
/// Returns Ok(Some(response)) on success, Ok(None) on terminal loop (exit 2),
/// Err on other failures.
pub async fn fetch(client: &NemoClient, loop_id: &str) -> Result<FetchResult> {
    let path = format!("/pod-introspect/{loop_id}");
    let response = client.get_response(&path).await?;
    let status = response.status();

    if status.as_u16() == 410 {
        let body: serde_json::Value = response.json().await?;
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("loop is terminal");
        return Ok(FetchResult::Terminal(msg.to_string()));
    }

    if status.as_u16() == 425 {
        let body: serde_json::Value = response.json().await?;
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("pod not yet running");
        return Ok(FetchResult::NotReady(msg.to_string()));
    }

    if status.as_u16() == 503 {
        return Ok(FetchResult::Timeout);
    }

    if !status.is_success() {
        let body = response.text().await?;
        anyhow::bail!("API error ({status}): {body}");
    }

    let snapshot: PodIntrospectResponse = response.json().await?;
    Ok(FetchResult::Ok(Box::new(snapshot)))
}

pub enum FetchResult {
    Ok(Box<PodIntrospectResponse>),
    Terminal(String),
    NotReady(String),
    Timeout,
}

/// Run `nemo ps <loop_id>` (one-shot mode).
/// Returns the exit code (0 = success, 1 = error, 2 = terminal loop).
pub async fn run(client: &NemoClient, loop_id: &str) -> Result<i32> {
    match fetch(client, loop_id).await? {
        FetchResult::Ok(snapshot) => {
            render_snapshot(&snapshot, &mut io::stdout())?;
            Ok(0)
        }
        FetchResult::Terminal(msg) => {
            eprintln!("{msg}");
            Ok(2)
        }
        FetchResult::NotReady(msg) => {
            eprintln!("{msg}");
            Ok(1)
        }
        FetchResult::Timeout => {
            eprintln!("pod introspection timeout — retry shortly");
            Ok(1)
        }
    }
}

/// Run `nemo ps --watch <loop_id>` (polling mode).
pub async fn run_watch(client: &NemoClient, loop_id: &str) -> Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // Install a panic hook that restores the terminal before printing the
    // panic message. Without this, a panic during watch mode leaves the
    // terminal in raw mode, requiring the user to run `reset`.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let result = run_watch_loop(client, loop_id).await;

    // Restore the original panic hook before exiting
    let _ = std::panic::take_hook();

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    result
}

async fn run_watch_loop(client: &NemoClient, loop_id: &str) -> Result<()> {
    loop {
        // Check for 'q' key press (non-blocking). Ignore transient input errors
        // (e.g. terminal resize events that fail to parse) to avoid killing the
        // watch loop on benign I/O hiccups.
        if crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key))
                    if key.kind == crossterm::event::KeyEventKind::Press
                        && key.code == crossterm::event::KeyCode::Char('q') =>
                {
                    return Ok(());
                }
                Ok(_) => {} // ignore non-key events (resize, mouse, etc.)
                Err(_) => {
                    // Prevent tight spin if read() fails persistently
                    // (poll returns true but read errors). Sleep briefly
                    // before the next iteration.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }

        // Clear screen and move to top
        print!("\x1b[2J\x1b[H");

        match fetch(client, loop_id).await {
            Ok(FetchResult::Ok(snapshot)) => {
                render_snapshot(&snapshot, &mut io::stdout())?;
                println!("\n\x1b[2m(press q to quit, refreshing every 2s)\x1b[0m");
            }
            Ok(FetchResult::Terminal(msg)) => {
                println!("{msg}");
                return Ok(());
            }
            Ok(FetchResult::NotReady(msg)) => {
                println!("{msg}");
            }
            Ok(FetchResult::Timeout) => {
                println!("pod introspection timeout — retrying...");
            }
            Err(e) => {
                println!("error: {e}");
            }
        }

        io::stdout().flush()?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Render a snapshot to the given writer (FR-4a).
pub fn render_snapshot(snapshot: &PodIntrospectResponse, w: &mut dyn Write) -> Result<()> {
    // Header: pod + phase
    writeln!(
        w,
        "Pod: {}  Phase: {}",
        snapshot.pod_name, snapshot.pod_phase
    )?;

    // Stats line
    match &snapshot.container_stats {
        Some(stats) => {
            writeln!(
                w,
                "CPU: {}m  Mem: {}",
                stats.cpu_millicores,
                format_bytes(stats.memory_bytes)
            )?;
        }
        None => {
            writeln!(w, "CPU: unavailable  Mem: unavailable")?;
        }
    }

    // Worktree line
    let wt = &snapshot.worktree;
    let head = wt.head_sha.as_deref().map(|s| {
        let len = s.len().min(7);
        &s[..len]
    }).unwrap_or("-");
    let target_str = match (wt.target_dir_bytes, wt.target_dir_artifacts) {
        (Some(bytes), Some(artifacts)) => {
            format!("target: {} ({} artifacts)", format_bytes(bytes), artifacts)
        }
        (Some(bytes), None) => format!("target: {}", format_bytes(bytes)),
        _ => "target: -".to_string(),
    };
    let dirty_str = match wt.uncommitted_files {
        Some(n) => format!("dirty={n} files"),
        None => "dirty=? files".to_string(),
    };
    writeln!(
        w,
        "Worktree: {}  HEAD: {}  {}  {}",
        wt.path, head, dirty_str, target_str
    )?;

    writeln!(w)?;

    // Process table
    writeln!(
        w,
        "{:<6}{:<6}{:<8}{:<7}{:<9}COMMAND",
        "PID", "PPID", "USER", "CPU%", "AGE"
    )?;
    for p in &snapshot.processes {
        writeln!(
            w,
            "{:<6}{:<6}{:<8}{:<7}{:<9}{}",
            p.pid,
            p.ppid,
            p.user,
            format!("{:.1}", p.cpu_percent),
            format_age(p.age_seconds),
            p.cmd
        )?;
    }

    if snapshot.processes.is_empty() {
        writeln!(w, "(no processes)")?;
    }

    if !snapshot.warnings.is_empty() {
        writeln!(w)?;
        for warning in &snapshot.warnings {
            writeln!(w, "\x1b[33mwarning:\x1b[0m {warning}")?;
        }
    }

    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

fn format_age(seconds: u64) -> String {
    if seconds >= 3600 {
        let hours = seconds / 3600;
        let mins = (seconds % 3600) / 60;
        format!("{hours}h{mins}m")
    } else if seconds >= 60 {
        let mins = seconds / 60;
        let secs = seconds % 60;
        if secs == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m{secs}s")
        }
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_types::*;

    fn sample_snapshot() -> PodIntrospectResponse {
        PodIntrospectResponse {
            loop_id: uuid::Uuid::nil(),
            pod_name: "nautiloop-abc123-implement-r2-t1-xyz".to_string(),
            pod_phase: "Running".to_string(),
            collected_at: "2026-04-17T12:45:00Z".to_string(),
            container_stats: Some(ContainerStats {
                cpu_millicores: 508,
                memory_bytes: 959963136,
            }),
            processes: vec![
                ProcessInfo {
                    pid: 12,
                    ppid: 1,
                    user: "agent".to_string(),
                    cpu_percent: 3.2,
                    cmd: "claude".to_string(),
                    age_seconds: 1320,
                },
                ProcessInfo {
                    pid: 126,
                    ppid: 124,
                    user: "agent".to_string(),
                    cpu_percent: 0.0,
                    cmd: "cargo-clippy clippy --workspace -- -D warnings".to_string(),
                    age_seconds: 900,
                },
                ProcessInfo {
                    pid: 4319,
                    ppid: 130,
                    user: "agent".to_string(),
                    cpu_percent: 18.7,
                    cmd: "rustc --crate-name axum ...".to_string(),
                    age_seconds: 12,
                },
            ],
            worktree: WorktreeInfo {
                path: "/work".to_string(),
                target_dir_artifacts: Some(1069),
                target_dir_bytes: Some(3221225472),
                uncommitted_files: Some(2),
                head_sha: Some("42bffd9abc".to_string()),
            },
            warnings: Vec::new(),
        }
    }

    #[test]
    fn test_render_snapshot_deterministic() {
        let snapshot = sample_snapshot();
        let mut buf = Vec::new();
        render_snapshot(&snapshot, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("Pod: nautiloop-abc123-implement-r2-t1-xyz"));
        assert!(output.contains("Phase: Running"));
        assert!(output.contains("CPU: 508m"));
        assert!(output.contains("915 MiB"));
        assert!(output.contains("HEAD: 42bffd9"));
        assert!(output.contains("dirty=2 files"));
        assert!(output.contains("1069 artifacts"));
        assert!(output.contains("claude"));
        assert!(output.contains("cargo-clippy"));
        assert!(output.contains("rustc --crate-name axum"));
    }

    #[test]
    fn test_render_snapshot_no_stats() {
        let mut snapshot = sample_snapshot();
        snapshot.container_stats = None;
        let mut buf = Vec::new();
        render_snapshot(&snapshot, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("CPU: unavailable"));
        assert!(output.contains("Mem: unavailable"));
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1 KiB");
        assert_eq!(format_bytes(1048576), "1 MiB");
        assert_eq!(format_bytes(3221225472), "3.0 GiB");
    }

    #[test]
    fn test_render_snapshot_with_warnings() {
        let mut snapshot = sample_snapshot();
        snapshot.warnings = vec!["exec timed out (read timeout after 1s), showing partial data".to_string()];
        snapshot.processes = Vec::new();
        let mut buf = Vec::new();
        render_snapshot(&snapshot, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("warning:"));
        assert!(output.contains("exec timed out"));
        assert!(output.contains("(no processes)"));
    }

    #[test]
    fn test_format_age() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(12), "12s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(900), "15m");
        assert_eq!(format_age(1320), "22m");
        assert_eq!(format_age(3661), "1h1m");
    }
}

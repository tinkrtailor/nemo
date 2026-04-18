use anyhow::Result;

use crate::api_types::CacheResponse;
use crate::client::NemoClient;

/// Run `nemo cache show`. Prints resolved cache config and disk usage.
pub async fn run(client: &NemoClient, json: bool) -> Result<()> {
    let resp: CacheResponse = client.get("/cache").await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }

    // Plain text output (FR-6a format).
    if resp.disabled {
        println!("Cache: disabled");
        println!("\nNo /cache mount or cache env vars are set on agent pods.");
        return Ok(());
    }

    // Volume info line (FR-6a: "Cache volume: nautiloop-cache (50 GiB)")
    if let Some(cap) = resp.volume_capacity_gi {
        println!("Cache volume: {} ({cap} GiB)", resp.volume_name);
    } else {
        println!("Cache volume: {}", resp.volume_name);
    }

    // Disk usage line (FR-6a: "Disk usage: 2.1 GiB / 50 GiB (4%)")
    if let Some(ref usage) = resp.disk_usage {
        let normalized_total = normalize_size_to_gib(&usage.total);
        if let Some(cap) = resp.volume_capacity_gi {
            let pct = compute_percentage(&usage.total, cap);
            println!("Disk usage:   {normalized_total} / {cap} GiB ({pct}%)");
        } else {
            println!("Disk usage:   {normalized_total}");
        }

        if !usage.subdirectories.is_empty() {
            println!();
            println!("Subdirectory sizes:");
            let mut dirs: Vec<_> = usage.subdirectories.iter().collect();
            dirs.sort_by_key(|(path, _)| path.as_str());
            for (path, size) in dirs {
                let normalized = normalize_size_to_gib(size);
                println!("  {path:<30} {normalized}");
            }
        }
        println!();
    } else {
        println!("Disk usage:   unavailable (no running pod)");
        println!();
    }

    // Active env vars
    if resp.env.is_empty() {
        println!("Active env vars: (none)");
    } else {
        println!("Active env vars (from control-plane config):");
        let mut keys: Vec<_> = resp.env.keys().collect();
        keys.sort();
        // Find max key length for alignment
        let max_len = keys.iter().map(|k| k.len()).max().unwrap_or(0);
        for key in keys {
            let val = &resp.env[key];
            println!("  {key:<max_len$} = {val}");
        }
    }

    Ok(())
}

/// Parse a `du` size string (e.g. "2.1G", "340M", "1.8K") to bytes.
fn parse_du_size_to_bytes(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('T') {
        (n, 1024.0 * 1024.0 * 1024.0 * 1024.0)
    } else if let Some(n) = s.strip_suffix('G') {
        (n, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1024.0 * 1024.0)
    } else if let Some(n) = s.strip_suffix('K') {
        (n, 1024.0)
    } else {
        (s, 1.0) // bytes
    };
    num_str.parse::<f64>().ok().map(|v| v * multiplier)
}

/// Normalize a `du` size string to GiB display (e.g. "2.1G" → "2.1 GiB", "340M" → "340 MiB").
/// Keeps the human-readable magnitude but uses IEC units for consistency with PVC capacity.
fn normalize_size_to_gib(s: &str) -> String {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('T') {
        format!("{n} TiB")
    } else if let Some(n) = s.strip_suffix('G') {
        format!("{n} GiB")
    } else if let Some(n) = s.strip_suffix('M') {
        format!("{n} MiB")
    } else if let Some(n) = s.strip_suffix('K') {
        format!("{n} KiB")
    } else {
        format!("{s} B")
    }
}

/// Compute usage percentage from a `du` size string and capacity in GiB.
fn compute_percentage(du_total: &str, capacity_gi: u64) -> u64 {
    let bytes = match parse_du_size_to_bytes(du_total) {
        Some(b) => b,
        None => return 0,
    };
    let cap_bytes = capacity_gi as f64 * 1024.0 * 1024.0 * 1024.0;
    if cap_bytes <= 0.0 {
        return 0;
    }
    ((bytes / cap_bytes) * 100.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_du_size_to_bytes() {
        // G suffix → GiB (du -h uses powers of 1024)
        let bytes = parse_du_size_to_bytes("2.1G").unwrap();
        assert!((bytes - 2.1 * 1024.0 * 1024.0 * 1024.0).abs() < 1.0);

        let bytes = parse_du_size_to_bytes("340M").unwrap();
        assert!((bytes - 340.0 * 1024.0 * 1024.0).abs() < 1.0);

        let bytes = parse_du_size_to_bytes("1.8K").unwrap();
        assert!((bytes - 1.8 * 1024.0).abs() < 1.0);

        let bytes = parse_du_size_to_bytes("512T").unwrap();
        assert!((bytes - 512.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0).abs() < 1.0);

        // Raw number (bytes)
        let bytes = parse_du_size_to_bytes("4096").unwrap();
        assert!((bytes - 4096.0).abs() < 0.001);

        // Empty / invalid
        assert!(parse_du_size_to_bytes("").is_none());
        assert!(parse_du_size_to_bytes("abcG").is_none());
    }

    #[test]
    fn test_normalize_size_to_gib() {
        assert_eq!(normalize_size_to_gib("2.1G"), "2.1 GiB");
        assert_eq!(normalize_size_to_gib("340M"), "340 MiB");
        assert_eq!(normalize_size_to_gib("1.8K"), "1.8 KiB");
        assert_eq!(normalize_size_to_gib("512T"), "512 TiB");
        assert_eq!(normalize_size_to_gib("4096"), "4096 B");
    }

    #[test]
    fn test_compute_percentage() {
        // 2.1 GiB out of 50 GiB ≈ 4%
        assert_eq!(compute_percentage("2.1G", 50), 4);

        // 25 GiB out of 50 GiB = 50%
        assert_eq!(compute_percentage("25G", 50), 50);

        // 340 MiB out of 50 GiB ≈ 1%
        assert_eq!(compute_percentage("340M", 50), 1);

        // 0 capacity → 0%
        assert_eq!(compute_percentage("2.1G", 0), 0);

        // Empty string → 0%
        assert_eq!(compute_percentage("", 50), 0);
    }
}

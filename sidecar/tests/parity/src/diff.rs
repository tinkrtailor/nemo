//! Diff engine: compare two normalized [`SideOutput`]s and report
//! field-level differences.
//!
//! FR-18 step 5 requires this to run on every test case, and NFR-5
//! requires that failure reports point at the case's JSON file.
//! The diff report here is plain-text and contains only field names
//! and values — the source path is added by the caller.

use std::collections::BTreeSet;

use crate::result::{ObservedMockRequest, SideOutput};

/// Compare two side outputs. Returns an empty string if they match,
/// or a human-readable diff summary otherwise.
pub fn diff_sides(go: &SideOutput, rust: &SideOutput) -> String {
    let mut report = Vec::new();

    if go.http_status != rust.http_status && (go.http_status != 0 || rust.http_status != 0) {
        report.push(format!(
            "http_status: go={}, rust={}",
            go.http_status, rust.http_status
        ));
    }
    if go.http_body != rust.http_body {
        report.push(format!(
            "http_body differs:\n  go:   {}\n  rust: {}",
            truncate(&go.http_body, 256),
            truncate(&rust.http_body, 256)
        ));
    }
    report.extend(diff_headers(&go.http_headers, &rust.http_headers));

    if go.ssh_exit_status != rust.ssh_exit_status {
        report.push(format!(
            "ssh_exit_status: go={:?}, rust={:?}",
            go.ssh_exit_status, rust.ssh_exit_status
        ));
    }
    if go.ssh_stdout_hex != rust.ssh_stdout_hex {
        report.push(format!(
            "ssh_stdout differs:\n  go:   {}\n  rust: {}",
            truncate(&go.ssh_stdout_hex, 256),
            truncate(&rust.ssh_stdout_hex, 256)
        ));
    }
    if go.ssh_stderr != rust.ssh_stderr {
        report.push(format!(
            "ssh_stderr differs:\n  go:   {:?}\n  rust: {:?}",
            go.ssh_stderr, rust.ssh_stderr
        ));
    }
    if go.ssh_channel_failed != rust.ssh_channel_failed {
        report.push(format!(
            "ssh_channel_failed: go={}, rust={}",
            go.ssh_channel_failed, rust.ssh_channel_failed
        ));
    }

    report.extend(diff_mock_observations(
        &go.mock_observations,
        &rust.mock_observations,
    ));

    report.join("\n")
}

fn diff_headers(
    go: &std::collections::BTreeMap<String, String>,
    rust: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let keys: BTreeSet<&String> = go.keys().chain(rust.keys()).collect();
    for k in keys {
        match (go.get(k), rust.get(k)) {
            (Some(gv), Some(rv)) if gv != rv => {
                out.push(format!("http_header {k}: go={gv:?}, rust={rv:?}"));
            }
            (Some(gv), None) => {
                out.push(format!("http_header {k}: only on go (value {gv:?})"));
            }
            (None, Some(rv)) => {
                out.push(format!("http_header {k}: only on rust (value {rv:?})"));
            }
            _ => {}
        }
    }
    out
}

fn diff_mock_observations(go: &[ObservedMockRequest], rust: &[ObservedMockRequest]) -> Vec<String> {
    let mut out = Vec::new();
    if go.len() != rust.len() {
        out.push(format!(
            "mock_observations count differs: go={}, rust={}",
            go.len(),
            rust.len()
        ));
    }
    // Strip source_ip before comparing — the only difference between
    // paired Go and Rust observations is the source IP, which is how
    // we split them in the first place. Everything else should match.
    let g_norm: Vec<String> = go.iter().map(observation_fingerprint).collect();
    let r_norm: Vec<String> = rust.iter().map(observation_fingerprint).collect();
    if g_norm != r_norm {
        out.push("mock_observations content differs".to_string());
        for i in 0..g_norm.len().max(r_norm.len()) {
            let g = g_norm.get(i).map(|s| s.as_str()).unwrap_or("<missing>");
            let r = r_norm.get(i).map(|s| s.as_str()).unwrap_or("<missing>");
            if g != r {
                out.push(format!("  idx {i}: go={g}"));
                out.push(format!("           rust={r}"));
            }
        }
    }
    out
}

/// Canonical single-line summary of a mock observation that strips
/// source_ip so paired observations compare equal.
fn observation_fingerprint(o: &ObservedMockRequest) -> String {
    let headers_sorted: Vec<String> = o.headers.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!(
        "{mock}|{method}|{path}|host={host}|headers=[{h}]|body={b}",
        mock = o.mock,
        method = o.method,
        path = o.path,
        host = o.host_header,
        h = headers_sorted.join(","),
        b = o.body_b64
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}… ({} bytes total)", &s[..max], s.len())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn bmap(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn identical_sides_produce_empty_diff() {
        let go = SideOutput::http(200, bmap(&[("content-type", "application/json")]), "{}");
        let rust = SideOutput::http(200, bmap(&[("content-type", "application/json")]), "{}");
        assert_eq!(diff_sides(&go, &rust), "");
    }

    #[test]
    fn status_difference_reported() {
        let go = SideOutput::http(200, bmap(&[]), "");
        let rust = SideOutput::http(403, bmap(&[]), "");
        let diff = diff_sides(&go, &rust);
        assert!(
            diff.contains("http_status: go=200, rust=403"),
            "diff={diff}"
        );
    }

    #[test]
    fn body_difference_reported() {
        let go = SideOutput::http(200, bmap(&[]), r#"{"a":1}"#);
        let rust = SideOutput::http(200, bmap(&[]), r#"{"a":2}"#);
        let diff = diff_sides(&go, &rust);
        assert!(diff.contains("http_body differs"));
    }

    #[test]
    fn header_only_on_one_side_reported() {
        let go = SideOutput::http(200, bmap(&[("x-rust-only", "val")]), "");
        let rust = SideOutput::http(200, bmap(&[]), "");
        let diff = diff_sides(&go, &rust);
        assert!(
            diff.contains("only on go"),
            "expected 'only on go' in diff:\n{diff}"
        );
    }

    #[test]
    fn ssh_exit_status_difference_reported() {
        let mut go = SideOutput::default();
        let mut rust = SideOutput::default();
        go.ssh_exit_status = Some(0);
        rust.ssh_exit_status = Some(1);
        let diff = diff_sides(&go, &rust);
        assert!(diff.contains("ssh_exit_status"));
    }

    #[test]
    fn mock_observations_source_ip_ignored_in_fingerprint() {
        let go_obs = vec![ObservedMockRequest {
            mock: "mock-openai".into(),
            method: "GET".into(),
            path: "/v1/models".into(),
            host_header: "api.openai.com".into(),
            headers: BTreeMap::new(),
            body_b64: String::new(),
            source_ip: "100.64.0.20".into(),
        }];
        let rust_obs = vec![ObservedMockRequest {
            mock: "mock-openai".into(),
            method: "GET".into(),
            path: "/v1/models".into(),
            host_header: "api.openai.com".into(),
            headers: BTreeMap::new(),
            body_b64: String::new(),
            source_ip: "100.64.0.21".into(),
        }];
        let mut go = SideOutput::default();
        let mut rust = SideOutput::default();
        go.mock_observations = go_obs;
        rust.mock_observations = rust_obs;
        assert_eq!(diff_sides(&go, &rust), "");
    }

    #[test]
    fn mock_observations_body_difference_reported() {
        let mut go = SideOutput::default();
        let mut rust = SideOutput::default();
        go.mock_observations = vec![ObservedMockRequest {
            mock: "mock-openai".into(),
            method: "GET".into(),
            path: "/".into(),
            host_header: "api.openai.com".into(),
            headers: BTreeMap::new(),
            body_b64: "dGVzdA==".into(),
            source_ip: "100.64.0.20".into(),
        }];
        rust.mock_observations = vec![ObservedMockRequest {
            mock: "mock-openai".into(),
            method: "GET".into(),
            path: "/".into(),
            host_header: "api.openai.com".into(),
            headers: BTreeMap::new(),
            body_b64: "b3RoZXI=".into(),
            source_ip: "100.64.0.21".into(),
        }];
        let diff = diff_sides(&go, &rust);
        assert!(diff.contains("content differs"));
    }

    #[test]
    fn truncate_handles_long_strings() {
        let long = "a".repeat(300);
        let t = truncate(&long, 256);
        assert!(t.starts_with(&"a".repeat(256)));
        assert!(t.contains("300 bytes total"));
    }

    #[test]
    fn truncate_passes_through_short() {
        assert_eq!(truncate("short", 256), "short");
    }
}

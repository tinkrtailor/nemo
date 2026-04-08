//! FR-19 normalization rules, applied to both Go and Rust sidecar
//! outputs before the diff engine compares them.
//!
//! Kept as pure functions so every rule can be unit-tested without
//! spinning up docker.

use std::collections::BTreeMap;

use crate::corpus::NormalizeConfig;
use crate::result::{ObservedMockRequest, SideOutput};

/// Per-rule: headers stripped from HTTP responses before comparison.
/// These fields are dynamic or backend-specific. The spec
/// (FR-19) explicitly lists these.
pub const BASELINE_STRIPPED_RESPONSE_HEADERS: &[&str] = &[
    "date",
    "server",
    "via",
    "x-request-id",
    "connection",
    "content-length",
];

/// Normalize a SideOutput in place. Idempotent — running twice yields
/// the same result.
pub fn normalize(side: &mut SideOutput, config: &NormalizeConfig) {
    strip_response_headers(&mut side.http_headers, config);
    strip_body_fields(&mut side.http_body, config);
    normalize_ssh_stderr(&mut side.ssh_stderr);
    normalize_mock_observations(&mut side.mock_observations);
}

/// Strip baseline + per-case extra headers. Header names are compared
/// case-insensitively (stored in BTreeMap<String, String> keyed by
/// lowercase).
pub fn strip_response_headers(headers: &mut BTreeMap<String, String>, config: &NormalizeConfig) {
    for h in BASELINE_STRIPPED_RESPONSE_HEADERS {
        headers.remove(*h);
    }
    for h in &config.extra_header_strip {
        headers.remove(&h.to_ascii_lowercase());
    }
}

/// If the body parses as JSON, remove every field named in
/// `config.body_strip_fields` (recursive over objects and arrays) and
/// re-serialize in canonical (sorted-key) form. Non-JSON bodies are
/// left untouched.
pub fn strip_body_fields(body: &mut String, config: &NormalizeConfig) {
    if body.is_empty() || config.body_strip_fields.is_empty() {
        return;
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    recursively_strip(&mut value, &config.body_strip_fields);
    if let Ok(reserialized) = canonical_json(&value) {
        *body = reserialized;
    }
}

fn recursively_strip(value: &mut serde_json::Value, fields: &[String]) {
    match value {
        serde_json::Value::Object(map) => {
            for f in fields {
                map.remove(f);
            }
            for (_, v) in map.iter_mut() {
                recursively_strip(v, fields);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                recursively_strip(v, fields);
            }
        }
        _ => {}
    }
}

/// Canonical JSON serialization: sort object keys recursively so two
/// equivalent JSON documents produce the same bytes.
pub fn canonical_json(value: &serde_json::Value) -> Result<String, serde_json::Error> {
    let sorted = sort_keys(value.clone());
    serde_json::to_string(&sorted)
}

fn sort_keys(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                std::collections::BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k, sort_keys(v));
            }
            // Convert BTreeMap back into a serde_json::Map with
            // preserved insertion order (which is now sorted).
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sort_keys).collect())
        }
        other => other,
    }
}

/// Trim trailing whitespace from SSH stderr per FR-19.
pub fn normalize_ssh_stderr(stderr: &mut String) {
    let trimmed_len = stderr.trim_end().len();
    stderr.truncate(trimmed_len);
}

/// Sort mock observations by `(path, method, source_ip)` and lowercase
/// header names per FR-19.
pub fn normalize_mock_observations(obs: &mut [ObservedMockRequest]) {
    for o in obs.iter_mut() {
        lowercase_header_names(&mut o.headers);
    }
    obs.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.method.cmp(&b.method))
            .then(a.source_ip.cmp(&b.source_ip))
    });
}

fn lowercase_header_names(headers: &mut BTreeMap<String, String>) {
    let taken: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    headers.clear();
    for (k, v) in taken {
        headers.insert(k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn btreemap_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        m
    }

    #[test]
    fn strip_baseline_headers_removes_date_server_etc() {
        let mut headers = btreemap_of(&[
            ("content-type", "application/json"),
            ("date", "Wed, 08 Apr 2026 00:00:00 GMT"),
            ("server", "hypercorn-h11/0.17.3"),
            ("x-request-id", "req-123"),
            ("content-length", "42"),
            ("via", "1.1 proxy"),
            ("connection", "keep-alive"),
        ]);
        strip_response_headers(&mut headers, &NormalizeConfig::default());
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn strip_extra_headers_from_config() {
        let mut headers =
            btreemap_of(&[("content-type", "application/json"), ("x-custom", "value")]);
        let cfg = NormalizeConfig {
            body_strip_fields: vec![],
            extra_header_strip: vec!["X-Custom".to_string()],
        };
        strip_response_headers(&mut headers, &cfg);
        assert!(!headers.contains_key("x-custom"));
    }

    #[test]
    fn strip_body_fields_removes_named_json_keys_recursively() {
        let mut body =
            r#"{"id":"abc","nested":{"id":"xyz","keep":"me"},"arr":[{"id":"zzz"},1]}"#.to_string();
        let cfg = NormalizeConfig {
            body_strip_fields: vec!["id".to_string()],
            extra_header_strip: vec![],
        };
        strip_body_fields(&mut body, &cfg);
        // Canonical form: sorted keys.
        assert_eq!(body, r#"{"arr":[{},1],"nested":{"keep":"me"}}"#);
    }

    #[test]
    fn strip_body_fields_no_op_on_non_json_body() {
        let mut body = "Not JSON".to_string();
        let cfg = NormalizeConfig {
            body_strip_fields: vec!["id".to_string()],
            extra_header_strip: vec![],
        };
        strip_body_fields(&mut body, &cfg);
        assert_eq!(body, "Not JSON");
    }

    #[test]
    fn strip_body_fields_preserves_when_no_match() {
        let mut body = r#"{"a":1,"b":2}"#.to_string();
        let cfg = NormalizeConfig {
            body_strip_fields: vec!["nothing".to_string()],
            extra_header_strip: vec![],
        };
        strip_body_fields(&mut body, &cfg);
        // Still canonicalized though.
        assert_eq!(body, r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"z":1,"a":2,"m":{"c":3,"b":4}}"#).unwrap();
        let out = canonical_json(&v).unwrap();
        assert_eq!(out, r#"{"a":2,"m":{"b":4,"c":3},"z":1}"#);
    }

    #[test]
    fn ssh_stderr_trims_trailing_whitespace() {
        let mut s = "error message\n\n  ".to_string();
        normalize_ssh_stderr(&mut s);
        assert_eq!(s, "error message");
    }

    #[test]
    fn mock_observations_sorted_deterministically() {
        let mut obs = vec![
            ObservedMockRequest {
                mock: "m".into(),
                method: "POST".into(),
                path: "/b".into(),
                host_header: "h".into(),
                headers: BTreeMap::new(),
                body_b64: String::new(),
                source_ip: "100.64.0.21".into(),
            },
            ObservedMockRequest {
                mock: "m".into(),
                method: "GET".into(),
                path: "/a".into(),
                host_header: "h".into(),
                headers: BTreeMap::new(),
                body_b64: String::new(),
                source_ip: "100.64.0.20".into(),
            },
        ];
        normalize_mock_observations(&mut obs);
        assert_eq!(obs[0].path, "/a");
        assert_eq!(obs[1].path, "/b");
    }

    #[test]
    fn normalize_side_output_is_idempotent() {
        let mut side = SideOutput::http(
            200,
            btreemap_of(&[("content-type", "application/json"), ("date", "now")]),
            r#"{"id":"abc","v":1}"#,
        );
        let cfg = NormalizeConfig {
            body_strip_fields: vec!["id".to_string()],
            extra_header_strip: vec![],
        };
        normalize(&mut side, &cfg);
        let after_first = side.clone();
        normalize(&mut side, &cfg);
        assert_eq!(side, after_first);
    }

    #[test]
    fn normalize_lowercases_mock_header_names() {
        let mut obs = vec![ObservedMockRequest {
            mock: "m".into(),
            method: "GET".into(),
            path: "/".into(),
            host_header: "h".into(),
            headers: btreemap_of(&[("X-Thing", "v"), ("Authorization", "Bearer x")]),
            body_b64: String::new(),
            source_ip: "100.64.0.20".into(),
        }];
        normalize_mock_observations(&mut obs);
        assert!(obs[0].headers.contains_key("x-thing"));
        assert!(obs[0].headers.contains_key("authorization"));
        assert!(!obs[0].headers.contains_key("X-Thing"));
    }
}

//! Parity corpus schema, loader, and validator (FR-21 / FR-22).
//!
//! One JSON file per case lives under `corpus/`. Each file deserializes
//! into a [`CorpusCase`]. The loader enforces the at-most-one
//! `order_hint: "last"` invariant and rejects unknown categories at
//! load time (rather than at runtime in the dispatcher).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Category of a parity test case.
///
/// The enum values correspond 1:1 to the spec's five categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    ModelProxy,
    Egress,
    GitSsh,
    Health,
    Divergence,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelProxy => "model_proxy",
            Self::Egress => "egress",
            Self::GitSsh => "git_ssh",
            Self::Health => "health",
            Self::Divergence => "divergence",
        }
    }
}

/// Order hint from the corpus schema. Currently only `"last"` is
/// recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderHint {
    Last,
}

/// Description of the expected divergent behavior when
/// `expected_parity: false`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DivergenceDescriptor {
    pub description: String,
    pub go_expected: String,
    pub rust_expected: String,
}

/// Per-case normalization overrides applied after the baseline FR-19
/// rules run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct NormalizeConfig {
    #[serde(default)]
    pub body_strip_fields: Vec<String>,
    #[serde(default)]
    pub extra_header_strip: Vec<String>,
}

/// Category-specific input descriptor. Kept as opaque `serde_json::Value`
/// so the corpus files can evolve without forcing a schema migration
/// here — the runner modules interpret the fields relevant to their
/// category.
pub type CaseInput = serde_json::Value;

/// A single parity case as loaded from disk.
///
/// `path` is populated at load time and points at the source file so
/// failure diagnostics can reference FR-NFR-5's "Diff points at the
/// corpus JSON filename".
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CorpusCase {
    pub name: String,
    pub category: Category,
    #[serde(default)]
    pub description: String,
    pub input: CaseInput,
    pub expected_parity: bool,
    #[serde(default)]
    pub divergence: Option<DivergenceDescriptor>,
    #[serde(default)]
    pub normalize: NormalizeConfig,
    #[serde(default)]
    pub order_hint: Option<OrderHint>,

    /// Absolute path of the case file. NOT deserialized (populated by
    /// the loader). `#[serde(skip)]` so JSON payloads don't need it.
    #[serde(skip)]
    pub path: PathBuf,
}

/// Errors produced by the corpus loader.
#[derive(Debug, Error)]
pub enum CorpusError {
    #[error("failed to read corpus directory {0}: {1}")]
    ReadDir(PathBuf, #[source] std::io::Error),
    #[error("failed to read corpus case file {0}: {1}")]
    ReadFile(PathBuf, #[source] std::io::Error),
    #[error("failed to parse corpus case file {0}: {1}")]
    Parse(PathBuf, #[source] serde_json::Error),
    #[error("corpus has duplicate case name {0:?}")]
    DuplicateName(String),
    #[error("corpus has {count} cases with order_hint=last; at most one is allowed (FR-21)")]
    MultipleOrderHintLast { count: usize },
    #[error(
        "corpus case {name} has expected_parity=false but no divergence descriptor (FR-21 / FR-22)"
    )]
    DivergenceWithoutDescriptor { name: String },
    #[error(
        "corpus case {name} has expected_parity=true but a divergence descriptor (FR-21 / FR-22)"
    )]
    DescriptorWithoutDivergence { name: String },
}

/// Load every `.json` file under `dir` and return the parsed cases,
/// sorted by file name for deterministic execution order.
///
/// This is the only public entry point the driver uses; individual
/// `CorpusCase` deserialization is not exposed so callers always get
/// a validated set.
pub fn load_corpus(dir: impl AsRef<Path>) -> Result<Vec<CorpusCase>, CorpusError> {
    let dir = dir.as_ref();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| CorpusError::ReadDir(dir.to_path_buf(), e))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort();

    let mut cases: Vec<CorpusCase> = Vec::with_capacity(entries.len());
    for path in entries {
        let bytes = fs::read(&path).map_err(|e| CorpusError::ReadFile(path.clone(), e))?;
        let mut case: CorpusCase =
            serde_json::from_slice(&bytes).map_err(|e| CorpusError::Parse(path.clone(), e))?;
        case.path = path.clone();
        cases.push(case);
    }

    validate_corpus(&cases)?;
    Ok(cases)
}

/// Validate the loaded corpus against the FR-21 / FR-22 invariants:
///
/// - case names are unique
/// - at most one case has `order_hint: "last"`
/// - `expected_parity == false` iff a divergence descriptor is present
pub fn validate_corpus(cases: &[CorpusCase]) -> Result<(), CorpusError> {
    let mut seen = HashSet::new();
    let mut order_hint_count = 0;
    for case in cases {
        if !seen.insert(case.name.clone()) {
            return Err(CorpusError::DuplicateName(case.name.clone()));
        }
        if case.order_hint == Some(OrderHint::Last) {
            order_hint_count += 1;
        }
        if !case.expected_parity && case.divergence.is_none() {
            return Err(CorpusError::DivergenceWithoutDescriptor {
                name: case.name.clone(),
            });
        }
        if case.expected_parity && case.divergence.is_some() {
            return Err(CorpusError::DescriptorWithoutDivergence {
                name: case.name.clone(),
            });
        }
    }
    if order_hint_count > 1 {
        return Err(CorpusError::MultipleOrderHintLast {
            count: order_hint_count,
        });
    }
    Ok(())
}

/// Partition a corpus slice into `(normal_cases, order_last_cases)` so
/// the driver can run the `order_hint: "last"` cases after everything
/// else (FR-22 `divergence_connect_drain_on_sigterm` kills the
/// containers).
pub fn partition_by_order_hint(cases: &[CorpusCase]) -> (Vec<&CorpusCase>, Vec<&CorpusCase>) {
    let (last, rest): (Vec<&CorpusCase>, Vec<&CorpusCase>) = cases
        .iter()
        .partition(|c| c.order_hint == Some(OrderHint::Last));
    (rest, last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn parity_case_bytes(name: &str, category: &str) -> Vec<u8> {
        serde_json::json!({
            "name": name,
            "category": category,
            "description": "test",
            "input": {},
            "expected_parity": true,
            "normalize": {}
        })
        .to_string()
        .into_bytes()
    }

    fn divergence_case_bytes(name: &str, order_hint: Option<&str>) -> Vec<u8> {
        let mut json = serde_json::json!({
            "name": name,
            "category": "divergence",
            "description": "test",
            "input": {},
            "expected_parity": false,
            "divergence": {
                "description": "x",
                "go_expected": "a",
                "rust_expected": "b"
            },
            "normalize": {}
        });
        if let Some(h) = order_hint {
            json["order_hint"] = serde_json::Value::String(h.to_string());
        }
        json.to_string().into_bytes()
    }

    #[test]
    fn load_parity_case_roundtrip() {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("a.json"),
            parity_case_bytes("a", "model_proxy"),
        )
        .unwrap();
        let cases = load_corpus(tmp.path()).expect("load");
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "a");
        assert_eq!(cases[0].category, Category::ModelProxy);
        assert!(cases[0].expected_parity);
        assert!(cases[0].path.ends_with("a.json"));
    }

    #[test]
    fn deserializes_all_five_categories() {
        for (file, cat, expected) in [
            ("m.json", "model_proxy", Category::ModelProxy),
            ("e.json", "egress", Category::Egress),
            ("g.json", "git_ssh", Category::GitSsh),
            ("h.json", "health", Category::Health),
        ] {
            let tmp = tempdir().expect("tempdir");
            std::fs::write(tmp.path().join(file), parity_case_bytes("x", cat)).unwrap();
            let cases = load_corpus(tmp.path()).expect("load");
            assert_eq!(cases[0].category, expected);
        }
    }

    #[test]
    fn divergence_case_requires_descriptor() {
        let tmp = tempdir().expect("tempdir");
        let bad = serde_json::json!({
            "name": "d",
            "category": "divergence",
            "input": {},
            "expected_parity": false
        })
        .to_string();
        std::fs::write(tmp.path().join("d.json"), bad).unwrap();
        let err = load_corpus(tmp.path()).unwrap_err();
        assert!(
            matches!(err, CorpusError::DivergenceWithoutDescriptor { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn parity_case_rejects_descriptor() {
        let tmp = tempdir().expect("tempdir");
        let bad = serde_json::json!({
            "name": "p",
            "category": "model_proxy",
            "input": {},
            "expected_parity": true,
            "divergence": {
                "description": "x",
                "go_expected": "a",
                "rust_expected": "b"
            }
        })
        .to_string();
        std::fs::write(tmp.path().join("p.json"), bad).unwrap();
        let err = load_corpus(tmp.path()).unwrap_err();
        assert!(matches!(
            err,
            CorpusError::DescriptorWithoutDivergence { .. }
        ));
    }

    #[test]
    fn order_hint_last_allowed_once() {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("only.json"),
            divergence_case_bytes("only", Some("last")),
        )
        .unwrap();
        let cases = load_corpus(tmp.path()).expect("load");
        assert_eq!(cases[0].order_hint, Some(OrderHint::Last));
    }

    #[test]
    fn order_hint_last_rejected_if_more_than_one() {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("a.json"),
            divergence_case_bytes("a", Some("last")),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("b.json"),
            divergence_case_bytes("b", Some("last")),
        )
        .unwrap();
        let err = load_corpus(tmp.path()).unwrap_err();
        assert!(
            matches!(err, CorpusError::MultipleOrderHintLast { count: 2 }),
            "got {err:?}"
        );
    }

    #[test]
    fn duplicate_name_rejected() {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("a.json"),
            parity_case_bytes("dup", "egress"),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("b.json"),
            parity_case_bytes("dup", "egress"),
        )
        .unwrap();
        let err = load_corpus(tmp.path()).unwrap_err();
        assert!(matches!(err, CorpusError::DuplicateName(_)));
    }

    #[test]
    fn partition_moves_last_to_end() {
        let cases = vec![
            CorpusCase {
                name: "first".to_string(),
                category: Category::ModelProxy,
                description: String::new(),
                input: serde_json::json!({}),
                expected_parity: true,
                divergence: None,
                normalize: NormalizeConfig::default(),
                order_hint: None,
                path: PathBuf::from("first.json"),
            },
            CorpusCase {
                name: "last".to_string(),
                category: Category::Divergence,
                description: String::new(),
                input: serde_json::json!({}),
                expected_parity: false,
                divergence: Some(DivergenceDescriptor {
                    description: "x".into(),
                    go_expected: "a".into(),
                    rust_expected: "b".into(),
                }),
                normalize: NormalizeConfig::default(),
                order_hint: Some(OrderHint::Last),
                path: PathBuf::from("last.json"),
            },
            CorpusCase {
                name: "middle".to_string(),
                category: Category::Egress,
                description: String::new(),
                input: serde_json::json!({}),
                expected_parity: true,
                divergence: None,
                normalize: NormalizeConfig::default(),
                order_hint: None,
                path: PathBuf::from("middle.json"),
            },
        ];
        let (rest, last) = partition_by_order_hint(&cases);
        assert_eq!(rest.len(), 2);
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].name, "last");
        assert!(rest.iter().all(|c| c.order_hint.is_none()));
    }

    #[test]
    fn loader_sorts_cases_by_filename() {
        // Deterministic run order (NFR-6) requires file-name sort.
        let tmp = tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("02_second.json"),
            parity_case_bytes("second", "egress"),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("01_first.json"),
            parity_case_bytes("first", "egress"),
        )
        .unwrap();
        let cases = load_corpus(tmp.path()).expect("load");
        assert_eq!(cases[0].name, "first");
        assert_eq!(cases[1].name, "second");
    }

    #[test]
    fn committed_corpus_parses_and_validates() {
        // Step 5 gate: the committed corpus files under corpus/
        // must all load, pass invariants, and cover every expected
        // category. If this test fails, the corpus JSON is
        // malformed or violates FR-21.
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus");
        let cases = load_corpus(&dir).expect("committed corpus must load");
        assert!(
            cases.len() >= 30,
            "corpus must contain at least 30 cases, got {}",
            cases.len()
        );

        // At least one case per category.
        let mut seen = std::collections::HashSet::new();
        for c in &cases {
            seen.insert(c.category);
        }
        for expected in [
            Category::ModelProxy,
            Category::Egress,
            Category::GitSsh,
            Category::Health,
            Category::Divergence,
        ] {
            assert!(
                seen.contains(&expected),
                "corpus missing category {expected:?}"
            );
        }

        // All five divergence case names from FR-22 are present.
        let names: std::collections::HashSet<&str> =
            cases.iter().map(|c| c.name.as_str()).collect();
        for n in [
            "divergence_sse_streaming_openai",
            "divergence_sse_streaming_anthropic",
            "divergence_bare_exec_upload_pack_rejection",
            "divergence_bare_exec_receive_pack_rejection",
            "divergence_connect_drain_on_sigterm",
        ] {
            assert!(names.contains(n), "corpus missing divergence case {n}");
        }

        // Exactly one order_hint=last case (the drain SIGTERM case).
        let last_count = cases
            .iter()
            .filter(|c| c.order_hint == Some(OrderHint::Last))
            .count();
        assert_eq!(last_count, 1, "expected exactly 1 order_hint=last case");
    }

    #[test]
    fn non_json_files_ignored() {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.json"), parity_case_bytes("a", "egress")).unwrap();
        std::fs::write(tmp.path().join("README.md"), "ignore me").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "ignore me too").unwrap();
        let cases = load_corpus(tmp.path()).expect("load");
        assert_eq!(cases.len(), 1);
    }
}

use anyhow::{bail, ensure, Result};
use scry_client::{Order, QueryKind};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub corpus: Corpus,
    pub cases: Vec<Case>,
    #[serde(default)]
    pub generation: Option<Generation>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    pub seed: String,
    pub snapshot_sha256: String,
    pub cases: Vec<Stratum>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stratum {
    pub case: String,
    pub shape: String,
    pub bucket: String,
    pub matches: u64,
}

/// Operator-supplied facts: never infer record count or delta state from a
/// truncated result page. Unknown facts must be explicitly recorded as such.
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub label: String,
    pub records: Option<u64>,
    pub volumes: Option<u32>,
    pub delta_state: String,
    pub cache_state: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub label: String,
    pub shape: String,
    pub kind: String,
    pub order: String,
    pub patterns: Vec<String>,
    /// Send every request before draining replies (RPC only). This exercises
    /// latest-wins; it does not assert the daemon cancelled any particular scan.
    #[serde(default)]
    pub supersede: bool,
    #[serde(default)]
    pub cadence_ms: u64,
}

impl Case {
    pub fn kind(&self) -> Result<QueryKind> {
        Ok(match self.kind.as_str() {
            "prefix" => QueryKind::Prefix,
            "substring" => QueryKind::Substring,
            "wildcard" => QueryKind::Wildcard,
            "path_terms" => QueryKind::PathTerms,
            _ => bail!("unknown query kind"),
        })
    }

    pub fn order(&self) -> Result<Order> {
        Ok(match self.order.as_str() {
            "relevance" => Order::Relevance,
            "recent" => Order::Recent,
            "largest" => Order::Largest,
            _ => bail!("unknown ordering"),
        })
    }
}

impl Workload {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.cases.is_empty(), "workload has no cases");
        ensure!(self.cases.len() <= 256, "at most 256 cases are supported");
        if let Some(generation) = &self.generation {
            ensure!(
                generation.cases.len() == self.cases.len(),
                "incomplete stratification"
            );
            for (stratum, case) in generation.cases.iter().zip(&self.cases) {
                ensure!(
                    stratum.case == case.label
                        && case.shape == format!("{}/{}", stratum.shape, stratum.bucket)
                        && crate::generate::bucket(stratum.matches) == stratum.bucket,
                    "invalid stratification"
                );
            }
        }
        let mut labels = std::collections::HashSet::new();
        for case in &self.cases {
            ensure!(labels.insert(&case.label), "duplicate case label");
            ensure!(
                !case.label.is_empty() && !case.shape.is_empty(),
                "empty case label or shape"
            );
            ensure!(
                !case.patterns.is_empty() && case.patterns.len() <= 128,
                "expected 1..128 steps per case"
            );
            // Stay below pipe buffers when pipelining, and bound user input.
            ensure!(
                case.patterns.iter().all(|s| s.len() <= 256),
                "query exceeds 256 bytes"
            );
            ensure!(case.cadence_ms <= 10_000, "cadence exceeds 10 seconds");
            case.kind()?;
            case.order()?;
            if case.kind()? == QueryKind::PathTerms {
                for pattern in &case.patterns {
                    ensure!(
                        scry_core::terms::parse_query(pattern, 0).is_ok(),
                        "invalid path-term query"
                    );
                }
            }
        }
        Ok(())
    }
}

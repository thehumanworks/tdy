//! Calibrated thresholds (per judge, per rule) and the labelled examples
//! they are fitted on.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Fitted {
    pub threshold: f64,
    pub precision: f64,
    pub recall: f64,
    pub positives: usize,
    pub n: usize,
    pub at: i64,
}

/// judge id → rule id → fitted threshold.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Calibration(pub BTreeMap<String, BTreeMap<String, Fitted>>);

pub fn calibration_path() -> PathBuf {
    tdy_ops::dirs::config_dir().join("calibration.json")
}

pub fn labels_path() -> PathBuf {
    tdy_ops::dirs::config_dir().join("labels.jsonl")
}

impl Calibration {
    pub fn load() -> Calibration {
        std::fs::read(calibration_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self) -> Result<()> {
        let p = calibration_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
    pub fn get(&self, judge: &str, rule: &str) -> Option<f64> {
        self.0.get(judge).and_then(|m| m.get(rule)).map(|f| f.threshold)
    }
}

/// A human label: which rule (if any) should win for this item.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Label {
    pub path: PathBuf,
    /// Winning rule id, or None for "no rule should apply".
    pub rule: Option<String>,
    /// The evidence text shown to the judge when labelled (reproducible
    /// even if the file later moves).
    pub evidence: String,
    pub at: i64,
}

pub fn load_labels() -> Vec<Label> {
    let Ok(f) = std::fs::File::open(labels_path()) else { return vec![] };
    std::io::BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect()
}

pub fn append_label(l: &Label) -> Result<()> {
    let p = labels_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
    let mut line = serde_json::to_vec(l)?;
    line.push(b'\n');
    f.write_all(&line)?;
    Ok(())
}

/// Smallest threshold whose precision on the labelled set reaches `target`
/// (rule fires when p ≥ τ). `scored` = (probability, is_positive).
/// Returns None when no threshold reaches the target.
pub fn fit_threshold(scored: &[(f64, bool)], target: f64) -> Option<Fitted> {
    let positives = scored.iter().filter(|x| x.1).count();
    if positives == 0 {
        return None;
    }
    let mut cands: Vec<f64> = scored.iter().map(|x| x.0).collect();
    cands.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    cands.dedup();
    for &t in &cands {
        let fired: Vec<&(f64, bool)> = scored.iter().filter(|x| x.0 >= t).collect();
        if fired.is_empty() {
            continue;
        }
        let tp = fired.iter().filter(|x| x.1).count();
        let precision = tp as f64 / fired.len() as f64;
        if precision >= target {
            return Some(Fitted {
                threshold: t,
                precision,
                recall: tp as f64 / positives as f64,
                positives,
                n: scored.len(),
                at: tdy_scan::fmt::now_secs(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fits_minimal_precise_threshold() {
        let s = vec![(0.95, true), (0.9, true), (0.85, false), (0.8, true), (0.4, false), (0.2, false), (0.1, true)];
        let f = fit_threshold(&s, 0.95).unwrap();
        assert!((f.threshold - 0.9).abs() < 1e-9);
        assert!((f.recall - 0.5).abs() < 1e-9);
        let f = fit_threshold(&s, 0.75).unwrap();
        assert!((f.threshold - 0.8).abs() < 1e-9, "{f:?}");
        assert!(fit_threshold(&[(0.5, false)], 0.9).is_none());
    }
}

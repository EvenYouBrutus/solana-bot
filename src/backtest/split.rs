use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Split period assignment for a historical signal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Split {
    Train,
    Validation,
    OutOfSample,
}

impl std::fmt::Display for Split {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Split::Train => write!(f, "train"),
            Split::Validation => write!(f, "validation"),
            Split::OutOfSample => write!(f, "out_of_sample"),
        }
    }
}

/// Configuration for chronological train/validation/OOS splitting.
/// All boundaries are optional; when absent, the corresponding split is empty.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SplitConfig {
    /// End of training period (exclusive). Signals with timestamp < train_end go to Train.
    pub train_end: Option<DateTime<Utc>>,
    /// Start of validation period (inclusive).
    pub validation_start: Option<DateTime<Utc>>,
    /// End of validation period (exclusive).
    pub validation_end: Option<DateTime<Utc>>,
    /// Start of final OOS period (inclusive).
    pub oos_start: Option<DateTime<Utc>>,
}

impl SplitConfig {
    pub fn no_split() -> Self {
        Self {
            train_end: None,
            validation_start: None,
            validation_end: None,
            oos_start: None,
        }
    }

    /// Validate that boundaries are in a consistent order.
    ///
    /// Requires: train_end <= validation_start < validation_end <= oos_start
    /// for every pair of boundaries that are both `Some`.
    pub fn validate_boundaries(&self) -> Result<(), String> {
        if let (Some(te), Some(vs)) = (&self.train_end, &self.validation_start) {
            if te > vs {
                return Err(format!(
                    "train_end ({}) must be <= validation_start ({})",
                    te, vs
                ));
            }
        }
        if let (Some(vs), Some(ve)) = (&self.validation_start, &self.validation_end) {
            if vs >= ve {
                return Err(format!(
                    "validation_start ({}) must be < validation_end ({})",
                    vs, ve
                ));
            }
        }
        if let (Some(ve), Some(os)) = (&self.validation_end, &self.oos_start) {
            if ve > os {
                return Err(format!(
                    "validation_end ({}) must be <= oos_start ({})",
                    ve, os
                ));
            }
        }
        Ok(())
    }
}

/// Describes a signal that was excluded from the experiment because it falls
/// outside all configured split ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitExclusion {
    pub timestamp: DateTime<Utc>,
    pub reason: String,
}

/// Classify a single signal, returning the split assignment and an optional
/// exclusion reason if the signal falls outside all configured ranges.
pub fn classify_split_with_exclusion(
    ts: DateTime<Utc>,
    c: &SplitConfig,
) -> (Split, Option<String>) {
    // OOS: ts >= oos_start (highest priority)
    if let Some(oos_start) = c.oos_start {
        if ts >= oos_start {
            return (Split::OutOfSample, None);
        }
    }
    // OOS fallback: ts >= validation_end when oos_start is not set
    if c.oos_start.is_none() {
        if let Some(val_end) = c.validation_end {
            if ts >= val_end {
                return (Split::OutOfSample, None);
            }
        }
    }
    // Validation: validation_start <= ts < validation_end
    if let (Some(val_start), Some(val_end)) = (&c.validation_start, &c.validation_end) {
        if ts >= *val_start && ts < *val_end {
            return (Split::Validation, None);
        }
    }
    // Gap between validation_end and oos_start (excluded)
    if let (Some(ve), Some(os)) = (&c.validation_end, &c.oos_start) {
        if ts >= *ve && ts < *os {
            return (
                Split::Train,
                Some(format!(
                    "signal at {} falls between validation_end ({}) and oos_start ({})",
                    ts, ve, os
                )),
            );
        }
    }

    // Train: ts < train_end (or ts < validation_start)
    (Split::Train, None)
}

/// Assign each signal to a split based on its timestamp.
///
/// Returns `Err` if the split configuration has invalid boundaries.
pub fn assign_splits(
    timestamps: &[DateTime<Utc>],
    config: &SplitConfig,
) -> Result<Vec<Split>, String> {
    config.validate_boundaries()?;
    Ok(timestamps
        .iter()
        .map(|ts| classify_split_with_exclusion(*ts, config).0)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn explicit_boundaries() {
        let config = SplitConfig {
            train_end: Some(ts("2024-06-01T00:00:00Z")),
            validation_start: Some(ts("2024-06-01T00:00:00Z")),
            validation_end: Some(ts("2024-09-01T00:00:00Z")),
            oos_start: Some(ts("2024-09-01T00:00:00Z")),
        };
        let timestamps = vec![
            ts("2024-03-01T00:00:00Z"),
            ts("2024-05-31T23:59:59Z"),
            ts("2024-06-01T00:00:00Z"),
            ts("2024-08-31T23:59:59Z"),
            ts("2024-09-01T00:00:00Z"),
            ts("2024-12-01T00:00:00Z"),
        ];
        let splits = assign_splits(&timestamps, &config).unwrap();
        assert_eq!(splits[0], Split::Train);
        assert_eq!(splits[1], Split::Train);
        assert_eq!(splits[2], Split::Validation);
        assert_eq!(splits[3], Split::Validation);
        assert_eq!(splits[4], Split::OutOfSample);
        assert_eq!(splits[5], Split::OutOfSample);
    }

    #[test]
    fn no_split_sends_everything_to_train() {
        let config = SplitConfig::no_split();
        let timestamps = vec![ts("2024-01-01T00:00:00Z"), ts("2024-12-31T00:00:00Z")];
        let splits = assign_splits(&timestamps, &config).unwrap();
        assert_eq!(splits[0], Split::Train);
        assert_eq!(splits[1], Split::Train);
    }

    #[test]
    fn only_oos_boundary() {
        let config = SplitConfig {
            train_end: None,
            validation_start: None,
            validation_end: None,
            oos_start: Some(ts("2024-09-01T00:00:00Z")),
        };
        let timestamps = vec![ts("2024-06-01T00:00:00Z"), ts("2024-09-01T00:00:00Z")];
        let splits = assign_splits(&timestamps, &config).unwrap();
        assert_eq!(splits[0], Split::Train);
        assert_eq!(splits[1], Split::OutOfSample);
    }

    #[test]
    fn validation_end_without_oos_start_sends_to_oos() {
        let config = SplitConfig {
            train_end: None,
            validation_start: Some(ts("2024-06-01T00:00:00Z")),
            validation_end: Some(ts("2024-09-01T00:00:00Z")),
            oos_start: None,
        };
        let timestamps = vec![
            ts("2024-05-01T00:00:00Z"),
            ts("2024-07-01T00:00:00Z"),
            ts("2024-09-01T00:00:00Z"),
        ];
        let splits = assign_splits(&timestamps, &config).unwrap();
        assert_eq!(splits[0], Split::Train);
        assert_eq!(splits[1], Split::Validation);
        assert_eq!(splits[2], Split::OutOfSample);
    }

    #[test]
    fn invalid_validation_end_before_validation_start() {
        let config = SplitConfig {
            train_end: None,
            validation_start: Some(ts("2024-09-01T00:00:00Z")),
            validation_end: Some(ts("2024-06-01T00:00:00Z")),
            oos_start: None,
        };
        let result = assign_splits(&[], &config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("validation_start"), "{}", err);
        assert!(err.contains("validation_end"), "{}", err);
    }

    #[test]
    fn invalid_oos_start_before_validation_end() {
        let config = SplitConfig {
            train_end: None,
            validation_start: Some(ts("2024-01-01T00:00:00Z")),
            validation_end: Some(ts("2024-09-01T00:00:00Z")),
            oos_start: Some(ts("2024-06-01T00:00:00Z")),
        };
        let result = assign_splits(&[], &config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("validation_end"), "{}", err);
        assert!(err.contains("oos_start"), "{}", err);
    }

    #[test]
    fn invalid_train_end_after_validation_start() {
        let config = SplitConfig {
            train_end: Some(ts("2024-09-01T00:00:00Z")),
            validation_start: Some(ts("2024-06-01T00:00:00Z")),
            validation_end: None,
            oos_start: None,
        };
        let result = assign_splits(&[], &config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("train_end"), "{}", err);
        assert!(err.contains("validation_start"), "{}", err);
    }

    #[test]
    fn signal_in_gap_between_validation_end_and_oos_start() {
        let config = SplitConfig {
            train_end: Some(ts("2024-06-01T00:00:00Z")),
            validation_start: Some(ts("2024-06-01T00:00:00Z")),
            validation_end: Some(ts("2024-09-01T00:00:00Z")),
            oos_start: Some(ts("2024-10-01T00:00:00Z")),
        };
        let timestamps = vec![
            ts("2024-05-01T00:00:00Z"),
            ts("2024-07-01T00:00:00Z"),
            ts("2024-09-15T00:00:00Z"), // gap
            ts("2024-10-01T00:00:00Z"),
        ];
        let splits = assign_splits(&timestamps, &config).unwrap();
        assert_eq!(splits[0], Split::Train);
        assert_eq!(splits[1], Split::Validation);
        assert_eq!(splits[2], Split::Train); // gap → falls to Train
        assert_eq!(splits[3], Split::OutOfSample);

        // classify_split_with_exclusion reports the gap
        let (_, exclusion) = classify_split_with_exclusion(ts("2024-09-15T00:00:00Z"), &config);
        assert!(exclusion.is_some());
        assert!(exclusion.unwrap().contains("between validation_end"));
    }

    #[test]
    fn empty_config_is_valid() {
        let config = SplitConfig::no_split();
        assert!(config.validate_boundaries().is_ok());
    }

    #[test]
    fn validate_boundaries_method_directly() {
        let valid = SplitConfig {
            train_end: Some(ts("2024-01-01T00:00:00Z")),
            validation_start: Some(ts("2024-06-01T00:00:00Z")),
            validation_end: Some(ts("2024-09-01T00:00:00Z")),
            oos_start: Some(ts("2024-09-01T00:00:00Z")),
        };
        assert!(valid.validate_boundaries().is_ok());

        let invalid = SplitConfig {
            train_end: None,
            validation_start: Some(ts("2024-09-01T00:00:00Z")),
            validation_end: Some(ts("2024-06-01T00:00:00Z")),
            oos_start: None,
        };
        assert!(invalid.validate_boundaries().is_err());
    }
}

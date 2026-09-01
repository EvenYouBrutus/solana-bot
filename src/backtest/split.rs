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
}

/// Assign each signal to a split based on its timestamp.
pub fn assign_splits(timestamps: &[DateTime<Utc>], config: &SplitConfig) -> Vec<Split> {
    timestamps
        .iter()
        .map(|ts| classify_split(*ts, config))
        .collect()
}

fn classify_split(ts: DateTime<Utc>, c: &SplitConfig) -> Split {
    // OOS: ts >= oos_start (highest priority)
    if let Some(oos_start) = c.oos_start {
        if ts >= oos_start {
            return Split::OutOfSample;
        }
    }
    // OOS fallback: ts >= validation_end when oos_start is not set
    if c.oos_start.is_none() {
        if let Some(val_end) = c.validation_end {
            if ts >= val_end {
                return Split::OutOfSample;
            }
        }
    }
    // Validation: validation_start <= ts < validation_end
    if let (Some(val_start), Some(val_end)) = (&c.validation_start, &c.validation_end) {
        if ts >= *val_start && ts < *val_end {
            return Split::Validation;
        }
    }
    // Train: ts < train_end (or ts < validation_start)
    Split::Train
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
        let splits = assign_splits(&timestamps, &config);
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
        let splits = assign_splits(&timestamps, &config);
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
        let splits = assign_splits(&timestamps, &config);
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
        let splits = assign_splits(&timestamps, &config);
        assert_eq!(splits[0], Split::Train);
        assert_eq!(splits[1], Split::Validation);
        assert_eq!(splits[2], Split::OutOfSample);
    }
}

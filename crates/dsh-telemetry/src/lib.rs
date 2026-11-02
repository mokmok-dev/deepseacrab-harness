#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Local run telemetry for the harness.
//!
//! Every recorded run bumps `runs` and exactly one outcome counter. The
//! counters live in one JSON file under the harness state directory, so a
//! later process reads the same numbers back.

use dsh_core::TurnOutcome;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Counters describing every run this installation has attempted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    /// Runs recorded, whether or not a turn settled.
    pub runs: u64,
    /// Runs whose turn completed normally.
    pub completed: u64,
    /// Runs that failed, including those that never settled a turn.
    pub failed: u64,
    /// Runs whose turn was cancelled.
    pub cancelled: u64,
    /// Runs whose turn ran out of step budget.
    pub step_limit: u64,
}

/// Telemetry file failures.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// The file could not be read.
    #[error("cannot read telemetry file {path}: {source}")]
    Read {
        /// Path that failed.
        path: PathBuf,
        /// Operating system failure.
        source: io::Error,
    },
    /// The file or its temporary sibling could not be written.
    #[error("cannot write telemetry file {path}: {source}")]
    Write {
        /// Path that failed.
        path: PathBuf,
        /// Operating system failure.
        source: io::Error,
    },
    /// The file does not hold a telemetry snapshot.
    #[error("telemetry file encoding failure: {0}")]
    Encoding(String),
}

/// A JSON file holding the run counters.
pub struct Telemetry {
    path: PathBuf,
}

impl Telemetry {
    /// Wrap a path. The file need not exist yet.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The wrapped path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the counters. A missing file is zero counters.
    pub fn load(&self) -> Result<TelemetrySnapshot, TelemetryError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                TelemetryError::Encoding(format!("{}: {error}", self.path.display()))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(TelemetrySnapshot::default())
            }
            Err(source) => Err(TelemetryError::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Count one settled turn under its outcome.
    pub fn record(&self, outcome: &TurnOutcome) -> Result<TelemetrySnapshot, TelemetryError> {
        self.update(|snapshot| {
            snapshot.runs = bumped(snapshot.runs);
            let counter = match outcome {
                TurnOutcome::Completed => &mut snapshot.completed,
                TurnOutcome::Cancelled { .. } => &mut snapshot.cancelled,
                TurnOutcome::Failed { .. } => &mut snapshot.failed,
                TurnOutcome::StepLimit => &mut snapshot.step_limit,
            };
            *counter = bumped(*counter);
        })
    }

    /// Count a run that failed before any turn settled.
    pub fn record_failure(&self) -> Result<TelemetrySnapshot, TelemetryError> {
        self.update(|snapshot| {
            snapshot.runs = bumped(snapshot.runs);
            snapshot.failed = bumped(snapshot.failed);
        })
    }

    fn update(
        &self,
        apply: impl FnOnce(&mut TelemetrySnapshot),
    ) -> Result<TelemetrySnapshot, TelemetryError> {
        let mut snapshot = self.load()?;
        apply(&mut snapshot);
        self.save(&snapshot)?;
        Ok(snapshot)
    }

    fn save(&self, snapshot: &TelemetrySnapshot) -> Result<(), TelemetryError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| TelemetryError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let encoded = serde_json::to_vec_pretty(snapshot)
            .map_err(|error| TelemetryError::Encoding(error.to_string()))?;
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, encoded).map_err(|source| TelemetryError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &self.path).map_err(|source| TelemetryError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

const fn bumped(count: u64) -> u64 {
    count.saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn telemetry(directory: &tempfile::TempDir) -> Telemetry {
        Telemetry::new(directory.path().join("telemetry.json"))
    }

    fn write_telemetry(path: &Path, snapshot: &TelemetrySnapshot) {
        fs::create_dir_all(path.parent().expect("path has a parent")).expect("mkdir");
        fs::write(path, serde_json::to_vec(snapshot).expect("encode")).expect("write");
    }

    #[test]
    fn an_absent_file_loads_as_zero_counters() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = telemetry(&directory);
        assert_eq!(store.load().expect("load"), TelemetrySnapshot::default());
        assert!(!store.path().exists());
        assert_eq!(store.path(), directory.path().join("telemetry.json"));
    }

    #[test]
    fn each_outcome_counts_its_own_run() {
        let cases = [
            (
                TurnOutcome::Completed,
                TelemetrySnapshot {
                    runs: 1,
                    completed: 1,
                    ..TelemetrySnapshot::default()
                },
            ),
            (
                TurnOutcome::Cancelled {
                    reason: "user".to_string(),
                },
                TelemetrySnapshot {
                    runs: 1,
                    cancelled: 1,
                    ..TelemetrySnapshot::default()
                },
            ),
            (
                TurnOutcome::Failed {
                    code: "PROVIDER".to_string(),
                    message: "boom".to_string(),
                },
                TelemetrySnapshot {
                    runs: 1,
                    failed: 1,
                    ..TelemetrySnapshot::default()
                },
            ),
            (
                TurnOutcome::StepLimit,
                TelemetrySnapshot {
                    runs: 1,
                    step_limit: 1,
                    ..TelemetrySnapshot::default()
                },
            ),
        ];
        for (outcome, expected) in cases {
            let directory = tempfile::tempdir().expect("tempdir");
            let store = telemetry(&directory);
            assert_eq!(store.record(&outcome).expect("record"), expected);
            assert_eq!(store.load().expect("load"), expected);
        }
    }

    #[test]
    fn a_failed_launch_counts_as_one_failed_run() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = telemetry(&directory);
        let recorded = store.record_failure().expect("record");
        assert_eq!(
            recorded,
            TelemetrySnapshot {
                runs: 1,
                failed: 1,
                ..TelemetrySnapshot::default()
            }
        );
        assert_eq!(telemetry(&directory).load().expect("load"), recorded);
    }

    #[test]
    fn repeated_records_accumulate_across_processes() {
        let directory = tempfile::tempdir().expect("tempdir");
        telemetry(&directory)
            .record(&TurnOutcome::Completed)
            .expect("first");
        telemetry(&directory)
            .record(&TurnOutcome::StepLimit)
            .expect("second");
        let store = telemetry(&directory);
        assert_eq!(
            store.record(&TurnOutcome::Completed).expect("third"),
            TelemetrySnapshot {
                runs: 3,
                completed: 2,
                step_limit: 1,
                ..TelemetrySnapshot::default()
            }
        );
        assert_eq!(
            store.record_failure().expect("failure"),
            TelemetrySnapshot {
                runs: 4,
                completed: 2,
                failed: 1,
                step_limit: 1,
                ..TelemetrySnapshot::default()
            }
        );
        assert_eq!(telemetry(&directory).load().expect("load").runs, 4);
    }

    #[test]
    fn malformed_json_is_rejected_and_left_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = telemetry(&directory);
        let broken = b"{not json";
        fs::write(store.path(), broken).expect("write");
        assert!(matches!(store.load(), Err(TelemetryError::Encoding(_))));
        assert!(matches!(
            store.record(&TurnOutcome::Completed),
            Err(TelemetryError::Encoding(_))
        ));
        assert!(matches!(
            store.record_failure(),
            Err(TelemetryError::Encoding(_))
        ));
        assert_eq!(fs::read(store.path()).expect("read"), broken);
        assert!(!temporary_path(store.path()).exists());
    }

    #[test]
    fn invalid_counts_are_rejected_and_left_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = telemetry(&directory);
        for broken in [
            br#"{"runs":-1,"completed":0,"failed":0,"cancelled":0,"step_limit":0}"#.as_slice(),
            br#"{"runs":1.5,"completed":0,"failed":0,"cancelled":0,"step_limit":0}"#.as_slice(),
            br#"{"runs":1}"#.as_slice(),
            b"[]".as_slice(),
        ] {
            fs::write(store.path(), broken).expect("write");
            assert!(matches!(store.load(), Err(TelemetryError::Encoding(_))));
            assert_eq!(fs::read(store.path()).expect("read"), broken);
        }
    }

    #[test]
    fn unreadable_paths_are_read_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("directory");
        fs::create_dir(&path).expect("mkdir");
        let store = Telemetry::new(&path);
        assert!(matches!(store.load(), Err(TelemetryError::Read { .. })));
    }

    #[test]
    fn the_error_wraps_as_a_send_sync_std_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("directory");
        fs::create_dir(&path).expect("mkdir");
        let error = Telemetry::new(path)
            .load()
            .expect_err("directory is unreadable");
        let wrapped: Box<dyn std::error::Error + Send + Sync> = Box::new(error);
        assert!(
            wrapped.to_string().contains("cannot read telemetry file"),
            "unexpected error text: {wrapped}"
        );
    }

    #[test]
    fn saving_is_atomic_and_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = telemetry(&directory);
        store.record(&TurnOutcome::Completed).expect("first");
        store.record(&TurnOutcome::StepLimit).expect("second");
        let names: Vec<String> = fs::read_dir(directory.path())
            .expect("read_dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["telemetry.json".to_string()]);
        assert!(!temporary_path(store.path()).exists());
    }

    #[test]
    fn recording_creates_the_parent_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = Telemetry::new(
            directory
                .path()
                .join("state")
                .join("nested")
                .join("telemetry.json"),
        );
        store.record(&TurnOutcome::Completed).expect("record");
        assert!(store.path().exists());
        assert_eq!(store.load().expect("load").runs, 1);
    }

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = telemetry(&directory);
        let saturated = TelemetrySnapshot {
            runs: u64::MAX,
            completed: u64::MAX,
            failed: u64::MAX,
            cancelled: u64::MAX,
            step_limit: 0,
        };
        write_telemetry(store.path(), &saturated);
        assert_eq!(
            store.record(&TurnOutcome::Completed).expect("completed"),
            saturated
        );
        assert_eq!(store.record_failure().expect("failure"), saturated);
        assert_eq!(
            store
                .record(&TurnOutcome::Cancelled {
                    reason: "user".to_string(),
                })
                .expect("cancelled"),
            saturated
        );
        assert_eq!(
            store.record(&TurnOutcome::StepLimit).expect("step limit"),
            TelemetrySnapshot {
                step_limit: 1,
                ..saturated
            }
        );
    }

    #[test]
    fn one_saturated_counter_does_not_stop_the_others() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = telemetry(&directory);
        write_telemetry(
            store.path(),
            &TelemetrySnapshot {
                runs: u64::MAX,
                ..TelemetrySnapshot::default()
            },
        );
        assert_eq!(
            store.record(&TurnOutcome::Completed).expect("record"),
            TelemetrySnapshot {
                runs: u64::MAX,
                completed: 1,
                ..TelemetrySnapshot::default()
            }
        );
    }

    #[test]
    fn the_file_keeps_its_json_field_names() {
        let snapshot = TelemetrySnapshot {
            runs: 1,
            completed: 2,
            failed: 3,
            cancelled: 4,
            step_limit: 5,
        };
        let encoded = serde_json::to_value(&snapshot).expect("serialize");
        assert_eq!(
            encoded,
            json!({
                "runs": 1,
                "completed": 2,
                "failed": 3,
                "cancelled": 4,
                "step_limit": 5,
            })
        );
        assert_eq!(
            serde_json::from_value::<TelemetrySnapshot>(encoded).expect("deserialize"),
            snapshot
        );
    }

    fn arbitrary_outcome() -> impl Strategy<Value = TurnOutcome> {
        prop_oneof![
            Just(TurnOutcome::Completed),
            any::<String>().prop_map(|reason| TurnOutcome::Cancelled { reason }),
            (any::<String>(), any::<String>())
                .prop_map(|(code, message)| TurnOutcome::Failed { code, message }),
            Just(TurnOutcome::StepLimit),
        ]
    }

    proptest! {
        #[test]
        fn every_run_lands_in_exactly_one_outcome(
            outcomes in prop::collection::vec(arbitrary_outcome(), 0..32)
        ) {
            let directory = tempfile::tempdir().expect("tempdir");
            let store = telemetry(&directory);
            for outcome in &outcomes {
                store.record(outcome).expect("record");
            }
            let snapshot = store.load().expect("load");
            let runs = u64::try_from(outcomes.len()).expect("fits");
            let completed = outcomes.iter().filter(|outcome| matches!(outcome, TurnOutcome::Completed)).count();
            let cancelled = outcomes.iter().filter(|outcome| matches!(outcome, TurnOutcome::Cancelled { .. })).count();
            let failed = outcomes.iter().filter(|outcome| matches!(outcome, TurnOutcome::Failed { .. })).count();
            let step_limit = outcomes.iter().filter(|outcome| matches!(outcome, TurnOutcome::StepLimit)).count();
            prop_assert_eq!(
                &snapshot,
                &TelemetrySnapshot {
                    runs,
                    completed: u64::try_from(completed).expect("fits"),
                    failed: u64::try_from(failed).expect("fits"),
                    cancelled: u64::try_from(cancelled).expect("fits"),
                    step_limit: u64::try_from(step_limit).expect("fits"),
                }
            );
            let settled = u128::from(snapshot.completed)
                + u128::from(snapshot.failed)
                + u128::from(snapshot.cancelled)
                + u128::from(snapshot.step_limit);
            prop_assert_eq!(u128::from(snapshot.runs), settled);
            prop_assert!(snapshot.completed <= runs);
            prop_assert!(snapshot.failed <= runs);
            prop_assert!(snapshot.cancelled <= runs);
            prop_assert!(snapshot.step_limit <= runs);
        }
    }
}

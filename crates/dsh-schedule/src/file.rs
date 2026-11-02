//! Durable recurring follow-ups persisted as one JSON file.

use dsh_agent::AgentError;
use dsh_core::SessionId;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

/// Largest accepted interval, one year in seconds.
pub const MAX_INTERVAL_SECONDS: u64 = 31_536_000;

const MILLIS_PER_SECOND: u64 = 1_000;

/// One durable recurring follow-up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Identifier minted by [`ScheduleFile::create`].
    pub id: String,
    /// Prompt replayed on every due run.
    pub task: String,
    /// Seconds between the start of one run and the next due time.
    pub interval_seconds: u64,
    /// Session the run appends to; a fresh one is generated when absent.
    pub session_id: Option<SessionId>,
    /// Working directory recorded when the schedule was created.
    pub cwd: String,
    /// Unix epoch milliseconds of the next due run.
    pub next_run_at: i64,
    /// Whether [`run_due`](crate::run_due) picks the schedule up.
    pub enabled: bool,
}

/// Schedule file failures.
#[derive(Debug, Error)]
pub enum ScheduleError {
    /// The file could not be read.
    #[error("cannot read schedule file {path}: {source}")]
    Read {
        /// Path that failed.
        path: PathBuf,
        /// Operating system failure.
        source: io::Error,
    },
    /// The file or its temporary sibling could not be written.
    #[error("cannot write schedule file {path}: {source}")]
    Write {
        /// Path that failed.
        path: PathBuf,
        /// Operating system failure.
        source: io::Error,
    },
    /// The file does not hold a schedule array.
    #[error("schedule file encoding failure: {0}")]
    Encoding(String),
    /// A caller-supplied value or stored schedule is not usable.
    #[error("invalid schedule: {0}")]
    Invalid(String),
    /// A due time cannot be represented.
    #[error("schedule time overflow")]
    Overflow,
    /// The scheduled turn could not be set up or recorded.
    #[error(transparent)]
    Agent(#[from] AgentError),
}

/// A JSON file holding the durable schedule list.
pub struct ScheduleFile {
    path: PathBuf,
}

impl ScheduleFile {
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

    /// Read every schedule. A missing file is an empty list.
    pub fn load(&self) -> Result<Vec<Schedule>, ScheduleError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                ScheduleError::Encoding(format!("{}: {error}", self.path.display()))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(ScheduleError::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Replace the file with `schedules`.
    ///
    /// The list is written to a sibling temporary file and renamed, so a
    /// reader never observes a half-written array.
    pub fn save(&self, schedules: &[Schedule]) -> Result<(), ScheduleError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| ScheduleError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let encoded = serde_json::to_vec_pretty(schedules)
            .map_err(|error| ScheduleError::Encoding(error.to_string()))?;
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, encoded).map_err(|source| ScheduleError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &self.path).map_err(|source| ScheduleError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// Append one schedule and return it with its first due time.
    pub fn create(
        &self,
        task: &str,
        interval_seconds: u64,
        session_id: Option<SessionId>,
        cwd: String,
        now: i64,
    ) -> Result<Schedule, ScheduleError> {
        validate_task(task)?;
        validate_interval(interval_seconds)?;
        let schedule = Schedule {
            id: format!("schedule-{}", Uuid::new_v4()),
            task: task.to_string(),
            interval_seconds,
            session_id,
            cwd,
            next_run_at: next_run_at(now, interval_seconds)?,
            enabled: true,
        };
        let mut schedules = self.load()?;
        schedules.push(schedule.clone());
        self.save(&schedules)?;
        Ok(schedule)
    }

    /// Every stored schedule in file order.
    pub fn list(&self) -> Result<Vec<Schedule>, ScheduleError> {
        self.load()
    }

    /// Remove one schedule. Returns whether a schedule was removed.
    pub fn cancel(&self, id: &str) -> Result<bool, ScheduleError> {
        let mut schedules = self.load()?;
        let Some(index) = schedules.iter().position(|schedule| schedule.id == id) else {
            return Ok(false);
        };
        schedules.remove(index);
        self.save(&schedules)?;
        Ok(true)
    }

    /// Enabled schedules whose due time is at or before `now`.
    pub fn due(&self, now: i64) -> Result<Vec<Schedule>, ScheduleError> {
        Ok(self
            .load()?
            .into_iter()
            .filter(|schedule| schedule.enabled && schedule.next_run_at <= now)
            .collect())
    }

    /// Move one schedule's due time to `now` plus its interval.
    ///
    /// Returns the updated schedule, or `None` when the id is unknown.
    pub fn advance(&self, id: &str, now: i64) -> Result<Option<Schedule>, ScheduleError> {
        let mut schedules = self.load()?;
        let Some(schedule) = schedules.iter_mut().find(|schedule| schedule.id == id) else {
            return Ok(None);
        };
        schedule.next_run_at = next_run_at(now, schedule.interval_seconds)?;
        let advanced = schedule.clone();
        self.save(&schedules)?;
        Ok(Some(advanced))
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

fn validate_task(task: &str) -> Result<(), ScheduleError> {
    if task.trim().is_empty() {
        return Err(ScheduleError::Invalid("task must not be empty".to_string()));
    }
    Ok(())
}

fn validate_interval(interval_seconds: u64) -> Result<(), ScheduleError> {
    if interval_seconds == 0 {
        return Err(ScheduleError::Invalid(
            "interval_seconds must be at least 1".to_string(),
        ));
    }
    if interval_seconds > MAX_INTERVAL_SECONDS {
        return Err(ScheduleError::Invalid(format!(
            "interval_seconds must be at most {MAX_INTERVAL_SECONDS}"
        )));
    }
    Ok(())
}

fn next_run_at(now: i64, interval_seconds: u64) -> Result<i64, ScheduleError> {
    let interval_millis = interval_seconds
        .checked_mul(MILLIS_PER_SECOND)
        .ok_or(ScheduleError::Overflow)?;
    let interval_millis = i64::try_from(interval_millis).map_err(|_| ScheduleError::Overflow)?;
    now.checked_add(interval_millis)
        .ok_or(ScheduleError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn file(directory: &tempfile::TempDir) -> ScheduleFile {
        ScheduleFile::new(directory.path().join("schedules.json"))
    }

    fn sample(interval_seconds: u64, now: i64) -> Schedule {
        Schedule {
            id: format!("schedule-{}", Uuid::new_v4()),
            task: "ping".to_string(),
            interval_seconds,
            session_id: None,
            cwd: "/work".to_string(),
            next_run_at: now + 1_000,
            enabled: true,
        }
    }

    #[test]
    fn an_absent_file_loads_as_an_empty_list() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        assert_eq!(file.load().expect("load"), Vec::new());
        assert_eq!(file.list().expect("list"), Vec::new());
        assert_eq!(file.due(0).expect("due"), Vec::new());
        assert_eq!(file.path(), directory.path().join("schedules.json"));
    }

    #[test]
    fn created_schedules_are_listed_with_their_first_due_time() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let schedule = file
            .create("ping", 60, None, "/work".to_string(), 1_000_000)
            .expect("create");
        assert!(schedule.id.starts_with("schedule-"));
        assert_eq!(schedule.task, "ping");
        assert_eq!(schedule.interval_seconds, 60);
        assert_eq!(schedule.cwd, "/work");
        assert_eq!(schedule.next_run_at, 1_060_000);
        assert!(schedule.enabled);
        assert_eq!(file.list().expect("list"), vec![schedule]);
    }

    #[test]
    fn create_makes_a_missing_state_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = ScheduleFile::new(directory.path().join("state").join("schedules.json"));
        file.create("ping", 1, None, "/work".to_string(), 0)
            .expect("create");
        assert!(file.path().exists());
    }

    #[test]
    fn cancelling_removes_only_the_named_schedule() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let first = file
            .create("first", 60, None, "/work".to_string(), 0)
            .expect("first");
        let second = file
            .create("second", 60, None, "/work".to_string(), 0)
            .expect("second");
        assert!(file.cancel(&first.id).expect("cancel"));
        assert_eq!(file.list().expect("list"), vec![second]);
        assert!(!file.cancel(&first.id).expect("cancel missing"));
        assert!(!file.cancel("unknown").expect("cancel unknown"));
    }

    #[test]
    fn due_selects_enabled_schedules_at_or_before_now() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let due = file
            .create("due", 60, None, "/work".to_string(), 1_000)
            .expect("due");
        let future = file
            .create("future", 3_600, None, "/work".to_string(), 1_000)
            .expect("future");
        assert_eq!(file.due(60_999).expect("due"), Vec::new());
        assert_eq!(
            file.due(61_000).expect("due"),
            vec![due],
            "a schedule due exactly now is due"
        );
        assert_eq!(file.due(i64::MAX).expect("due").len(), 2);
        assert!(file.due(i64::MAX).expect("due").contains(&future));
    }

    #[test]
    fn disabled_schedules_are_never_due() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let mut schedule = sample(60, 0);
        schedule.enabled = false;
        file.save(std::slice::from_ref(&schedule)).expect("save");
        assert_eq!(file.due(i64::MAX).expect("due"), Vec::new());
    }

    #[test]
    fn advancing_moves_the_due_time_and_keeps_the_rest() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let schedule = file
            .create("ping", 60, None, "/work".to_string(), 1_000)
            .expect("create");
        let advanced = file
            .advance(&schedule.id, 1_060_000)
            .expect("advance")
            .expect("present");
        assert_eq!(advanced.next_run_at, 1_120_000);
        assert_eq!(advanced.id, schedule.id);
        assert_eq!(advanced.task, schedule.task);
        assert_eq!(advanced.cwd, schedule.cwd);
        assert_eq!(advanced.enabled, schedule.enabled);
        assert_eq!(file.list().expect("list"), vec![advanced]);
        assert_eq!(file.advance("unknown", 0).expect("advance missing"), None);
    }

    #[test]
    fn advancing_without_a_match_leaves_the_file_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let schedule = file
            .create("ping", 60, None, "/work".to_string(), 1_000)
            .expect("create");
        let before = fs::read(file.path()).expect("read");
        assert_eq!(file.advance("unknown", 5).expect("advance"), None);
        assert_eq!(fs::read(file.path()).expect("read"), before);
        assert_eq!(file.list().expect("list"), vec![schedule]);
    }

    #[test]
    fn tasks_and_intervals_are_validated() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        for task in ["", " ", "\t\n"] {
            assert!(matches!(
                file.create(task, 60, None, "/work".to_string(), 0),
                Err(ScheduleError::Invalid(_))
            ));
        }
        assert!(matches!(
            file.create("ping", 0, None, "/work".to_string(), 0),
            Err(ScheduleError::Invalid(_))
        ));
        assert!(matches!(
            file.create(
                "ping",
                MAX_INTERVAL_SECONDS + 1,
                None,
                "/work".to_string(),
                0
            ),
            Err(ScheduleError::Invalid(_))
        ));
        assert!(
            file.create("ping", MAX_INTERVAL_SECONDS, None, "/work".to_string(), 0)
                .is_ok()
        );
        assert!(file.create("ping", 1, None, "/work".to_string(), 0).is_ok());
    }

    #[test]
    fn malformed_json_is_an_encoding_error_and_is_left_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let broken = b"{not json";
        fs::write(file.path(), broken).expect("write");
        assert!(matches!(file.load(), Err(ScheduleError::Encoding(_))));
        assert!(matches!(
            file.create("ping", 60, None, "/work".to_string(), 0),
            Err(ScheduleError::Encoding(_))
        ));
        assert_eq!(fs::read(file.path()).expect("read"), broken);
        assert!(matches!(file.due(0), Err(ScheduleError::Encoding(_))));
        assert!(matches!(
            file.cancel("schedule-1"),
            Err(ScheduleError::Encoding(_))
        ));
        assert!(matches!(
            file.advance("schedule-1", 0),
            Err(ScheduleError::Encoding(_))
        ));
    }

    #[test]
    fn a_json_value_that_is_not_a_schedule_array_is_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        fs::write(file.path(), b"{\"schedules\": []}").expect("write");
        assert!(matches!(file.load(), Err(ScheduleError::Encoding(_))));
    }

    #[test]
    fn saving_is_atomic_and_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let schedules = vec![sample(60, 0), sample(120, 0)];
        file.save(&schedules).expect("save");
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
        assert_eq!(names, vec!["schedules.json".to_string()]);
        assert_eq!(file.load().expect("load"), schedules);
        let mut replaced = schedules;
        replaced[0].task = "replaced".to_string();
        file.save(&replaced).expect("save again");
        assert_eq!(file.load().expect("load"), replaced);
        assert!(!temporary_path(file.path()).exists());
    }

    #[test]
    fn unreadable_paths_are_read_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("directory");
        fs::create_dir(&path).expect("mkdir");
        let file = ScheduleFile::new(&path);
        assert!(matches!(file.load(), Err(ScheduleError::Read { .. })));
    }

    #[test]
    fn unwritable_paths_are_write_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let blocker = directory.path().join("blocker");
        fs::write(&blocker, b"").expect("write");
        let file = ScheduleFile::new(blocker.join("schedules.json"));
        assert!(matches!(
            file.save(&[sample(60, 0)]),
            Err(ScheduleError::Write { .. })
        ));
    }

    #[test]
    fn due_time_overflow_is_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        assert!(matches!(
            file.create("ping", 60, None, "/work".to_string(), i64::MAX),
            Err(ScheduleError::Overflow)
        ));

        let huge = sample(u64::MAX, 0);
        file.save(std::slice::from_ref(&huge)).expect("save");
        assert!(matches!(
            file.advance(&huge.id, 0),
            Err(ScheduleError::Overflow)
        ));

        let wide = sample(10_000_000_000_000_000, 0);
        file.save(std::slice::from_ref(&wide)).expect("save");
        assert!(matches!(
            file.advance(&wide.id, 0),
            Err(ScheduleError::Overflow)
        ));
    }

    fn arbitrary_schedule() -> impl Strategy<Value = Schedule> {
        (
            "[A-Za-z0-9_-]{1,40}",
            "[ -~]{0,80}",
            1u64..=MAX_INTERVAL_SECONDS,
            proptest::option::of("[A-Za-z0-9_-]{1,40}"),
            "[A-Za-z0-9_/.-]{0,40}",
            any::<i64>(),
            any::<bool>(),
        )
            .prop_map(
                |(id, task, interval_seconds, session_id, cwd, next_run_at, enabled)| Schedule {
                    id,
                    task,
                    interval_seconds,
                    session_id: session_id
                        .map(|raw| SessionId::parse(raw).expect("generated ids are valid")),
                    cwd,
                    next_run_at,
                    enabled,
                },
            )
    }

    proptest! {
        #[test]
        fn schedule_files_round_trip(schedules in prop::collection::vec(arbitrary_schedule(), 0..8)) {
            let directory = tempfile::tempdir().expect("tempdir");
            let file = file(&directory);
            file.save(&schedules).expect("save");
            prop_assert_eq!(file.load().expect("load"), schedules);
        }

        #[test]
        fn created_due_times_follow_the_interval(
            interval in 1u64..=MAX_INTERVAL_SECONDS,
            now in -1_000_000_000_000i64..1_000_000_000_000,
        ) {
            let directory = tempfile::tempdir().expect("tempdir");
            let file = file(&directory);
            let schedule = file
                .create("ping", interval, None, "/work".to_string(), now)
                .expect("create");
            let delta = i64::try_from(interval * MILLIS_PER_SECOND).expect("fits");
            prop_assert_eq!(schedule.next_run_at, now + delta);
            prop_assert_eq!(file.due(schedule.next_run_at - 1).expect("due"), Vec::new());
            prop_assert_eq!(
                file.due(schedule.next_run_at).expect("due"),
                vec![schedule]
            );
        }
    }
}

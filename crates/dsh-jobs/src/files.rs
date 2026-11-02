//! The job directory layout and the id rules that keep every job path inside it.

use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// Directory under the session working directory that holds job files.
pub const JOB_DIR: &str = ".dsh-jobs";

/// Suffix of the file recording a live or finished job's process id.
pub const PID_SUFFIX: &str = ".pid";

/// Whether `id` may be used as a job file stem.
///
/// The validator is the whole traversal guard for `job_output` and `job_kill`:
/// a token of ASCII alphanumerics, `-`, and `_` cannot name a parent directory
/// or a nested path.
#[must_use]
pub fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Paths to the four files that make up one job.
pub struct JobPaths {
    directory: PathBuf,
    id: String,
}

impl JobPaths {
    pub fn new(directory: &Path, id: &str) -> Self {
        Self {
            directory: directory.to_path_buf(),
            id: id.to_string(),
        }
    }

    fn file(&self, suffix: &str) -> PathBuf {
        self.directory.join(format!("{}{suffix}", self.id))
    }

    #[must_use]
    pub fn stdout(&self) -> PathBuf {
        self.file(".out")
    }

    #[must_use]
    pub fn stderr(&self) -> PathBuf {
        self.file(".err")
    }

    #[must_use]
    pub fn pid(&self) -> PathBuf {
        self.file(PID_SUFFIX)
    }

    #[must_use]
    pub fn status(&self) -> PathBuf {
        self.file(".status")
    }
}

/// The recorded process id, absent when the file is missing or does not hold a
/// process id. Zero is rejected because `kill -9 0` signals the whole group.
pub fn read_pid(path: &Path) -> Option<NonZeroU32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn empty_and_path_shaped_ids_are_rejected() {
        for id in ["", "..", ".", "a/b", "a\\b", "a.b", "a b", "a\nb", "a\0b"] {
            assert!(!valid_job_id(id), "{id:?}");
        }
    }

    #[test]
    fn generated_ids_are_accepted() {
        let generated = uuid::Uuid::new_v4().simple().to_string();
        assert!(valid_job_id(&generated));
        assert!(valid_job_id("job-1_a"));
    }

    #[test]
    fn pid_files_hold_process_ids_only() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("job.pid");
        fs::write(&path, "4242").expect("write");
        assert_eq!(read_pid(&path).map(NonZeroU32::get), Some(4242));
        for raw in ["0", "", "abc", "-1", "12 34"] {
            fs::write(&path, raw).expect("write");
            assert_eq!(read_pid(&path), None, "{raw:?}");
        }
        assert_eq!(read_pid(&directory.path().join("missing.pid")), None);
    }

    proptest! {
        #[test]
        fn valid_ids_name_direct_children_of_the_job_directory(candidate in "[A-Za-z0-9_-]{1,64}") {
            prop_assert!(valid_job_id(&candidate));
            let paths = JobPaths::new(Path::new("/jobs"), &candidate);
            prop_assert_eq!(paths.stdout(), Path::new("/jobs").join(format!("{candidate}.out")));
            prop_assert_eq!(paths.stderr(), Path::new("/jobs").join(format!("{candidate}.err")));
            prop_assert_eq!(paths.pid(), Path::new("/jobs").join(format!("{candidate}.pid")));
            prop_assert_eq!(paths.status(), Path::new("/jobs").join(format!("{candidate}.status")));
        }

        #[test]
        fn path_syntax_never_survives_validation(
            prefix in "[A-Za-z0-9_-]{0,8}",
            suffix in "[A-Za-z0-9_-]{0,8}",
            injected in prop_oneof![
                Just("/"), Just("\\"), Just(".."), Just("."), Just("\n"), Just(" "), Just("\0"),
            ],
        ) {
            let candidate = format!("{prefix}{injected}{suffix}");
            prop_assert!(!valid_job_id(&candidate));
        }

        #[test]
        fn arbitrary_strings_are_valid_only_when_they_name_a_token(candidate in ".{0,32}") {
            if valid_job_id(&candidate) {
                prop_assert!(!candidate.is_empty());
                prop_assert!(
                    candidate
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric()
                            || character == '-'
                            || character == '_')
                );
            } else {
                prop_assert!(
                    candidate.is_empty()
                        || candidate.chars().any(|character| !(character.is_ascii_alphanumeric()
                            || character == '-'
                            || character == '_'))
                );
            }
        }
    }
}

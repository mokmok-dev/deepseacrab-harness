//! The durable team state file.

use crate::error::TeamError;
use crate::model::TeamState;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const TEAM_FILE: &str = "team.json";

/// A JSON file holding the member and task lists.
pub struct TeamFile {
    path: PathBuf,
}

impl TeamFile {
    /// Wrap the team file under `state_dir`.
    pub(crate) fn in_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            path: state_dir.into().join(TEAM_FILE),
        }
    }

    /// The wrapped path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Read the state. A missing file is an empty team.
    pub(crate) fn load(&self) -> Result<TeamState, TeamError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                    TeamError::Encoding(format!("{}: {error}", self.path.display()))
                })?;
                if !value.is_object() {
                    return Err(TeamError::Encoding(format!(
                        "{}: expected a team object",
                        self.path.display()
                    )));
                }
                let state: TeamState = serde_json::from_value(value).map_err(|error| {
                    TeamError::Encoding(format!("{}: {error}", self.path.display()))
                })?;
                TeamState::validate(&state)?;
                Ok(state)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TeamState::default()),
            Err(source) => Err(TeamError::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Replace the file with `state`.
    ///
    /// The state is written to a sibling temporary file and renamed, so a
    /// reader never observes a half-written object.
    pub(crate) fn save(&self, state: &TeamState) -> Result<(), TeamError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| TeamError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let encoded = serde_json::to_vec_pretty(state)
            .map_err(|error| TeamError::Encoding(error.to_string()))?;
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, encoded).map_err(|source| TeamError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &self.path).map_err(|source| TeamError::Write {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MemberStatus, TaskStatus, TeamMember, TeamTask};
    use dsh_core::SessionId;
    use proptest::prelude::*;

    fn file(directory: &tempfile::TempDir) -> TeamFile {
        TeamFile::in_dir(directory.path())
    }

    fn member(id: &str, name: &str) -> TeamMember {
        TeamMember {
            id: id.to_string(),
            name: name.to_string(),
            session_id: SessionId::generate(),
            status: MemberStatus::Idle,
            last_text: "done".to_string(),
        }
    }

    fn task(id: &str, title: &str) -> TeamTask {
        TeamTask {
            id: id.to_string(),
            title: title.to_string(),
            description: "body".to_string(),
            status: TaskStatus::InProgress,
        }
    }

    fn state() -> TeamState {
        TeamState {
            members: vec![member("member-1", "alice")],
            tasks: vec![task("task-1", "ship")],
        }
    }

    #[test]
    fn an_absent_file_loads_as_an_empty_team() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        assert_eq!(file.load().expect("load"), TeamState::default());
        assert_eq!(file.path(), directory.path().join("team.json"));
    }

    #[test]
    fn saved_state_round_trips() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let state = state();
        file.save(&state).expect("save");
        assert_eq!(file.load().expect("load"), state);

        let mut replaced = state;
        replaced.tasks[0].status = TaskStatus::Completed;
        replaced.members[0].status = MemberStatus::Failed;
        replaced.members[0].last_text = "boom".to_string();
        file.save(&replaced).expect("save again");
        assert_eq!(file.load().expect("load"), replaced);
    }

    #[test]
    fn saving_is_atomic_and_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        file.save(&state()).expect("save");
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
        assert_eq!(names, vec!["team.json".to_string()]);
        assert!(!temporary_path(file.path()).exists());
        assert!(serde_json::from_slice::<TeamState>(&fs::read(file.path()).expect("read")).is_ok());
    }

    #[test]
    fn a_missing_state_directory_is_created() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = TeamFile::in_dir(directory.path().join("state"));
        file.save(&state()).expect("save");
        assert!(file.path().exists());
    }

    #[test]
    fn malformed_json_is_an_encoding_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        let broken = b"{not json";
        fs::write(file.path(), broken).expect("write");
        assert!(matches!(file.load(), Err(TeamError::Encoding(_))));
        assert_eq!(fs::read(file.path()).expect("read"), broken);
    }

    #[test]
    fn a_json_value_that_is_not_a_team_object_is_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        for body in [b"[]".as_slice(), b"null", b"\"team\"", b"42"] {
            fs::write(file.path(), body).expect("write");
            assert!(matches!(file.load(), Err(TeamError::Encoding(_))));
        }
        fs::write(
            file.path(),
            br#"{"members":[{"id":"bogus","name":"a","session_id":"s","status":"idle","last_text":""}],"tasks":[]}"#,
        )
        .expect("write");
        assert!(matches!(file.load(), Err(TeamError::Invalid(_))));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = file(&directory);
        fs::write(file.path(), br#"{"members":[],"tasks":[],"future":true}"#).expect("write");
        assert_eq!(file.load().expect("load"), TeamState::default());
    }

    #[test]
    fn unreadable_paths_are_read_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("team.json");
        fs::create_dir(&path).expect("mkdir");
        assert!(matches!(
            TeamFile::in_dir(directory.path()).load(),
            Err(TeamError::Read { .. })
        ));
    }

    #[test]
    fn unwritable_paths_are_write_errors() {
        let directory = tempfile::tempdir().expect("tempdir");
        let blocker = directory.path().join("blocker");
        fs::write(&blocker, b"").expect("write");
        let file = TeamFile::in_dir(blocker);
        assert!(matches!(file.save(&state()), Err(TeamError::Write { .. })));
    }

    fn arbitrary_member() -> impl Strategy<Value = TeamMember> {
        (
            "[A-Za-z0-9_-]{1,20}".prop_map(|suffix| format!("member-{suffix}")),
            "[A-Za-z][A-Za-z0-9 _-]{0,20}",
            "[A-Za-z0-9_-]{1,40}",
            prop_oneof![
                Just(MemberStatus::Running),
                Just(MemberStatus::Idle),
                Just(MemberStatus::Failed),
            ],
            "[ -~]{0,60}",
        )
            .prop_map(|(id, name, session, status, last_text)| TeamMember {
                id,
                name,
                session_id: SessionId::parse(session).expect("generated ids are valid"),
                status,
                last_text,
            })
    }

    fn arbitrary_task() -> impl Strategy<Value = TeamTask> {
        (
            "[A-Za-z0-9_-]{1,20}".prop_map(|suffix| format!("task-{suffix}")),
            "[A-Za-z][A-Za-z0-9 _-]{0,20}",
            "[ -~]{0,60}",
            prop_oneof![
                Just(TaskStatus::Pending),
                Just(TaskStatus::InProgress),
                Just(TaskStatus::Completed),
            ],
        )
            .prop_map(|(id, title, description, status)| TeamTask {
                id,
                title,
                description,
                status,
            })
    }

    proptest! {
        #[test]
        fn team_files_round_trip(
            members in prop::collection::vec(arbitrary_member(), 0..5),
            tasks in prop::collection::vec(arbitrary_task(), 0..5),
        ) {
            let directory = tempfile::tempdir().expect("tempdir");
            let file = file(&directory);
            let state = TeamState { members, tasks };
            file.save(&state).expect("save");
            prop_assert_eq!(file.load().expect("load"), state);
        }
    }
}

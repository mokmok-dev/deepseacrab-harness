//! The durable team vocabulary: members, tasks, and their validation.

use crate::error::TeamError;
use dsh_core::SessionId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MEMBER_PREFIX: &str = "member-";
const TASK_PREFIX: &str = "task-";

/// One teammate backed by its own child session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    /// Identifier minted when the teammate was spawned.
    pub id: String,
    /// Human-readable name chosen by the caller.
    pub name: String,
    /// Session that receives the teammate's turns.
    pub session_id: SessionId,
    /// Observable run state.
    pub status: MemberStatus,
    /// Last assistant text the settled turn produced.
    pub last_text: String,
}

/// Observable state of one teammate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberStatus {
    /// A turn is in flight.
    Running,
    /// The last turn completed normally.
    Idle,
    /// The last turn failed.
    Failed,
}

impl MemberStatus {
    /// The spelling used in listings and tool output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Failed => "failed",
        }
    }
}

impl TeamMember {
    /// Reject an id or name the tools cannot use.
    pub fn validate(&self) -> Result<(), TeamError> {
        validate_member_id(&self.id)?;
        if self.name.trim().is_empty() {
            return Err(TeamError::Invalid(
                "teammate name must not be empty".to_string(),
            ));
        }
        Ok(())
    }

    /// Mint a running member for `name`.
    pub(crate) fn mint(name: &str) -> Self {
        Self {
            id: format!("{MEMBER_PREFIX}{}", Uuid::new_v4()),
            name: name.to_string(),
            session_id: SessionId::generate(),
            status: MemberStatus::Running,
            last_text: String::new(),
        }
    }
}

/// One task shared by the team.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTask {
    /// Identifier minted when the task was created.
    pub id: String,
    /// Short summary shown in listings.
    pub title: String,
    /// Full task body.
    pub description: String,
    /// Lifecycle position.
    pub status: TaskStatus,
}

/// Lifecycle position of one team task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
}

impl TaskStatus {
    /// The spelling used in listings and tool output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

impl TeamTask {
    /// Reject an id or title the tools cannot use.
    pub fn validate(&self) -> Result<(), TeamError> {
        validate_task_id(&self.id)?;
        if self.title.trim().is_empty() {
            return Err(TeamError::Invalid(
                "team task title must not be empty".to_string(),
            ));
        }
        Ok(())
    }

    /// Mint a pending task.
    pub(crate) fn mint(title: &str, description: &str) -> Self {
        Self {
            id: format!("{TASK_PREFIX}{}", Uuid::new_v4()),
            title: title.to_string(),
            description: description.to_string(),
            status: TaskStatus::Pending,
        }
    }
}

/// Members and tasks as they are persisted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamState {
    #[serde(default)]
    pub members: Vec<TeamMember>,
    #[serde(default)]
    pub tasks: Vec<TeamTask>,
}

impl TeamState {
    pub(crate) fn validate(&self) -> Result<(), TeamError> {
        for member in &self.members {
            member.validate()?;
        }
        for task in &self.tasks {
            task.validate()?;
        }
        Ok(())
    }
}

fn validate_member_id(id: &str) -> Result<(), TeamError> {
    validate_id(id, MEMBER_PREFIX)
}

fn validate_task_id(id: &str) -> Result<(), TeamError> {
    validate_id(id, TASK_PREFIX)
}

fn validate_id(id: &str, prefix: &str) -> Result<(), TeamError> {
    let Some(suffix) = id.strip_prefix(prefix) else {
        return Err(TeamError::Invalid(format!(
            "id {id:?} must start with {prefix:?}"
        )));
    };
    if suffix.is_empty() {
        return Err(TeamError::Invalid(format!("id {id:?} has no suffix")));
    }
    if id.chars().any(char::is_control) {
        return Err(TeamError::Invalid(format!(
            "id {id:?} must not contain control characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn member() -> TeamMember {
        TeamMember {
            id: "member-1".to_string(),
            name: "alice".to_string(),
            session_id: SessionId::generate(),
            status: MemberStatus::Idle,
            last_text: "done".to_string(),
        }
    }

    fn task() -> TeamTask {
        TeamTask {
            id: "task-1".to_string(),
            title: "ship".to_string(),
            description: "cut the release".to_string(),
            status: TaskStatus::Pending,
        }
    }

    #[test]
    fn members_and_tasks_reject_blank_fields_and_bad_ids() {
        assert!(member().validate().is_ok());
        assert!(task().validate().is_ok());

        for name in ["", " ", "\t\n"] {
            let mut bad = member();
            bad.name = name.to_string();
            assert!(matches!(bad.validate(), Err(TeamError::Invalid(_))));
        }
        for id in ["", "alice", "member-", "member-1\u{7}"] {
            let mut bad = member();
            bad.id = id.to_string();
            assert!(matches!(bad.validate(), Err(TeamError::Invalid(_))));
        }
        for title in ["", " ", "\t\n"] {
            let mut bad = task();
            bad.title = title.to_string();
            assert!(matches!(bad.validate(), Err(TeamError::Invalid(_))));
        }
        for id in ["", "ship", "task-", "task-1\u{7}"] {
            let mut bad = task();
            bad.id = id.to_string();
            assert!(matches!(bad.validate(), Err(TeamError::Invalid(_))));
        }
        let mut bare = task();
        bare.description = String::new();
        assert!(bare.validate().is_ok());
    }

    #[test]
    fn minted_values_carry_their_prefix_and_a_fresh_session() {
        let first = TeamMember::mint("alice");
        let second = TeamMember::mint("alice");
        assert!(first.id.starts_with("member-"));
        assert_ne!(first.id, second.id);
        assert_ne!(first.session_id, second.session_id);
        assert_eq!(first.status, MemberStatus::Running);
        assert_eq!(first.last_text, "");
        first.validate().expect("valid member");

        let task = TeamTask::mint("ship", "cut the release");
        assert!(task.id.starts_with("task-"));
        assert_eq!(task.status, TaskStatus::Pending);
        task.validate().expect("valid task");
    }

    #[test]
    fn statuses_use_the_stable_snake_case_spelling() {
        assert_eq!(MemberStatus::Running.as_str(), "running");
        assert_eq!(MemberStatus::Idle.as_str(), "idle");
        assert_eq!(MemberStatus::Failed.as_str(), "failed");
        assert_eq!(
            serde_json::to_string(&MemberStatus::Running).expect("encode"),
            "\"running\""
        );
        assert_eq!(
            serde_json::from_str::<MemberStatus>("\"failed\"").expect("decode"),
            MemberStatus::Failed
        );

        assert_eq!(TaskStatus::Pending.as_str(), "pending");
        assert_eq!(TaskStatus::InProgress.as_str(), "in_progress");
        assert_eq!(TaskStatus::Completed.as_str(), "completed");
        assert_eq!(
            serde_json::to_string(&TaskStatus::InProgress).expect("encode"),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::from_str::<TaskStatus>("\"completed\"").expect("decode"),
            TaskStatus::Completed
        );
        assert!(serde_json::from_str::<TaskStatus>("\"done\"").is_err());
    }

    #[test]
    fn the_state_validator_walks_every_entry() {
        let mut state = TeamState {
            members: vec![member()],
            tasks: vec![task()],
        };
        state.validate().expect("valid state");

        let mut bad_member = member();
        bad_member.id = "bogus".to_string();
        let mut bad = state.clone();
        bad.members = vec![bad_member];
        assert!(matches!(bad.validate(), Err(TeamError::Invalid(_))));

        let mut bad_task = task();
        bad_task.title = String::new();
        let mut bad = state.clone();
        bad.tasks = vec![bad_task];
        assert!(matches!(bad.validate(), Err(TeamError::Invalid(_))));

        state.members.clear();
        state.tasks.clear();
        state.validate().expect("empty state");
    }

    proptest! {
        #[test]
        fn member_ids_with_a_suffix_are_accepted(suffix in "[A-Za-z0-9_-]{1,40}") {
            let id = format!("member-{suffix}");
            prop_assert!(validate_member_id(&id).is_ok());
        }

        #[test]
        fn ids_without_the_member_prefix_are_rejected(raw in "[A-Za-z0-9_-]{0,40}") {
            prop_assume!(!raw.starts_with(MEMBER_PREFIX));
            prop_assert!(matches!(
                validate_member_id(&raw),
                Err(TeamError::Invalid(_))
            ));
        }

        #[test]
        fn task_ids_with_a_suffix_are_accepted(suffix in "[A-Za-z0-9_-]{1,40}") {
            let id = format!("task-{suffix}");
            prop_assert!(validate_task_id(&id).is_ok());
        }

        #[test]
        fn ids_without_the_task_prefix_are_rejected(raw in "[A-Za-z0-9_-]{0,40}") {
            prop_assume!(!raw.starts_with(TASK_PREFIX));
            prop_assert!(matches!(
                validate_task_id(&raw),
                Err(TeamError::Invalid(_))
            ));
        }

        #[test]
        fn ids_containing_control_characters_are_rejected(
            head in "[A-Za-z0-9_-]{0,10}",
            bad in 0u8..=31,
            tail in "[A-Za-z0-9_-]{0,10}",
        ) {
            let raw = format!("{MEMBER_PREFIX}{head}{}{tail}", char::from(bad));
            prop_assert!(matches!(
                validate_member_id(&raw),
                Err(TeamError::Invalid(_))
            ));
            let raw = format!("{TASK_PREFIX}{head}{}{tail}", char::from(bad));
            prop_assert!(matches!(
                validate_task_id(&raw),
                Err(TeamError::Invalid(_))
            ));
        }
    }
}

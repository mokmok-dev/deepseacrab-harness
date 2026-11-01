//! Lexical path normalization and the workspace write policy.

use crate::tool::PermissionMode;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

/// A path that the session policy refuses.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathError {
    /// The caller supplied no path.
    #[error("path must not be empty")]
    Empty,
    /// The resolved path leaves the session working directory.
    #[error("path {path} is outside workspace {cwd}")]
    OutsideWorkspace {
        /// Resolved path.
        path: PathBuf,
        /// Session working directory.
        cwd: PathBuf,
    },
    /// The permission mode rejects every write.
    #[error("permission mode read-only rejects a write to {path}")]
    ReadOnly {
        /// Resolved path.
        path: PathBuf,
    },
}

/// Collapse `.` and `..` without touching the filesystem.
///
/// Symlinks are intentionally not resolved here. Callers that need physical
/// containment canonicalize the workspace root once and use this for the
/// syntactic part of the check.
#[must_use]
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let popped = normalized.pop();
                if !popped && !normalized.is_absolute() {
                    normalized.push(Component::ParentDir.as_os_str());
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

/// Resolve a caller-supplied path against the session directory.
pub fn resolve_path(cwd: &Path, raw: &str) -> Result<PathBuf, PathError> {
    if raw.is_empty() {
        return Err(PathError::Empty);
    }
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    Ok(normalize_lexical(&joined))
}

/// Enforce the write policy for a resolved path.
pub fn ensure_within(cwd: &Path, path: &Path, mode: PermissionMode) -> Result<(), PathError> {
    match mode {
        PermissionMode::DangerFullAccess => Ok(()),
        PermissionMode::ReadOnly => Err(PathError::ReadOnly {
            path: path.to_path_buf(),
        }),
        PermissionMode::WorkspaceWrite => {
            if path.starts_with(cwd) {
                Ok(())
            } else {
                Err(PathError::OutsideWorkspace {
                    path: path.to_path_buf(),
                    cwd: cwd.to_path_buf(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn segment() -> impl Strategy<Value = String> {
        "[A-Za-z0-9._-]{1,12}"
            .prop_filter("reject dot segments", |value| value != "." && value != "..")
    }

    fn relative_path() -> impl Strategy<Value = String> {
        prop::collection::vec(segment(), 1..6).prop_map(|segments| segments.join("/"))
    }

    proptest! {
        #[test]
        fn normalization_is_idempotent(raw in relative_path()) {
            let once = normalize_lexical(Path::new(&raw));
            let twice = normalize_lexical(&once);
            prop_assert_eq!(once, twice);
        }

        #[test]
        fn normalization_removes_parent_segments(cwd in relative_path(), extra in segment()) {
            let path = PathBuf::from(&cwd).join("..").join(&extra);
            let normalized = normalize_lexical(&path);
            prop_assert!(!normalized.components().any(|component| component == Component::ParentDir));
        }

        #[test]
        fn relative_paths_resolve_below_the_workspace(root in relative_path(), file in segment()) {
            let cwd = Path::new("/workspace").join(&root);
            let resolved = resolve_path(&cwd, &file).expect("resolvable");
            prop_assert_eq!(resolved, cwd.join(&file));
        }

        #[test]
        fn workspace_writes_accept_descendants(root in relative_path(), file in segment()) {
            let cwd = Path::new("/workspace").join(&root);
            let target = cwd.join(&file);
            prop_assert_eq!(ensure_within(&cwd, &target, PermissionMode::WorkspaceWrite), Ok(()));
        }
    }

    #[test]
    fn parent_traversal_is_rejected_by_the_workspace_policy() {
        let cwd = Path::new("/workspace/project");
        let target = resolve_path(cwd, "../other/file").expect("resolvable");
        assert!(matches!(
            ensure_within(cwd, &target, PermissionMode::WorkspaceWrite),
            Err(PathError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn absolute_paths_outside_the_workspace_are_rejected() {
        let cwd = Path::new("/workspace/project");
        let target = resolve_path(cwd, "/etc/passwd").expect("resolvable");
        assert!(matches!(
            ensure_within(cwd, &target, PermissionMode::WorkspaceWrite),
            Err(PathError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn read_only_rejects_every_write() {
        let cwd = Path::new("/workspace/project");
        let target = cwd.join("file.txt");
        assert_eq!(
            ensure_within(cwd, &target, PermissionMode::ReadOnly),
            Err(PathError::ReadOnly { path: target })
        );
    }

    #[test]
    fn full_access_accepts_outside_paths() {
        let target = Path::new("/etc/passwd");
        assert_eq!(
            ensure_within(
                Path::new("/workspace"),
                target,
                PermissionMode::DangerFullAccess
            ),
            Ok(())
        );
    }

    #[test]
    fn empty_paths_are_rejected() {
        assert_eq!(
            resolve_path(Path::new("/workspace"), ""),
            Err(PathError::Empty)
        );
    }

    #[test]
    fn dot_and_parent_segments_collapse() {
        assert_eq!(
            normalize_lexical(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(normalize_lexical(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(normalize_lexical(Path::new("../a")), PathBuf::from("../a"));
    }
}

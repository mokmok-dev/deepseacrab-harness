use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const WORKSPACE_NAME: &str = "Workspace";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Canonical absolute directory, resolved when it was added.
    pub path: String,
}

/// The HTTP view of a project, with its state read at response time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectView {
    pub name: String,
    /// Canonical absolute path.
    pub path: String,
    pub exists: bool,
    pub builtin: bool,
}

pub struct ProjectStore {
    path: PathBuf,
}

impl ProjectStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn at_home(state_dir: &Path) -> Self {
        Self::new(state_dir.join("projects.json"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Vec<Project>, ProjectError> {
        match fs::read_to_string(&self.path) {
            Ok(raw) if raw.trim().is_empty() => Ok(Vec::new()),
            Ok(raw) => serde_json::from_str(&raw).map_err(|error| {
                ProjectError::Encoding(format!("{}: {error}", self.path.display()))
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(ProjectError::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }

    pub fn save(&self, projects: &[Project]) -> Result<(), ProjectError> {
        let encoded = serde_json::to_string(projects)
            .map_err(|error| ProjectError::Encoding(format!("{}: {error}", self.path.display())))?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| ProjectError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let temporary = temporary_path(&self.path);
        fs::write(&temporary, encoded.as_bytes()).map_err(|source| ProjectError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &self.path).map_err(|source| ProjectError::Write {
            path: self.path.clone(),
            source,
        })
    }

    pub fn add(&self, raw_path: &str) -> Result<(Project, bool), ProjectError> {
        let canonical = canonical_directory(raw_path)?;
        let text = canonical.display().to_string();
        let mut projects = self.load()?;
        if let Some(existing) = projects.iter().find(|project| project.path == text) {
            return Ok((existing.clone(), false));
        }
        let project = Project { path: text };
        projects.push(project.clone());
        self.save(&projects)?;
        Ok((project, true))
    }

    /// The exact string is tried first because a registered directory may have
    /// been deleted, and canonicalizing such a path fails.
    pub fn remove(&self, raw_path: &str) -> Result<bool, ProjectError> {
        let canonical = fs::canonicalize(raw_path)
            .ok()
            .map(|path| path.display().to_string());
        let mut projects = self.load()?;
        let before = projects.len();
        projects.retain(|project| {
            project.path != raw_path && canonical.as_deref() != Some(project.path.as_str())
        });
        if projects.len() == before {
            return Ok(false);
        }
        self.save(&projects)?;
        Ok(true)
    }

    pub fn views(&self, workspace: &Path) -> Result<Vec<ProjectView>, ProjectError> {
        let workspace_path = workspace.display().to_string();
        let mut views = vec![workspace_view(workspace)];
        for project in self.load()? {
            if project.path != workspace_path {
                views.push(view(&project));
            }
        }
        Ok(views)
    }

    /// The request is never joined onto a path, so an arbitrary string can only
    /// select a directory the server already serves or holds.
    pub fn resolve(
        &self,
        workspace: &Path,
        requested: Option<&str>,
    ) -> Result<PathBuf, ProjectError> {
        let workspace_path = workspace.display().to_string();
        let Some(requested) = requested else {
            return available_workspace(workspace);
        };
        if requested == workspace_path {
            return available_workspace(workspace);
        }
        let projects = self.load()?;
        if projects.iter().any(|project| project.path == requested) {
            return available(Path::new(requested));
        }
        let Ok(canonical) = fs::canonicalize(requested) else {
            return Err(ProjectError::Unknown(requested.to_string()));
        };
        if canonical == workspace {
            return available_workspace(workspace);
        }
        let canonical_text = canonical.display().to_string();
        if projects
            .iter()
            .any(|project| project.path == canonical_text)
        {
            return available(&canonical);
        }
        Err(ProjectError::Unknown(requested.to_string()))
    }
}

#[must_use]
pub fn workspace_view(workspace: &Path) -> ProjectView {
    ProjectView {
        name: WORKSPACE_NAME.to_string(),
        path: workspace.display().to_string(),
        exists: workspace.is_dir(),
        builtin: true,
    }
}

#[must_use]
pub fn view(project: &Project) -> ProjectView {
    ProjectView {
        name: display_name(&project.path),
        path: project.path.clone(),
        exists: Path::new(&project.path).is_dir(),
        builtin: false,
    }
}

pub fn canonical_directory(raw_path: &str) -> Result<PathBuf, ProjectError> {
    let input = raw_path.trim();
    if input.is_empty() {
        return Err(ProjectError::InvalidPath {
            input: input.to_string(),
            reason: "must not be blank",
        });
    }
    if !Path::new(input).is_absolute() {
        return Err(ProjectError::InvalidPath {
            input: input.to_string(),
            reason: "must be absolute",
        });
    }
    let path = fs::canonicalize(input).map_err(|source| ProjectError::Unresolved {
        input: input.to_string(),
        source,
    })?;
    if !path.is_dir() {
        return Err(ProjectError::NotADirectory { path });
    }
    Ok(path)
}

fn available_workspace(workspace: &Path) -> Result<PathBuf, ProjectError> {
    if workspace.is_dir() {
        Ok(workspace.to_path_buf())
    } else {
        Err(ProjectError::NotADirectory {
            path: workspace.to_path_buf(),
        })
    }
}

fn available(path: &Path) -> Result<PathBuf, ProjectError> {
    let Ok(canonical) = fs::canonicalize(path) else {
        return Err(ProjectError::Unavailable {
            path: path.to_path_buf(),
            reason: "no longer exists",
        });
    };
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(ProjectError::Unavailable {
            path: path.to_path_buf(),
            reason: "is no longer a directory",
        })
    }
}

fn display_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.to_string(), str::to_string)
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("project path {input:?} {reason}")]
    InvalidPath { input: String, reason: &'static str },
    #[error("cannot resolve project path {input}: {source}")]
    Unresolved { input: String, source: io::Error },
    #[error("project path {path} is not a directory")]
    NotADirectory { path: PathBuf },
    #[error("project directory {path} {reason}")]
    Unavailable { path: PathBuf, reason: &'static str },
    #[error("unknown project {0}")]
    Unknown(String),
    #[error("cannot read project file {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("cannot write project file {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("project file encoding failure: {0}")]
    Encoding(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().expect("home")
    }

    fn store(home: &tempfile::TempDir) -> ProjectStore {
        ProjectStore::at_home(home.path())
    }

    fn directory(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        fs::create_dir_all(&path).expect("mkdir");
        path.canonicalize().expect("canonical")
    }

    #[test]
    fn a_missing_or_blank_project_file_is_empty() {
        let state = home();
        let store = store(&state);
        assert_eq!(store.load().expect("load"), Vec::new());
        fs::write(store.path(), " \n\t").expect("write");
        assert_eq!(store.load().expect("load"), Vec::new());
    }

    #[test]
    fn the_store_lives_under_the_state_directory() {
        let state = home();
        assert_eq!(
            ProjectStore::at_home(state.path()).path(),
            state.path().join("projects.json")
        );
    }

    #[test]
    fn adding_a_project_stores_its_canonical_path() {
        let state = home();
        let projects = home();
        let app = directory(projects.path(), "app");
        let nested = directory(&app, "nested");
        let store = store(&state);

        let (project, created) = store.add(&format!("{}/..", nested.display())).expect("add");

        assert!(created);
        assert_eq!(project.path, app.display().to_string());
        assert_eq!(store.load().expect("load"), vec![project.clone()]);
        assert_eq!(
            fs::read_to_string(store.path()).expect("file"),
            format!(r#"[{{"path":"{}"}}]"#, app.display())
        );
        assert_eq!(view(&project).name, "app");
    }

    #[cfg(unix)]
    #[test]
    fn adding_a_symlink_stores_its_target() {
        let state = home();
        let projects = home();
        let target = directory(projects.path(), "target");
        let link = projects.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let store = store(&state);

        let (project, created) = store.add(&link.display().to_string()).expect("add");

        assert!(created);
        assert_eq!(project.path, target.display().to_string());
    }

    #[test]
    fn invalid_paths_are_rejected() {
        let state = home();
        let projects = home();
        let store = store(&state);
        let file = projects.path().join("notes.txt");
        fs::write(&file, b"hi").expect("write");
        let missing = projects.path().join("missing");

        assert!(matches!(
            store.add("app"),
            Err(ProjectError::InvalidPath { .. })
        ));
        assert!(matches!(
            store.add("   "),
            Err(ProjectError::InvalidPath { .. })
        ));
        assert!(matches!(
            store.add(&missing.display().to_string()),
            Err(ProjectError::Unresolved { .. })
        ));
        assert!(matches!(
            store.add(&file.display().to_string()),
            Err(ProjectError::NotADirectory { .. })
        ));
        assert!(!store.path().exists());
    }

    #[test]
    fn a_rejected_add_leaves_the_file_unchanged() {
        let state = home();
        let projects = home();
        let kept = directory(projects.path(), "kept");
        let store = store(&state);
        store.add(&kept.display().to_string()).expect("add");
        let before = fs::read(store.path()).expect("read");

        let rejected = store.add(&store.path().display().to_string());

        assert!(matches!(rejected, Err(ProjectError::NotADirectory { .. })));
        assert_eq!(fs::read(store.path()).expect("read"), before);
    }

    #[test]
    fn adding_the_same_directory_twice_returns_the_existing_row() {
        let state = home();
        let projects = home();
        let app = directory(projects.path(), "app");
        let store = store(&state);

        let (first, created) = store.add(&app.display().to_string()).expect("first");
        let (second, created_again) = store.add(&app.display().to_string()).expect("second");

        assert!(created);
        assert!(!created_again);
        assert_eq!(second, first);
        assert_eq!(store.load().expect("load"), vec![first]);
    }

    #[test]
    fn removing_a_project_reports_whether_a_row_was_removed() {
        let state = home();
        let projects = home();
        let app = directory(projects.path(), "app");
        let store = store(&state);
        let (project, _) = store.add(&app.display().to_string()).expect("add");

        assert!(store.remove(&project.path).expect("remove"));
        assert!(!store.remove(&project.path).expect("remove again"));
        assert_eq!(store.load().expect("load"), Vec::new());
    }

    #[test]
    fn a_registered_directory_that_vanished_can_still_be_removed() {
        let state = home();
        let projects = home();
        let app = directory(projects.path(), "app");
        let store = store(&state);
        let (project, _) = store.add(&app.display().to_string()).expect("add");
        fs::remove_dir_all(&app).expect("remove directory");

        assert!(store.remove(&project.path).expect("remove"));
        assert_eq!(store.load().expect("load"), Vec::new());
    }

    #[test]
    fn a_corrupt_project_file_is_an_encoding_error() {
        let state = home();
        let store = store(&state);
        fs::write(store.path(), r#"[{"path":"#).expect("write");

        assert!(matches!(store.load(), Err(ProjectError::Encoding(_))));
    }

    #[test]
    fn views_list_the_workspace_first_and_skip_it_again() {
        let state = home();
        let projects = home();
        let workspace = directory(projects.path(), "workspace");
        let app = directory(projects.path(), "app");
        let store = store(&state);
        store.add(&app.display().to_string()).expect("add");
        store
            .add(&workspace.display().to_string())
            .expect("add workspace");

        let views = store.views(&workspace).expect("views");

        assert_eq!(views.len(), 2);
        assert_eq!(views[0].name, WORKSPACE_NAME);
        assert_eq!(views[0].path, workspace.display().to_string());
        assert!(views[0].builtin);
        assert!(views[0].exists);
        assert_eq!(views[1].name, "app");
        assert_eq!(views[1].path, app.display().to_string());
        assert!(!views[1].builtin);
        assert!(views[1].exists);
    }

    #[test]
    fn a_deleted_project_is_listed_as_missing() {
        let state = home();
        let projects = home();
        let workspace = directory(projects.path(), "workspace");
        let app = directory(projects.path(), "app");
        let store = store(&state);
        store.add(&app.display().to_string()).expect("add");
        fs::remove_dir_all(&app).expect("remove directory");

        let views = store.views(&workspace).expect("views");

        assert_eq!(views[1].name, "app");
        assert!(!views[1].exists);
    }

    #[test]
    fn resolving_selects_the_workspace_or_a_stored_project() {
        let state = home();
        let projects = home();
        let workspace = directory(projects.path(), "workspace");
        let app = directory(projects.path(), "app");
        let store = store(&state);
        store.add(&app.display().to_string()).expect("add");
        let workspace_text = workspace.display().to_string();
        let app_text = app.display().to_string();

        assert_eq!(store.resolve(&workspace, None).expect("default"), workspace);
        assert_eq!(
            store
                .resolve(&workspace, Some(&workspace_text))
                .expect("workspace"),
            workspace
        );
        assert_eq!(
            store.resolve(&workspace, Some(&app_text)).expect("project"),
            app
        );
    }

    #[test]
    fn resolving_rejects_anything_else_without_touching_the_filesystem() {
        let state = home();
        let projects = home();
        let workspace = directory(projects.path(), "workspace");
        let other = directory(projects.path(), "other");
        let store = store(&state);
        let requests = [
            "../../etc".to_string(),
            "/etc".to_string(),
            other.display().to_string(),
        ];

        for request in &requests {
            assert!(
                matches!(
                    store.resolve(&workspace, Some(request.as_str())),
                    Err(ProjectError::Unknown(_))
                ),
                "{request}"
            );
        }
    }

    #[test]
    fn resolving_a_vanished_project_is_unavailable() {
        let state = home();
        let projects = home();
        let workspace = directory(projects.path(), "workspace");
        let app = directory(projects.path(), "app");
        let store = store(&state);
        let (project, _) = store.add(&app.display().to_string()).expect("add");
        fs::remove_dir_all(&app).expect("remove directory");

        let error = store
            .resolve(&workspace, Some(&project.path))
            .expect_err("vanished");

        assert!(matches!(error, ProjectError::Unavailable { .. }));
        assert!(error.to_string().contains(&project.path), "{error}");
    }

    #[test]
    fn a_vanished_workspace_is_not_a_directory() {
        let state = home();
        let projects = home();
        let workspace = directory(projects.path(), "workspace");
        let store = store(&state);
        fs::remove_dir_all(&workspace).expect("remove directory");

        assert!(matches!(
            store.resolve(&workspace, None),
            Err(ProjectError::NotADirectory { .. })
        ));
    }
}

//! Finding the repositories to search. Frozen: only the coordinator changes this file.
//!
//! `discover` turns a start folder into a `Workspace`, first rule that applies wins:
//! 1. A configured workspace whose root is the start folder or one of its parents (the deepest
//!    one): its explicit `repos`, or the repositories discovered under its root.
//! 2. The nearest folder at or above the start that holds `.git` (a folder or a file, as in
//!    worktrees), never the home folder or the file system root: that one repository.
//! 3. Repositories under the start folder, down to `MAX_DEPTH` levels: folders with any of
//!    `REPO_MARKERS`. The search does not go into a found repository, hidden folders or
//!    `SKIP_DIRS`. If the start folder has a marker itself and none of the found folders has
//!    `.git`, they are modules of one project, so the start folder is that one repository.
//! 4. The start folder itself, if it has a marker.
//! 5. Otherwise `WorkspaceError::NoRepos`.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{Config, RepoConfig};
use crate::types::{Repo, Workspace};

/// Files and folders that mark the root of a repository or project.
pub const REPO_MARKERS: &[&str] = &[
    ".git",
    "settings.gradle.kts",
    "settings.gradle",
    "build.gradle.kts",
    "build.gradle",
    "pom.xml",
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "Package.swift",
    "pubspec.yaml",
];

/// Folders never searched for repositories, besides hidden ones.
pub const SKIP_DIRS: &[&str] = &["build", "node_modules", "target", "dist", "out", "vendor", "Pods", "DerivedData"];

/// How deep below the start folder discovery looks for repositories.
pub const MAX_DEPTH: usize = 2;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("{0} is not a folder")]
    NotADirectory(PathBuf),
    #[error("cannot read {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("no repositories in {0}: no .git or build files found up to {MAX_DEPTH} levels down")]
    NoRepos(PathBuf),
}

/// Finds the workspace for `start` with the rules in the module docs.
pub fn discover(start: &Path, config: &Config) -> Result<Workspace, WorkspaceError> {
    discover_with_home(start, config, dirs::home_dir().as_deref())
}

/// `discover` with an explicit home folder, for tests.
pub fn discover_with_home(start: &Path, config: &Config, home: Option<&Path>) -> Result<Workspace, WorkspaceError> {
    let start = canonical_dir(start)?;
    let home = home.and_then(|path| path.canonicalize().ok());

    if let Some(configured) = configured_workspace(&start, config) {
        return configured;
    }
    if let Some(git_root) = git_root(&start, home.as_deref()) {
        return Ok(single(git_root));
    }
    scan(&start, &[])
}

fn canonical_dir(path: &Path) -> Result<PathBuf, WorkspaceError> {
    let canonical = path.canonicalize().map_err(|source| WorkspaceError::Io { path: path.to_path_buf(), source })?;
    if canonical.is_dir() { Ok(canonical) } else { Err(WorkspaceError::NotADirectory(path.to_path_buf())) }
}

/// Rule 1. Returns `None` when no configured workspace contains `start`.
fn configured_workspace(start: &Path, config: &Config) -> Option<Result<Workspace, WorkspaceError>> {
    let (root, ws) = config
        .workspaces
        .values()
        .filter_map(|ws| ws.root.canonicalize().ok().map(|root| (root, ws)))
        .filter(|(root, _)| start.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())?;
    if ws.repos.is_empty() {
        return Some(scan(&root, &ws.exclude));
    }
    Some(explicit_repos(root, &ws.repos))
}

fn explicit_repos(root: PathBuf, configured: &[RepoConfig]) -> Result<Workspace, WorkspaceError> {
    let mut repos = configured
        .iter()
        .map(|repo| {
            let path = if repo.path.is_absolute() { repo.path.clone() } else { root.join(&repo.path) };
            Ok(Repo { name: repo.name.clone(), root: canonical_dir(&path)? })
        })
        .collect::<Result<Vec<_>, WorkspaceError>>()?;
    repos.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Workspace { root, repos })
}

/// Rule 2: the nearest folder at or above `start` with `.git`, below `home` and the file system root.
fn git_root(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    start
        .ancestors()
        .take_while(|dir| Some(*dir) != home && dir.parent().is_some())
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Rules 3 to 5 for `root`.
fn scan(root: &Path, exclude: &[String]) -> Result<Workspace, WorkspaceError> {
    let mut found = Vec::new();
    collect(root, root, 1, exclude, &mut found)?;
    let has_marker = REPO_MARKERS.iter().any(|marker| root.join(marker).exists());
    let any_git = found.iter().any(|dir: &PathBuf| dir.join(".git").exists());
    if has_marker && !any_git {
        return Ok(single(root.to_path_buf()));
    }
    if found.is_empty() {
        return Err(WorkspaceError::NoRepos(root.to_path_buf()));
    }
    let mut repos: Vec<Repo> = found
        .into_iter()
        .map(|dir| {
            let name = dir
                .strip_prefix(root)
                .unwrap_or(&dir)
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            Repo { name, root: dir }
        })
        .collect();
    repos.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Workspace { root: root.to_path_buf(), repos })
}

fn collect(
    root: &Path,
    dir: &Path,
    depth: usize,
    exclude: &[String],
    found: &mut Vec<PathBuf>,
) -> Result<(), WorkspaceError> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|source| WorkspaceError::Io { path: dir.to_path_buf(), source })?;
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| !skipped(root, path, exclude))
        .collect();
    children.sort();
    for child in children {
        if REPO_MARKERS.iter().any(|marker| child.join(marker).exists()) {
            found.push(child);
        } else {
            collect(root, &child, depth + 1, exclude, found)?;
        }
    }
    Ok(())
}

/// Hidden folders, `SKIP_DIRS`, and config excludes (a folder name, or a path relative to the root).
fn skipped(root: &Path, path: &Path, exclude: &[String]) -> bool {
    let name = path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
        return true;
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    exclude.iter().any(|entry| {
        let entry = entry.trim_end_matches('/');
        if entry.contains('/') { relative == Path::new(entry) } else { name == entry }
    })
}

fn single(root: PathBuf) -> Workspace {
    let name = root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    Workspace { root: root.clone(), repos: vec![Repo { name, root }] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceConfig;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;

    fn touch(root: &Path, relative: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    fn mkdir(root: &Path, relative: &str) {
        fs::create_dir_all(root.join(relative)).unwrap();
    }

    fn names(ws: &Workspace) -> Vec<&str> {
        ws.repos.iter().map(|repo| repo.name.as_str()).collect()
    }

    fn tmp() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    #[test]
    fn parent_without_git_lists_child_repos() {
        let (_dir, root) = tmp();
        mkdir(&root, "android/.git");
        touch(&root, "android/core/build.gradle.kts"); // a module inside a repo, not a repo
        mkdir(&root, "sdk/.git");
        touch(&root, "group/tools/pyproject.toml"); // depth 2
        touch(&root, "group/deep/er/package.json"); // depth 3: too deep
        touch(&root, ".idea/package.json"); // hidden
        touch(&root, "build/package.json"); // skipped folder
        touch(&root, "notes/readme.md"); // no marker
        let ws = discover_with_home(&root, &Config::default(), None).unwrap();
        assert_eq!(ws.root, root);
        assert_eq!(names(&ws), vec!["android", "group/tools", "sdk"]);
        assert_eq!(ws.repos[0].root, root.join("android"));
    }

    #[test]
    fn start_inside_a_repo_gives_that_repo() {
        let (_dir, root) = tmp();
        mkdir(&root, "android/.git");
        mkdir(&root, "android/core/src");
        let ws = discover_with_home(&root.join("android/core/src"), &Config::default(), None).unwrap();
        assert_eq!(names(&ws), vec!["android"]);
        assert_eq!(ws.root, root.join("android"));
    }

    #[test]
    fn git_file_marks_a_worktree() {
        let (_dir, root) = tmp();
        touch(&root, "wt/.git");
        let ws = discover_with_home(&root.join("wt"), &Config::default(), None).unwrap();
        assert_eq!(names(&ws), vec!["wt"]);
    }

    #[test]
    fn home_is_never_a_repo() {
        let (_dir, root) = tmp();
        mkdir(&root, ".git"); // a dotfiles repository in the home folder
        mkdir(&root, "projects/a/.git");
        mkdir(&root, "projects/b/.git");
        let ws = discover_with_home(&root.join("projects"), &Config::default(), Some(&root)).unwrap();
        assert_eq!(names(&ws), vec!["a", "b"]);
    }

    #[test]
    fn project_with_modules_and_no_git_is_one_repo() {
        let (_dir, root) = tmp();
        touch(&root, "app/settings.gradle.kts");
        touch(&root, "app/core/build.gradle.kts");
        touch(&root, "app/network/build.gradle.kts");
        let ws = discover_with_home(&root.join("app"), &Config::default(), None).unwrap();
        assert_eq!(names(&ws), vec!["app"]);
    }

    #[test]
    fn marker_in_parent_does_not_hide_git_children() {
        let (_dir, root) = tmp();
        touch(&root, "package.json");
        mkdir(&root, "one/.git");
        mkdir(&root, "two/.git");
        let ws = discover_with_home(&root, &Config::default(), None).unwrap();
        assert_eq!(names(&ws), vec!["one", "two"]);
    }

    #[test]
    fn configured_workspace_containing_start_wins() {
        let (_dir, root) = tmp();
        mkdir(&root, "work/android/.git");
        mkdir(&root, "work/sdk/.git");
        mkdir(&root, "work/docs/.git");
        mkdir(&root, "work/android/core");
        let mut config = Config::default();
        config.workspaces.insert(
            "work".into(),
            WorkspaceConfig { root: root.join("work"), exclude: vec!["docs".into()], repos: vec![] },
        );
        let ws = discover_with_home(&root.join("work/android/core"), &config, None).unwrap();
        assert_eq!(ws.root, root.join("work"));
        assert_eq!(names(&ws), vec!["android", "sdk"]);
    }

    #[test]
    fn configured_repos_are_used_as_given() {
        let (_dir, root) = tmp();
        mkdir(&root, "ws/a/.git");
        mkdir(&root, "elsewhere/b");
        let mut config = Config::default();
        config.workspaces.insert(
            "ws".into(),
            WorkspaceConfig {
                root: root.join("ws"),
                exclude: vec![],
                repos: vec![
                    RepoConfig { name: "zeta".into(), path: "a".into() },
                    RepoConfig { name: "alpha".into(), path: root.join("elsewhere/b") },
                ],
            },
        );
        let ws = discover_with_home(&root.join("ws"), &config, None).unwrap();
        assert_eq!(names(&ws), vec!["alpha", "zeta"]);
        assert_eq!(ws.repos[1].root, root.join("ws/a"));
    }

    #[test]
    fn empty_folder_is_an_error() {
        let (_dir, root) = tmp();
        mkdir(&root, "empty/sub");
        let err = discover_with_home(&root.join("empty"), &Config::default(), None).unwrap_err();
        assert!(matches!(err, WorkspaceError::NoRepos(_)));
    }

    #[test]
    fn missing_folder_is_an_error() {
        let (_dir, root) = tmp();
        let err = discover_with_home(&root.join("nope"), &Config::default(), None).unwrap_err();
        assert!(matches!(err, WorkspaceError::Io { .. }));
    }
}

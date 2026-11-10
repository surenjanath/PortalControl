//! Native Git delta tracking (PRD module 2). Everything goes through libgit2
//! (git2-rs); no `git` executable is ever spawned.

use crate::state::AppState;
use crate::util::{log, now_secs};
use git2::{Cred, CredentialType, FetchOptions, RemoteCallbacks, Repository, StatusOptions};
use serde::Serialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const UP_TO_DATE: &str = "UP_TO_DATE";
pub const UPDATES_AVAILABLE: &str = "UPDATES_AVAILABLE";
pub const DIRTY: &str = "DIRTY";

/// How often remotes are fetched (PRD: every 5 minutes).
const FETCH_INTERVAL: Duration = Duration::from_secs(300);
/// How often the local working tree is re-inspected (no network).
const INSPECT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub struct GitInfo {
    pub is_repo: bool,
    pub status: &'static str,
    pub dirty_files: usize,
    pub ahead: usize,
    pub behind: usize,
    pub head: Option<String>,
    pub remote_head: Option<String>,
    pub checked_out_branch: Option<String>,
    pub tracked_branch: String,
    pub last_fetch: Option<String>,
    pub fetch_error: Option<String>,
    pub error: Option<String>,
}

impl GitInfo {
    fn not_repo(branch: &str, error: Option<String>) -> Self {
        Self {
            is_repo: false,
            status: UP_TO_DATE,
            dirty_files: 0,
            ahead: 0,
            behind: 0,
            head: None,
            remote_head: None,
            checked_out_branch: None,
            tracked_branch: branch.to_string(),
            last_fetch: None,
            fetch_error: None,
            error,
        }
    }
}

/// One-time libgit2 configuration.
pub fn init() {
    // SAFETY: called once at startup before any repository is opened.
    unsafe {
        // Targets are registered explicitly by the administrator, so the
        // "dubious ownership" check only gets in the way (e.g. synced folders).
        let _ = git2::opts::set_verify_owner_validation(false);
        // Never let a stalled remote hang a fetch worker forever.
        let _ = git2::opts::set_server_connect_timeout_in_milliseconds(15_000);
        let _ = git2::opts::set_server_timeout_in_milliseconds(60_000);
    }
}

/// Derive the PRD state matrix from the on-disk repository. No network.
///
/// * `DIRTY` – modified or untracked files in the workspace.
/// * `UPDATES_AVAILABLE` – `origin/<branch>` has commits HEAD does not.
/// * `UP_TO_DATE` – otherwise.
pub fn inspect(dir: &Path, branch: &str) -> GitInfo {
    let repo = match Repository::open(dir) {
        Ok(r) => r,
        Err(e) => {
            let missing = e.code() == git2::ErrorCode::NotFound;
            return GitInfo::not_repo(
                branch,
                if missing {
                    None
                } else {
                    Some(e.message().to_string())
                },
            );
        }
    };

    let mut info = GitInfo::not_repo(branch, None);
    info.is_repo = true;

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false)
        .exclude_submodules(true);
    match repo.statuses(Some(&mut opts)) {
        Ok(st) => info.dirty_files = st.len(),
        Err(e) => info.error = Some(format!("status failed: {}", e.message())),
    }

    let head_ref = repo.head().ok();
    info.checked_out_branch = head_ref
        .as_ref()
        .filter(|h| h.is_branch())
        .and_then(|h| h.shorthand().map(str::to_string));
    let head = head_ref.and_then(|h| h.target());
    let remote = repo
        .refname_to_id(&format!("refs/remotes/origin/{branch}"))
        .ok();
    info.head = head.map(|o| o.to_string());
    info.remote_head = remote.map(|o| o.to_string());

    if let (Some(h), Some(r)) = (head, remote) {
        if h != r {
            match repo.graph_ahead_behind(h, r) {
                Ok((ahead, behind)) => {
                    info.ahead = ahead;
                    info.behind = behind;
                }
                Err(e) => info.error = Some(format!("ahead/behind failed: {}", e.message())),
            }
        }
    }

    info.status = if info.dirty_files > 0 {
        DIRTY
    } else if info.behind > 0 {
        UPDATES_AVAILABLE
    } else {
        UP_TO_DATE
    };
    info
}

/// Fetch `origin` so remote-tracking refs reflect the server. Only
/// `refs/remotes/*` and FETCH_HEAD change; the working tree is untouched.
pub fn fetch(dir: &Path) -> Result<(), String> {
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    let mut remote = repo
        .find_remote("origin")
        .map_err(|_| "no 'origin' remote configured".to_string())?;
    let config = repo.config().map_err(|e| e.message().to_string())?;

    let mut attempts = 0;
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |url, username, allowed| {
        attempts += 1;
        if attempts > 3 {
            return Err(git2::Error::from_str("authentication failed"));
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            if let Ok(cred) = Cred::credential_helper(&config, url, username) {
                return Ok(cred);
            }
        }
        if allowed.contains(CredentialType::DEFAULT) {
            return Cred::default();
        }
        Err(git2::Error::from_str("no usable credentials for this remote"))
    });
    let mut options = FetchOptions::new();
    options.remote_callbacks(callbacks);
    remote
        .fetch(&[] as &[&str], Some(&mut options), None)
        .map_err(|e| e.message().to_string())
}

/// Inspect (optionally fetching first) one target and publish the result.
pub async fn refresh_target(
    state: &Arc<AppState>,
    target_id: i64,
    dir: String,
    branch: String,
    do_fetch: bool,
) -> GitInfo {
    let previous = state
        .git
        .lock()
        .ok()
        .and_then(|g| g.get(&target_id).cloned());

    let joined = tokio::task::spawn_blocking(move || {
        let path = Path::new(&dir);
        let mut fetch_result = None;
        if do_fetch && path.join(".git").exists() {
            fetch_result = Some(fetch(path));
        }
        (inspect(path, &branch), fetch_result)
    })
    .await;

    let (mut info, fetch_result) = match joined {
        Ok(v) => v,
        Err(e) => (
            GitInfo::not_repo("", Some(format!("git worker failed: {e}"))),
            None,
        ),
    };

    // Carry fetch bookkeeping across local-only inspections.
    if let Some(prev) = &previous {
        info.last_fetch = prev.last_fetch.clone();
        info.fetch_error = prev.fetch_error.clone();
    }
    match fetch_result {
        Some(Ok(())) => {
            info.last_fetch = Some(crate::util::fmt_utc(now_secs()));
            info.fetch_error = None;
        }
        Some(Err(e)) => {
            log(&format!("git fetch failed for target {target_id}: {e}"));
            info.fetch_error = Some(e);
        }
        None => {}
    }

    let changed = previous
        .as_ref()
        .map(|p| {
            p.status != info.status
                || p.dirty_files != info.dirty_files
                || p.ahead != info.ahead
                || p.behind != info.behind
                || p.head != info.head
                || p.remote_head != info.remote_head
                || p.last_fetch != info.last_fetch
                || p.fetch_error != info.fetch_error
        })
        .unwrap_or(true);

    if info.is_repo {
        let _ = state.db.set_git_status(target_id, info.status).await;
    }
    if let Ok(mut g) = state.git.lock() {
        g.insert(target_id, info.clone());
    }
    if changed {
        state.emit(json!({"type": "git", "target_id": target_id, "git": info}));
    }
    info
}

/// Background worker: local inspection every 30s, remote fetch every 5 minutes.
/// Each target is handled by its own decoupled task so a slow remote never
/// blocks the others or the request handlers.
pub fn spawn_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        let ticks_per_fetch = (FETCH_INTERVAL.as_secs() / INSPECT_INTERVAL.as_secs()).max(1);
        let mut tick: u64 = 0;
        loop {
            let do_fetch = tick % ticks_per_fetch == 0;
            if let Ok(targets) = state.db.list_targets().await {
                let ids: Vec<i64> = targets.iter().map(|t| t.id).collect();
                for t in targets {
                    let st = state.clone();
                    tokio::spawn(async move {
                        refresh_target(&st, t.id, t.working_directory, t.git_branch, do_fetch)
                            .await;
                    });
                }
                if let Ok(mut g) = state.git.lock() {
                    g.retain(|id, _| ids.contains(id));
                }
            }
            tick += 1;
            tokio::time::sleep(INSPECT_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, Signature};

    fn commit_all(repo: &Repository, msg: &str) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = Signature::now("Tester", "tester@example.com").unwrap();
        let parents: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &refs).unwrap()
    }

    fn init_repo(dir: &Path) -> Repository {
        let mut opts = git2::RepositoryInitOptions::new();
        opts.initial_head("main");
        let repo = Repository::init_opts(dir, &opts).unwrap();
        std::fs::write(dir.join("app.py"), "print('v1')\n").unwrap();
        commit_all(&repo, "initial");
        repo
    }

    #[test]
    fn non_repo_is_reported_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let info = inspect(dir.path(), "main");
        assert!(!info.is_repo);
        assert!(info.error.is_none());
        assert_eq!(info.status, UP_TO_DATE);
    }

    #[test]
    fn full_state_matrix_against_a_real_remote() {
        // "origin" is a second repository on disk; fetch goes through libgit2's
        // transport layer exactly as it would for a network remote.
        let origin_dir = tempfile::tempdir().unwrap();
        let origin = init_repo(origin_dir.path());

        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::clone(origin_dir.path().to_str().unwrap(), clone_dir.path()).unwrap();

        // 1. Fresh clone: identical SHA-1s.
        let info = inspect(clone_dir.path(), "main");
        assert!(info.is_repo);
        assert_eq!(info.status, UP_TO_DATE);
        assert_eq!(info.head, info.remote_head);
        assert_eq!(info.checked_out_branch.as_deref(), Some("main"));

        // 2. Remote gains a commit; before fetching we cannot know.
        std::fs::write(origin_dir.path().join("app.py"), "print('v2')\n").unwrap();
        let new_remote = commit_all(&origin, "second");
        assert_eq!(inspect(clone_dir.path(), "main").status, UP_TO_DATE);

        // 3. After fetch: remote tracking branch is ahead of HEAD.
        fetch(clone_dir.path()).unwrap();
        let info = inspect(clone_dir.path(), "main");
        assert_eq!(info.status, UPDATES_AVAILABLE);
        assert_eq!((info.ahead, info.behind), (0, 1));
        assert_eq!(info.remote_head, Some(new_remote.to_string()));

        // 4. Untracked file => DIRTY (takes priority).
        std::fs::write(clone_dir.path().join("notes.txt"), "wip").unwrap();
        let info = inspect(clone_dir.path(), "main");
        assert_eq!(info.status, DIRTY);
        assert_eq!(info.dirty_files, 1);
        assert_eq!(info.behind, 1);

        // 5. Modified tracked file is also DIRTY.
        std::fs::remove_file(clone_dir.path().join("notes.txt")).unwrap();
        std::fs::write(clone_dir.path().join("app.py"), "print('local edit')\n").unwrap();
        assert_eq!(inspect(clone_dir.path(), "main").status, DIRTY);

        // 6. Local commit: clean again, now diverged (1 ahead, 1 behind).
        commit_all(&clone, "local");
        let info = inspect(clone_dir.path(), "main");
        assert_eq!(info.status, UPDATES_AVAILABLE);
        assert_eq!((info.ahead, info.behind), (1, 1));
    }

    #[test]
    fn ignored_files_do_not_make_the_tree_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        std::fs::write(dir.path().join(".gitignore"), "db.sqlite3\n").unwrap();
        commit_all(&repo, "ignore db");
        std::fs::write(dir.path().join("db.sqlite3"), "binary").unwrap();
        let info = inspect(dir.path(), "main");
        assert_eq!(info.dirty_files, 0);
        assert_eq!(info.status, UP_TO_DATE);
    }

    #[test]
    fn fetch_without_origin_fails_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        assert!(fetch(dir.path()).unwrap_err().contains("origin"));
    }
}

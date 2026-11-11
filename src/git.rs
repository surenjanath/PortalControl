//! Native Git delta tracking (PRD module 2). Everything goes through libgit2
//! (git2-rs); no `git` executable is ever spawned.

use crate::state::AppState;
use crate::util::{log, now_secs};
use git2::{
    build::CheckoutBuilder, Cred, CredentialType, FetchOptions, Oid, RemoteCallbacks, Repository,
    Status, StatusOptions,
};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
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
    /// First line of the checked-out commit's message.
    pub head_summary: Option<String>,
    pub head_author: Option<String>,
    /// Commit time of HEAD, unix seconds.
    pub head_time: Option<i64>,
    pub remote_summary: Option<String>,
    /// URL of `origin`, with any embedded credentials removed.
    pub remote_url: Option<String>,
}

/// One commit in the history view.
#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub summary: String,
    pub author: String,
    pub time: i64,
    /// `incoming` (on origin, not pulled yet), `local` (not on origin) or `synced`.
    pub state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PullOutcome {
    pub updated: bool,
    pub from: Option<String>,
    pub to: Option<String>,
    pub commits: usize,
}

/// Remove `user:password@` from an http(s) remote URL before showing it.
pub fn strip_credentials(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.starts_with("http") {
            let (authority, tail) = match rest.find('/') {
                Some(i) => rest.split_at(i),
                None => (rest, ""),
            };
            if let Some((_, host)) = authority.rsplit_once('@') {
                return format!("{scheme}://{host}{tail}");
            }
        }
    }
    url.to_string()
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
            head_summary: None,
            head_author: None,
            head_time: None,
            remote_summary: None,
            remote_url: None,
        }
    }
}

fn commit_info(repo: &Repository, oid: Oid, state: &'static str) -> Option<CommitInfo> {
    let c = repo.find_commit(oid).ok()?;
    let author = c.author().name().unwrap_or("").to_string();
    let info = CommitInfo {
        sha: oid.to_string(),
        summary: c.summary().unwrap_or("").to_string(),
        author,
        time: c.time().seconds(),
        state,
    };
    Some(info)
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
    if let Some(c) = head.and_then(|h| commit_info(&repo, h, "synced")) {
        info.head_summary = Some(c.summary);
        info.head_author = Some(c.author);
        info.head_time = Some(c.time);
    }
    info.remote_summary = remote
        .and_then(|r| commit_info(&repo, r, "synced"))
        .map(|c| c.summary);
    info.remote_url = repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().map(strip_credentials));

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

/// Recent history: commits waiting on `origin/<branch>` first, then the
/// commits of HEAD, each marked incoming / local / synced.
pub fn recent_commits(dir: &Path, branch: &str, limit: usize) -> Result<Vec<CommitInfo>, String> {
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    let err = |e: git2::Error| e.message().to_string();
    let head = repo.head().ok().and_then(|h| h.target());
    let remote = repo
        .refname_to_id(&format!("refs/remotes/origin/{branch}"))
        .ok();
    let mut out = Vec::new();

    if let (Some(h), Some(r)) = (head, remote) {
        if h != r {
            let mut walk = repo.revwalk().map_err(err)?;
            walk.push(r).map_err(err)?;
            walk.hide(h).map_err(err)?;
            for oid in walk.flatten().take(limit) {
                out.extend(commit_info(&repo, oid, "incoming"));
            }
        }
    }

    let Some(h) = head else { return Ok(out) };
    let mut unpushed: HashSet<Oid> = HashSet::new();
    if let Some(r) = remote {
        if h != r {
            let mut walk = repo.revwalk().map_err(err)?;
            walk.push(h).map_err(err)?;
            walk.hide(r).map_err(err)?;
            unpushed.extend(walk.flatten().take(500));
        }
    }
    let mut walk = repo.revwalk().map_err(err)?;
    walk.push(h).map_err(err)?;
    for oid in walk.flatten().take(limit) {
        let state = if unpushed.contains(&oid) {
            "local"
        } else if remote.is_some() {
            "synced"
        } else {
            "local"
        };
        out.extend(commit_info(&repo, oid, state));
    }
    Ok(out)
}

fn status_label(s: Status) -> &'static str {
    if s.contains(Status::CONFLICTED) {
        "conflicted"
    } else if s.contains(Status::WT_NEW) {
        "untracked"
    } else if s.contains(Status::INDEX_NEW) {
        "added"
    } else if s.intersects(Status::WT_DELETED | Status::INDEX_DELETED) {
        "deleted"
    } else if s.intersects(Status::WT_RENAMED | Status::INDEX_RENAMED) {
        "renamed"
    } else {
        "modified"
    }
}

/// The files that make the working tree DIRTY.
pub fn changed_files(dir: &Path, limit: usize) -> Result<Vec<ChangedFile>, String> {
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false)
        .exclude_submodules(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| e.message().to_string())?;
    Ok(statuses
        .iter()
        .take(limit)
        .map(|e| ChangedFile {
            path: e.path().unwrap_or("?").to_string(),
            status: status_label(e.status()),
        })
        .collect())
}

/// Fetch `origin` and fast-forward the checked-out branch to
/// `origin/<branch>`. This is the only operation that changes the working
/// tree, and it refuses to do so unless the update is a clean fast-forward:
/// the tracked branch must be checked out, tracked files must be unmodified
/// and there must be no local commits missing from origin.
pub fn pull_ff(dir: &Path, branch: &str) -> Result<PullOutcome, String> {
    fetch(dir)?;
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    let err = |e: git2::Error| e.message().to_string();

    let head = repo.head().map_err(err)?;
    if !head.is_branch() || head.shorthand() != Some(branch) {
        return Err(format!(
            "the checked-out branch is '{}', not the tracked branch '{branch}'",
            head.shorthand().unwrap_or("detached HEAD")
        ));
    }
    let head_oid = head.target().ok_or("HEAD does not point at a commit")?;
    let remote_oid = repo
        .refname_to_id(&format!("refs/remotes/origin/{branch}"))
        .map_err(|_| format!("origin has no branch '{branch}'"))?;
    if head_oid == remote_oid {
        return Ok(PullOutcome {
            updated: false,
            from: Some(head_oid.to_string()),
            to: Some(head_oid.to_string()),
            commits: 0,
        });
    }
    let (ahead, behind) = repo.graph_ahead_behind(head_oid, remote_oid).map_err(err)?;
    if ahead > 0 {
        return Err(format!(
            "the local branch has {ahead} commit(s) that are not on origin; a fast-forward is not possible"
        ));
    }

    let mut opts = StatusOptions::new();
    opts.include_untracked(false)
        .include_ignored(false)
        .exclude_submodules(true);
    let modified = repo.statuses(Some(&mut opts)).map_err(err)?.len();
    if modified > 0 {
        return Err(format!(
            "{modified} tracked file(s) have local changes; commit or discard them before pulling"
        ));
    }

    let object = repo.find_object(remote_oid, None).map_err(err)?;
    let mut checkout = CheckoutBuilder::new();
    checkout.safe();
    repo.checkout_tree(&object, Some(&mut checkout))
        .map_err(|e| format!("checkout failed: {}", e.message()))?;
    let refname = format!("refs/heads/{branch}");
    repo.find_reference(&refname)
        .and_then(|mut r| r.set_target(remote_oid, "portalcontrol: fast-forward"))
        .map_err(err)?;
    repo.set_head(&refname).map_err(err)?;
    Ok(PullOutcome {
        updated: true,
        from: Some(head_oid.to_string()),
        to: Some(remote_oid.to_string()),
        commits: behind,
    })
}

/// Roll the checked-out branch back to an earlier commit and update the
/// working tree to match. Like `pull_ff` it refuses anything that could lose
/// work: the tracked branch must be checked out, tracked files must be
/// unmodified, HEAD must have no commits missing from origin, and the commit
/// must be part of `origin/<branch>` (so *Pull* can always bring it forward
/// again). No network access.
pub fn rollback_to(dir: &Path, branch: &str, sha: &str) -> Result<PullOutcome, String> {
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    let err = |e: git2::Error| e.message().to_string();
    let target = Oid::from_str(sha.trim()).map_err(|_| "that is not a valid commit id".to_string())?;
    repo.find_commit(target)
        .map_err(|_| "that commit does not exist in this repository".to_string())?;

    let head = repo.head().map_err(err)?;
    if !head.is_branch() || head.shorthand() != Some(branch) {
        return Err(format!(
            "the checked-out branch is '{}', not the tracked branch '{branch}'",
            head.shorthand().unwrap_or("detached HEAD")
        ));
    }
    let head_oid = head.target().ok_or("HEAD does not point at a commit")?;
    if head_oid == target {
        return Ok(PullOutcome {
            updated: false,
            from: Some(head_oid.to_string()),
            to: Some(target.to_string()),
            commits: 0,
        });
    }
    let remote_oid = repo
        .refname_to_id(&format!("refs/remotes/origin/{branch}"))
        .map_err(|_| format!("origin has no branch '{branch}', so a rollback could not be undone"))?;
    let on_origin = |oid: Oid| oid == remote_oid || repo.graph_descendant_of(remote_oid, oid).unwrap_or(false);
    if !on_origin(head_oid) {
        return Err("the local branch has commits that are not on origin; a rollback would lose them".into());
    }
    if !on_origin(target) {
        return Err(format!("that commit is not part of origin/{branch}"));
    }

    let mut opts = StatusOptions::new();
    opts.include_untracked(false)
        .include_ignored(false)
        .exclude_submodules(true);
    let modified = repo.statuses(Some(&mut opts)).map_err(err)?.len();
    if modified > 0 {
        return Err(format!(
            "{modified} tracked file(s) have local changes; commit or discard them before rolling back"
        ));
    }

    let object = repo.find_object(target, None).map_err(err)?;
    let mut checkout = CheckoutBuilder::new();
    checkout.safe();
    repo.checkout_tree(&object, Some(&mut checkout))
        .map_err(|e| format!("checkout failed: {}", e.message()))?;
    let refname = format!("refs/heads/{branch}");
    repo.find_reference(&refname)
        .and_then(|mut r| r.set_target(target, "portalcontrol: rollback"))
        .map_err(err)?;
    repo.set_head(&refname).map_err(err)?;
    let (_, behind) = repo.graph_ahead_behind(target, head_oid).unwrap_or((0, 0));
    Ok(PullOutcome {
        updated: true,
        from: Some(head_oid.to_string()),
        to: Some(target.to_string()),
        commits: behind,
    })
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
    if let Some(prev) = previous.as_ref().filter(|p| p.is_repo && info.is_repo) {
        if info.behind > prev.behind {
            state.activity(
                "info",
                "git",
                Some(target_id),
                format!(
                    "{} new commit(s) available on origin/{}{}",
                    info.behind,
                    info.tracked_branch,
                    info.remote_summary
                        .as_deref()
                        .map(|s| format!(": {s}"))
                        .unwrap_or_default()
                ),
            );
        } else if prev.status != info.status {
            state.activity(
                "info",
                "git",
                Some(target_id),
                format!(
                    "Git state changed from {} to {}",
                    prev.status.replace('_', " "),
                    info.status.replace('_', " ")
                ),
            );
        }
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
                        let info = refresh_target(
                            &st,
                            t.id,
                            t.working_directory.clone(),
                            t.git_branch.clone(),
                            do_fetch,
                        )
                        .await;
                        if do_fetch && t.auto_deploy {
                            crate::automation::auto_deploy(&st, &t, &info).await;
                        }
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
    fn commit_details_history_and_changed_files() {
        let origin_dir = tempfile::tempdir().unwrap();
        let origin = init_repo(origin_dir.path());
        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::clone(origin_dir.path().to_str().unwrap(), clone_dir.path()).unwrap();

        let info = inspect(clone_dir.path(), "main");
        assert_eq!(info.head_summary.as_deref(), Some("initial"));
        assert_eq!(info.head_author.as_deref(), Some("Tester"));
        assert!(info.head_time.unwrap() > 1_600_000_000);
        assert!(info.remote_url.is_some());

        std::fs::write(origin_dir.path().join("app.py"), "print('v2')\n").unwrap();
        commit_all(&origin, "remote change");
        fetch(clone_dir.path()).unwrap();
        std::fs::write(clone_dir.path().join("local.txt"), "x").unwrap();
        commit_all(&clone, "local change");
        std::fs::write(clone_dir.path().join("notes.txt"), "wip").unwrap();
        std::fs::write(clone_dir.path().join("app.py"), "print('edited')\n").unwrap();

        let log = recent_commits(clone_dir.path(), "main", 10).unwrap();
        let states: Vec<(&str, &str)> = log.iter().map(|c| (c.summary.as_str(), c.state)).collect();
        assert_eq!(
            states,
            vec![("remote change", "incoming"), ("local change", "local"), ("initial", "synced")]
        );
        assert_eq!(inspect(clone_dir.path(), "main").remote_summary.as_deref(), Some("remote change"));

        let mut files = changed_files(clone_dir.path(), 50).unwrap();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let files: Vec<(&str, &str)> = files.iter().map(|f| (f.path.as_str(), f.status)).collect();
        assert_eq!(files, vec![("app.py", "modified"), ("notes.txt", "untracked")]);
    }

    #[test]
    fn pull_fast_forwards_only_when_it_is_safe() {
        let origin_dir = tempfile::tempdir().unwrap();
        let origin = init_repo(origin_dir.path());
        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::clone(origin_dir.path().to_str().unwrap(), clone_dir.path()).unwrap();

        // Nothing to do.
        assert!(!pull_ff(clone_dir.path(), "main").unwrap().updated);

        // Remote moves ahead by two commits; an untracked file does not block the pull.
        std::fs::write(origin_dir.path().join("app.py"), "print('v2')\n").unwrap();
        commit_all(&origin, "second");
        std::fs::write(origin_dir.path().join("new.py"), "print('new')\n").unwrap();
        let tip = commit_all(&origin, "third");
        std::fs::write(clone_dir.path().join("db.sqlite3"), "local data").unwrap();
        let out = pull_ff(clone_dir.path(), "main").unwrap();
        assert!(out.updated);
        assert_eq!(out.commits, 2);
        assert_eq!(out.to, Some(tip.to_string()));
        // (`contains`: a global core.autocrlf setting may change line endings.)
        assert!(std::fs::read_to_string(clone_dir.path().join("app.py")).unwrap().contains("v2"));
        assert!(clone_dir.path().join("new.py").exists());
        assert_eq!(std::fs::read_to_string(clone_dir.path().join("db.sqlite3")).unwrap(), "local data");
        let info = inspect(clone_dir.path(), "main");
        assert_eq!((info.ahead, info.behind), (0, 0));
        assert_eq!(info.head, Some(tip.to_string()));

        // A modified tracked file blocks the pull and is left untouched.
        std::fs::write(origin_dir.path().join("app.py"), "print('v3')\n").unwrap();
        commit_all(&origin, "fourth");
        std::fs::write(clone_dir.path().join("app.py"), "print('mine')\n").unwrap();
        assert!(pull_ff(clone_dir.path(), "main").unwrap_err().contains("local changes"));
        assert_eq!(std::fs::read_to_string(clone_dir.path().join("app.py")).unwrap(), "print('mine')\n");

        // A local commit (diverged history) blocks it too.
        commit_all(&clone, "local work");
        assert!(pull_ff(clone_dir.path(), "main").unwrap_err().contains("fast-forward"));

        // So does tracking a branch that is not the one checked out.
        assert!(pull_ff(clone_dir.path(), "release").unwrap_err().contains("tracked branch"));
    }

    #[test]
    fn rollback_goes_back_and_pull_brings_it_forward_again() {
        let origin_dir = tempfile::tempdir().unwrap();
        let origin = init_repo(origin_dir.path());
        let first = origin.head().unwrap().target().unwrap();
        std::fs::write(origin_dir.path().join("app.py"), "print('v2')\n").unwrap();
        std::fs::write(origin_dir.path().join("extra.py"), "print('extra')\n").unwrap();
        let second = commit_all(&origin, "second");
        let clone_dir = tempfile::tempdir().unwrap();
        let clone = Repository::clone(origin_dir.path().to_str().unwrap(), clone_dir.path()).unwrap();
        let dir = clone_dir.path();
        std::fs::write(dir.join("db.sqlite3"), "local data").unwrap();

        // Back to the first commit: files follow, untracked data is untouched.
        let out = rollback_to(dir, "main", &first.to_string()).unwrap();
        assert!(out.updated);
        assert_eq!(out.commits, 1);
        assert!(std::fs::read_to_string(dir.join("app.py")).unwrap().contains("v1"));
        assert!(!dir.join("extra.py").exists());
        assert_eq!(std::fs::read_to_string(dir.join("db.sqlite3")).unwrap(), "local data");
        let info = inspect(dir, "main");
        assert_eq!(info.head, Some(first.to_string()));
        assert_eq!((info.ahead, info.behind), (0, 1));
        assert!(!rollback_to(dir, "main", &first.to_string()).unwrap().updated);

        // Pull undoes the rollback.
        assert!(pull_ff(dir, "main").unwrap().updated);
        assert_eq!(inspect(dir, "main").head, Some(second.to_string()));
        assert!(dir.join("extra.py").exists());

        // Refusals: bad ids, local edits, unpushed commits, commits not on origin.
        assert!(rollback_to(dir, "main", "not-a-sha").is_err());
        assert!(rollback_to(dir, "main", &"0".repeat(40)).is_err());
        std::fs::write(dir.join("app.py"), "print('mine')\n").unwrap();
        assert!(rollback_to(dir, "main", &first.to_string()).unwrap_err().contains("local changes"));
        let local = commit_all(&clone, "local only");
        assert!(rollback_to(dir, "main", &first.to_string()).unwrap_err().contains("not on origin"));
        assert_eq!(inspect(dir, "main").head, Some(local.to_string()));
    }

    #[test]
    fn credentials_are_stripped_from_remote_urls() {
        assert_eq!(
            strip_credentials("https://user:secret@example.com/org/repo.git"),
            "https://example.com/org/repo.git"
        );
        assert_eq!(strip_credentials("https://example.com/r.git"), "https://example.com/r.git");
        assert_eq!(strip_credentials("git@example.com:org/repo.git"), "git@example.com:org/repo.git");
    }

    #[test]
    fn fetch_without_origin_fails_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        assert!(fetch(dir.path()).unwrap_err().contains("origin"));
    }
}

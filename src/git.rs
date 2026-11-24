//! Native Git delta tracking (PRD module 2). Everything goes through libgit2
//! (git2-rs); no `git` executable is ever spawned.

use crate::state::AppState;
use crate::util::{log, now_secs};
use git2::{
    build::CheckoutBuilder, Cred, CredentialType, FetchOptions, Oid, RemoteCallbacks, Repository, Status,
    StatusOptions,
};
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const UP_TO_DATE: &str = "UP_TO_DATE";
pub const UPDATES_AVAILABLE: &str = "UPDATES_AVAILABLE";
pub const DIRTY: &str = "DIRTY";

/// How often remotes are fetched (PRD: every 5 minutes).
const FETCH_INTERVAL: Duration = Duration::from_secs(300);
/// How often the local working tree is re-inspected (no network).
const INSPECT_INTERVAL: Duration = Duration::from_secs(30);

/// The `git_dir` setting that searches upward from the working directory.
pub const AUTO: &str = "auto";

/// The folder of a target's repository (its working tree), from the
/// working directory and the target's `git_dir` setting:
///
/// * empty: the working directory itself (what every target had before 0.14.0);
/// * `auto`: the nearest folder at or above the working directory that holds
///   a repository, for a project kept in a subfolder of one;
/// * a folder: that folder, wherever it is; a relative path is taken from the
///   working directory. Choosing the `.git` folder itself means its parent.
pub fn repo_dir_of(working_directory: &str, git_dir: Option<&str>) -> PathBuf {
    let base = Path::new(working_directory);
    match git_dir.map(str::trim).filter(|s| !s.is_empty()) {
        None => base.to_path_buf(),
        Some(s) if s.eq_ignore_ascii_case(AUTO) => Repository::discover(base)
            .ok()
            .and_then(|r| {
                r.workdir().map(|w| PathBuf::from(w.to_string_lossy().trim_end_matches(['/', '\\'])))
            })
            .unwrap_or_else(|| base.to_path_buf()),
        Some(s) => {
            let p = Path::new(s);
            let p = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
            if p.file_name().is_some_and(|n| n == ".git") {
                p.parent().map(Path::to_path_buf).unwrap_or(p)
            } else {
                p
            }
        }
    }
}

/// The repository folder of a target, as a string for the workers.
pub fn repo_dir(t: &crate::db::Target) -> String {
    repo_dir_of(&t.working_directory, t.git_dir.as_deref()).to_string_lossy().into_owned()
}

/// Check a `git_dir` setting before it is saved. `auto` is always accepted
/// (a target need not be in a repository); a chosen folder must exist and
/// hold a repository, so a typing error is caught at once.
pub fn check_repo_dir(working_directory: &str, git_dir: &str) -> Result<(), String> {
    if git_dir.eq_ignore_ascii_case(AUTO) {
        return Ok(());
    }
    let dir = repo_dir_of(working_directory, Some(git_dir));
    if !dir.is_dir() {
        return Err(format!("the folder {} does not exist", dir.display()));
    }
    Repository::open(&dir)
        .map(|_| ())
        .map_err(|_| format!("{} is not a Git repository (it has no .git folder)", dir.display()))
}

/// Whether a file name is an env file that holds real values: `.env`,
/// `.env.production`, `prod.env`, but not `.env.example` and the like.
pub fn is_secret_env_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (n == ".env" || n.starts_with(".env.") || n.ends_with(".env"))
        && !["example", "sample", "template", "dist", "default"].iter().any(|w| n.contains(w))
}

/// Env files that are committed to the repository (in its index), so their
/// secrets are in its history and on every clone. At most 20.
pub fn tracked_env_files(dir: &Path) -> Vec<String> {
    let Ok(repo) = Repository::open(dir) else { return Vec::new() };
    let Ok(index) = repo.index() else { return Vec::new() };
    index
        .iter()
        .filter_map(|e| String::from_utf8(e.path).ok())
        .filter(|p| is_secret_env_name(p.rsplit('/').next().unwrap_or(p)))
        .take(20)
        .collect()
}

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

/// Access tokens for private HTTPS remotes, by host (`github.com`). Loaded
/// from the database at start and replaced when they change in Settings;
/// kept here so every libgit2 network call (clone, fetch, pull) can use them.
static TOKENS: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

pub fn set_tokens(tokens: HashMap<String, String>) {
    if let Ok(mut t) = TOKENS.write() {
        *t = Some(tokens);
    }
}

/// The host of a remote URL, lower-case and without user or port:
/// `https://user@GitHub.com:443/a/b.git` -> `github.com`.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = host.split(':').next()?.trim().to_lowercase();
    (!host.is_empty()).then_some(host)
}

fn token_for(url: &str) -> Option<String> {
    let host = url_host(url)?;
    TOKENS.read().ok()?.as_ref()?.get(&host).cloned()
}

/// Credentials for a network operation: a saved token for the host first,
/// then the user's Git credential helper, then the platform default.
fn credential_callbacks<'a>(config: git2::Config) -> RemoteCallbacks<'a> {
    let mut attempts = 0;
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |url, username, allowed| {
        attempts += 1;
        if attempts > 3 {
            return Err(git2::Error::from_str(
                "authentication failed: for a private repository, add an access token for this host",
            ));
        }
        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            if let Some(token) = token_for(url) {
                // GitHub, GitLab, Gitea and Bitbucket accept a token as the
                // password with any user name.
                return Cred::userpass_plaintext(username.unwrap_or("portalcontrol"), &token);
            }
            if let Ok(cred) = Cred::credential_helper(&config, url, username) {
                return Ok(cred);
            }
        }
        if allowed.contains(CredentialType::DEFAULT) {
            return Cred::default();
        }
        Err(git2::Error::from_str(
            "the repository was not found, or it is private: check the URL, or save an access token for this host",
        ))
    });
    callbacks
}

/// Is this a remote libgit2 can clone here? SSH is not compiled in.
pub fn check_clone_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("enter the repository URL".into());
    }
    if url.starts_with("git@") || url.starts_with("ssh://") {
        return Err("SSH addresses are not supported; use the HTTPS address \
                    (for a private repository, add an access token)"
            .into());
    }
    if url.contains(char::is_whitespace) {
        return Err("the URL contains spaces".into());
    }
    if !(url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("file://")
        || Path::new(url).is_absolute())
    {
        return Err("use an https:// address".into());
    }
    Ok(())
}

/// The folder name a repository would get: `https://x/acme/shop.git` -> `shop`.
pub fn repo_folder_name(url: &str) -> String {
    let last = url.trim().trim_end_matches('/').rsplit(['/', '\\', ':']).next().unwrap_or("");
    let name: String = last
        .trim_end_matches(".git")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let name = name.trim_matches(['-', '.']).to_string();
    if name.is_empty() {
        "project".into()
    } else {
        name
    }
}

#[derive(Debug, Serialize)]
pub struct Cloned {
    pub dir: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// Clone `url` into `dest`, which must not exist yet or be an empty folder.
/// `branch` checks out that branch instead of the remote's default.
/// `progress(received_objects, total_objects)` is called as data arrives.
pub fn clone_repo(
    url: &str,
    dest: &Path,
    branch: Option<&str>,
    mut progress: impl FnMut(usize, usize),
) -> Result<Cloned, String> {
    check_clone_url(url)?;
    if dest.exists() && std::fs::read_dir(dest).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(format!("{} already exists and is not empty", dest.display()));
    }
    let config = git2::Config::open_default().map_err(|e| e.message().to_string())?;
    let mut callbacks = credential_callbacks(config);
    let mut last = 0;
    callbacks.transfer_progress(move |p| {
        // Report every 2% or so, not every object.
        let step = (p.total_objects() / 50).max(1);
        if p.received_objects() >= last + step || p.received_objects() == p.total_objects() {
            last = p.received_objects();
            progress(p.received_objects(), p.total_objects());
        }
        true
    });
    let mut fetch = FetchOptions::new();
    fetch.remote_callbacks(callbacks);
    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch);
    if let Some(b) = branch.map(str::trim).filter(|b| !b.is_empty()) {
        builder.branch(b);
    }
    let created = !dest.exists();
    let repo = builder.clone(url.trim(), dest).map_err(|e| {
        if created {
            // Leave nothing half-cloned behind.
            let _ = std::fs::remove_dir_all(dest);
        }
        e.message().to_string()
    })?;
    let head = repo.head().ok();
    Ok(Cloned {
        dir: dest.to_path_buf(),
        branch: head.as_ref().and_then(|h| h.shorthand().map(str::to_string)),
        head: head.and_then(|h| h.target()).map(|o| o.to_string()),
    })
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

/// Version of the libgit2 compiled into this binary, for the About page.
pub fn library_version() -> String {
    let (major, minor, rev) = git2::Version::get().libgit2_version();
    format!("{major}.{minor}.{rev}")
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
            return GitInfo::not_repo(branch, if missing { None } else { Some(e.message().to_string()) });
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
    info.checked_out_branch =
        head_ref.as_ref().filter(|h| h.is_branch()).and_then(|h| h.shorthand().map(str::to_string));
    let head = head_ref.and_then(|h| h.target());
    let remote = repo.refname_to_id(&format!("refs/remotes/origin/{branch}")).ok();
    info.head = head.map(|o| o.to_string());
    info.remote_head = remote.map(|o| o.to_string());
    if let Some(c) = head.and_then(|h| commit_info(&repo, h, "synced")) {
        info.head_summary = Some(c.summary);
        info.head_author = Some(c.author);
        info.head_time = Some(c.time);
    }
    info.remote_summary = remote.and_then(|r| commit_info(&repo, r, "synced")).map(|c| c.summary);
    info.remote_url = repo.find_remote("origin").ok().and_then(|r| r.url().map(strip_credentials));

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
    let mut remote = repo.find_remote("origin").map_err(|_| "no 'origin' remote configured".to_string())?;
    let config = repo.config().map_err(|e| e.message().to_string())?;
    let callbacks = credential_callbacks(config);
    let mut options = FetchOptions::new();
    options.remote_callbacks(callbacks);
    remote.fetch(&[] as &[&str], Some(&mut options), None).map_err(|e| e.message().to_string())
}

/// Recent history: commits waiting on `origin/<branch>` first, then the
/// commits of HEAD, each marked incoming / local / synced.
pub fn recent_commits(dir: &Path, branch: &str, limit: usize) -> Result<Vec<CommitInfo>, String> {
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    let err = |e: git2::Error| e.message().to_string();
    let head = repo.head().ok().and_then(|h| h.target());
    let remote = repo.refname_to_id(&format!("refs/remotes/origin/{branch}")).ok();
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
    let statuses = repo.statuses(Some(&mut opts)).map_err(|e| e.message().to_string())?;
    Ok(statuses
        .iter()
        .take(limit)
        .map(|e| ChangedFile { path: e.path().unwrap_or("?").to_string(), status: status_label(e.status()) })
        .collect())
}

/// A change in the working tree, and whether it stands in the way of a pull.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LocalChange {
    pub path: String,
    pub status: &'static str,
    /// The commits waiting on origin change this path too.
    pub blocks: bool,
}

/// What a pull would do, worked out without touching anything (no fetch).
#[derive(Debug, Clone, Serialize, Default)]
pub struct PullPlan {
    pub behind: usize,
    pub ahead: usize,
    /// Files the waiting commits change (at most 500).
    pub incoming: Vec<ChangedFile>,
    pub local: Vec<LocalChange>,
    /// The local changes with `blocks` set.
    pub blocked_by: Vec<String>,
    pub can_pull: bool,
    /// Why a pull is not possible, when there is something to pull.
    pub reason: Option<String>,
}

fn plan_in(repo: &Repository, branch: &str) -> Result<PullPlan, String> {
    let err = |e: git2::Error| e.message().to_string();
    let mut plan = PullPlan::default();

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false)
        .exclude_submodules(true);
    plan.local = repo
        .statuses(Some(&mut opts))
        .map_err(err)?
        .iter()
        .take(500)
        .map(|e| LocalChange {
            path: e.path().unwrap_or("?").to_string(),
            status: status_label(e.status()),
            blocks: false,
        })
        .collect();

    let head = repo.head().map_err(err)?;
    let on_branch = head.is_branch() && head.shorthand() == Some(branch);
    let head_oid = head.target().ok_or("HEAD does not point at a commit")?;
    let Ok(remote_oid) = repo.refname_to_id(&format!("refs/remotes/origin/{branch}")) else {
        plan.reason = Some(format!("origin has no branch '{branch}'"));
        return Ok(plan);
    };
    if head_oid == remote_oid {
        return Ok(plan);
    }
    let (ahead, behind) = repo.graph_ahead_behind(head_oid, remote_oid).map_err(err)?;
    plan.ahead = ahead;
    plan.behind = behind;

    let head_tree = repo.find_commit(head_oid).and_then(|c| c.tree()).map_err(err)?;
    let remote_tree = repo.find_commit(remote_oid).and_then(|c| c.tree()).map_err(err)?;
    let diff = repo.diff_tree_to_tree(Some(&head_tree), Some(&remote_tree), None).map_err(err)?;
    let mut touched: HashSet<String> = HashSet::new();
    for delta in diff.deltas() {
        let name = |f: git2::DiffFile| f.path().map(|p| p.to_string_lossy().replace('\\', "/"));
        let (old, new) = (name(delta.old_file()), name(delta.new_file()));
        let status = match delta.status() {
            git2::Delta::Added => "added",
            git2::Delta::Deleted => "deleted",
            git2::Delta::Renamed => "renamed",
            _ => "modified",
        };
        if let Some(path) = new.clone().or_else(|| old.clone()) {
            if plan.incoming.len() < 500 {
                plan.incoming.push(ChangedFile { path, status });
            }
        }
        touched.extend(old);
        touched.extend(new);
    }
    for change in &mut plan.local {
        // An untracked folder is listed as `folder/`.
        let folder =
            if change.path.ends_with('/') { change.path.clone() } else { format!("{}/", change.path) };
        change.blocks = touched.contains(&change.path) || touched.iter().any(|p| p.starts_with(&folder));
    }
    plan.blocked_by = plan.local.iter().filter(|c| c.blocks).map(|c| c.path.clone()).collect();

    plan.reason = if !on_branch {
        Some(format!(
            "the checked-out branch is '{}', not the tracked branch '{branch}'",
            head.shorthand().unwrap_or("detached HEAD")
        ))
    } else if ahead > 0 {
        Some(format!(
            "the local branch has {ahead} commit(s) that are not on origin; a fast-forward is not possible"
        ))
    } else if !plan.blocked_by.is_empty() {
        Some(format!(
            "{} file(s) with local changes are also changed by the incoming commits",
            plan.blocked_by.len()
        ))
    } else {
        None
    };
    plan.can_pull = behind > 0 && plan.reason.is_none();
    Ok(plan)
}

/// What a pull would do right now: the files the waiting commits change, the
/// local changes, and which of those are in the way.
pub fn pull_plan(dir: &Path, branch: &str) -> Result<PullPlan, String> {
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    plan_in(&repo, branch)
}

/// The change to one file as text: what was changed here (`mine`, against
/// the checked-out commit) and what the waiting commits change (`incoming`).
#[derive(Debug, Clone, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub mine: String,
    pub incoming: String,
    pub truncated: bool,
}

/// Longest diff text returned for one side.
const DIFF_LIMIT: usize = 200 * 1024;

fn patch_text(diff: &git2::Diff) -> (String, bool) {
    let mut out = String::new();
    let mut cut = false;
    // Stopping early makes `print` report an error; the text so far is kept.
    let _ = diff.print(git2::DiffFormat::Patch, |_, _, line| {
        if out.len() > DIFF_LIMIT {
            cut = true;
            return false;
        }
        if matches!(line.origin(), '+' | '-' | ' ') {
            out.push(line.origin());
        }
        out.push_str(&String::from_utf8_lossy(line.content()));
        true
    });
    (out, cut)
}

/// `path` must be one of the paths `pull_plan` lists; anything else is refused.
pub fn file_diff(dir: &Path, branch: &str, path: &str) -> Result<FileDiff, String> {
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    let err = |e: git2::Error| e.message().to_string();
    let plan = plan_in(&repo, branch)?;
    if !plan.local.iter().any(|c| c.path == path) && !plan.incoming.iter().any(|c| c.path == path) {
        return Err("that file is not among the changes".into());
    }
    let head_tree = repo.head().and_then(|h| h.peel_to_tree()).map_err(err)?;
    let only = |opts: &mut git2::DiffOptions| {
        opts.pathspec(path).disable_pathspec_match(true);
    };

    let mut opts = git2::DiffOptions::new();
    only(&mut opts);
    opts.include_untracked(true).recurse_untracked_dirs(true).show_untracked_content(true);
    let (mine, cut_mine) = repo
        .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))
        .map(|d| patch_text(&d))
        .map_err(err)?;

    let mut incoming = (String::new(), false);
    if let Ok(remote) = repo.refname_to_id(&format!("refs/remotes/origin/{branch}")) {
        let remote_tree = repo.find_commit(remote).and_then(|c| c.tree()).map_err(err)?;
        let mut opts = git2::DiffOptions::new();
        only(&mut opts);
        incoming = repo
            .diff_tree_to_tree(Some(&head_tree), Some(&remote_tree), Some(&mut opts))
            .map(|d| patch_text(&d))
            .map_err(err)?;
    }
    Ok(FileDiff { path: path.to_string(), mine, incoming: incoming.0, truncated: cut_mine || incoming.1 })
}

/// What `discard_changes` did.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Discarded {
    /// Tracked files put back as they are in the checked-out commit.
    pub restored: Vec<String>,
    /// Files that are not in the repository, moved out of the folder.
    pub removed: Vec<String>,
    /// Where the copies of what was there are kept.
    pub backup: String,
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let kind = std::fs::symlink_metadata(from)?.file_type();
    if kind.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else if kind.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// Undo local changes to the given paths, after copying what is there now
/// into `backup` (same relative paths). A tracked file goes back to the
/// checked-out commit; a file that is not in the repository is moved out.
/// Every path must be one that the status lists, exactly as it lists it.
pub fn discard_changes(dir: &Path, paths: &[String], backup: &Path) -> Result<Discarded, String> {
    let repo = Repository::open(dir).map_err(|e| e.message().to_string())?;
    let err = |e: git2::Error| e.message().to_string();
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false)
        .exclude_submodules(true);
    let known: HashMap<String, Status> = repo
        .statuses(Some(&mut opts))
        .map_err(err)?
        .iter()
        .filter_map(|e| e.path().map(|p| (p.to_string(), e.status())))
        .collect();

    let mut out =
        Discarded { restored: Vec::new(), removed: Vec::new(), backup: backup.display().to_string() };
    let mut plan: Vec<(&String, bool)> = Vec::new();
    for path in paths {
        let status = known.get(path).ok_or_else(|| format!("'{path}' has no local change"))?;
        let plain = Path::new(path).components().all(|c| matches!(c, std::path::Component::Normal(_)));
        if !plain {
            return Err(format!("'{path}' is not a path inside the project"));
        }
        plan.push((path, status.intersects(Status::WT_NEW | Status::INDEX_NEW)));
    }
    if plan.is_empty() {
        return Err("no files were named".into());
    }

    // Copies first: nothing is changed unless every copy was written.
    for (path, _) in &plan {
        let from = dir.join(path);
        if std::fs::symlink_metadata(&from).is_ok() {
            copy_tree(&from, &backup.join(path))
                .map_err(|e| format!("could not keep a copy of '{path}': {e}"))?;
        }
    }

    let mut checkout = CheckoutBuilder::new();
    checkout.force().disable_pathspec_match(true);
    for (path, new) in &plan {
        if *new {
            let full = dir.join(path);
            let gone = match std::fs::symlink_metadata(&full) {
                Ok(m) if m.is_dir() => std::fs::remove_dir_all(&full),
                Ok(_) => std::fs::remove_file(&full),
                Err(_) => Ok(()),
            };
            gone.map_err(|e| format!("could not remove '{path}': {e}"))?;
            if let Ok(mut index) = repo.index() {
                if index.remove_path(Path::new(path)).is_ok() {
                    let _ = index.write();
                }
            }
            out.removed.push((*path).clone());
        } else {
            checkout.path(path);
            out.restored.push((*path).clone());
        }
    }
    // Without a path the checkout would put back every file.
    if !out.restored.is_empty() {
        repo.checkout_head(Some(&mut checkout)).map_err(|e| format!("restore failed: {}", e.message()))?;
    }
    Ok(out)
}

/// Fetch `origin` and fast-forward the checked-out branch to
/// `origin/<branch>`. This is the only operation that changes the working
/// tree, and it refuses to do so unless the update is a clean fast-forward:
/// the tracked branch must be checked out, there must be no local commits
/// missing from origin, and no file changed here may also be changed by the
/// commits being pulled. Local changes to other files stay as they are.
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

    // Local changes are in the way only where the pulled commits change the
    // same file; the others are left exactly as they are.
    let blocked = plan_in(&repo, branch)?.blocked_by;
    if !blocked.is_empty() {
        let shown: Vec<&str> = blocked.iter().take(5).map(String::as_str).collect();
        return Err(format!(
            "{} file(s) with local changes are also changed by the incoming commits ({}{}); discard those changes or move the files first",
            blocked.len(),
            shown.join(", "),
            if blocked.len() > shown.len() { ", ..." } else { "" }
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
    repo.find_commit(target).map_err(|_| "that commit does not exist in this repository".to_string())?;

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
    let on_origin =
        |oid: Oid| oid == remote_oid || repo.graph_descendant_of(remote_oid, oid).unwrap_or(false);
    if !on_origin(head_oid) {
        return Err("the local branch has commits that are not on origin; a rollback would lose them".into());
    }
    if !on_origin(target) {
        return Err(format!("that commit is not part of origin/{branch}"));
    }

    let mut opts = StatusOptions::new();
    opts.include_untracked(false).include_ignored(false).exclude_submodules(true);
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
    let previous = state.git.lock().ok().and_then(|g| g.get(&target_id).cloned());

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
        Err(e) => (GitInfo::not_repo("", Some(format!("git worker failed: {e}"))), None),
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
                    info.remote_summary.as_deref().map(|s| format!(": {s}")).unwrap_or_default()
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
            let do_fetch = tick.is_multiple_of(ticks_per_fetch);
            if let Ok(targets) = state.db.list_targets().await {
                let ids: Vec<i64> = targets.iter().map(|t| t.id).collect();
                for t in targets {
                    let st = state.clone();
                    tokio::spawn(async move {
                        let dir = repo_dir(&t);
                        let info = refresh_target(&st, t.id, dir, t.git_branch.clone(), do_fetch).await;
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

    #[test]
    fn clone_urls_and_folder_names() {
        assert_eq!(url_host("https://user@GitHub.com:443/a/b.git").as_deref(), Some("github.com"));
        assert_eq!(url_host("http://gitlab.local/x").as_deref(), Some("gitlab.local"));
        assert_eq!(url_host("not a url"), None);
        assert!(check_clone_url("git@github.com:a/b.git").unwrap_err().contains("SSH"));
        assert!(check_clone_url("ftp://x/y").is_err());
        assert!(check_clone_url("https://github.com/a/b").is_ok());
        assert_eq!(repo_folder_name("https://github.com/acme/shop.git"), "shop");
        assert_eq!(repo_folder_name("https://github.com/acme/my shop/"), "my-shop");
        assert_eq!(repo_folder_name("https://x/"), "x");
    }

    #[test]
    fn clones_into_a_new_folder_and_refuses_a_full_one() {
        let (_upstream, bare) = bare_with_commit();
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("app");
        // (A local clone copies objects directly, so it reports no transfer progress.)
        let c = clone_repo(bare.to_str().unwrap(), &dest, None, |_, _| {}).unwrap();
        assert!(dest.join(".git").is_dir() && c.head.is_some());
        assert!(c.branch.is_some());
        let err = clone_repo(bare.to_str().unwrap(), &dest, None, |_, _| {}).unwrap_err();
        assert!(err.contains("not empty"));
        // A failed clone leaves no folder behind.
        let missing = parent.path().join("nothing");
        assert!(clone_repo("/definitely/not/a/repo", &missing, None, |_, _| {}).is_err());
        assert!(!missing.exists());
    }

    /// A bare repository with one commit, as a clone source.
    fn bare_with_commit() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let repo = Repository::init(&work).unwrap();
        std::fs::write(work.join("README.md"), "hello").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        let bare = dir.path().join("origin.git");
        git2::build::RepoBuilder::new().bare(true).clone(work.to_str().unwrap(), &bare).unwrap();
        (dir, bare)
    }
    use git2::{IndexAddOption, Signature};

    fn commit_all(repo: &Repository, msg: &str) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = Signature::now("Tester", "tester@example.com").unwrap();
        let parents: Vec<git2::Commit> =
            repo.head().ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
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
    fn pull_names_what_is_in_the_way_and_keeps_the_other_local_changes() {
        let origin_dir = tempfile::tempdir().unwrap();
        let origin = init_repo(origin_dir.path());
        std::fs::write(origin_dir.path().join("run.bat"), "run v1\n").unwrap();
        std::fs::write(origin_dir.path().join("start.bat"), "start v1\n").unwrap();
        commit_all(&origin, "scripts");
        let clone_dir = tempfile::tempdir().unwrap();
        Repository::clone(origin_dir.path().to_str().unwrap(), clone_dir.path()).unwrap();
        let dir = clone_dir.path();

        // Origin changes app.py and adds a migration.
        std::fs::write(origin_dir.path().join("app.py"), "print('v2')\n").unwrap();
        std::fs::create_dir(origin_dir.path().join("migrations")).unwrap();
        std::fs::write(origin_dir.path().join("migrations").join("0002.py"), "theirs\n").unwrap();
        commit_all(&origin, "second");
        fetch(dir).unwrap();

        // Here: an edited script, a deleted one, notes, and a migration of the same name.
        std::fs::write(dir.join("run.bat"), "mine\n").unwrap();
        std::fs::remove_file(dir.join("start.bat")).unwrap();
        std::fs::write(dir.join("notes.txt"), "wip").unwrap();
        std::fs::create_dir(dir.join("migrations")).unwrap();
        std::fs::write(dir.join("migrations").join("0002.py"), "made here\n").unwrap();

        let plan = pull_plan(dir, "main").unwrap();
        assert_eq!((plan.behind, plan.ahead), (1, 0));
        assert_eq!(plan.blocked_by, vec!["migrations/".to_string()]);
        assert!(!plan.can_pull && plan.reason.as_deref().unwrap().contains("1 file(s)"));
        assert_eq!(plan.local.len(), 4);
        let mut incoming: Vec<(&str, &str)> =
            plan.incoming.iter().map(|f| (f.path.as_str(), f.status)).collect();
        incoming.sort();
        assert_eq!(incoming, vec![("app.py", "modified"), ("migrations/0002.py", "added")]);
        let err = pull_ff(dir, "main").unwrap_err();
        assert!(err.contains("migrations/") && err.contains("local changes"), "{err}");

        // The text of a change, mine and incoming; other paths are refused.
        let mine = file_diff(dir, "main", "run.bat").unwrap();
        assert!(mine.mine.contains("-run v1") && mine.mine.contains("+mine") && mine.incoming.is_empty());
        assert!(file_diff(dir, "main", "app.py").unwrap().incoming.contains("v2"));
        assert!(file_diff(dir, "main", "migrations/").unwrap().mine.contains("made here"));
        assert!(file_diff(dir, "main", "../secret").is_err());

        // Moving the file in the way out keeps a copy, and the pull then works
        // without touching the other local changes.
        let backup = tempfile::tempdir().unwrap();
        let done = discard_changes(dir, &["migrations/".to_string()], backup.path()).unwrap();
        assert_eq!((done.restored.len(), done.removed), (0, vec!["migrations/".to_string()]));
        assert_eq!(
            std::fs::read_to_string(backup.path().join("migrations").join("0002.py")).unwrap(),
            "made here\n"
        );
        assert!(!dir.join("migrations").exists());
        assert!(pull_plan(dir, "main").unwrap().can_pull);
        assert!(pull_ff(dir, "main").unwrap().updated);
        assert!(std::fs::read_to_string(dir.join("app.py")).unwrap().contains("v2"));
        assert!(std::fs::read_to_string(dir.join("migrations").join("0002.py")).unwrap().contains("theirs"));
        assert_eq!(std::fs::read_to_string(dir.join("run.bat")).unwrap(), "mine\n");
        assert!(!dir.join("start.bat").exists());
        assert!(dir.join("notes.txt").exists());

        // Undoing changes to tracked files puts them back and keeps what was there.
        let done =
            discard_changes(dir, &["run.bat".to_string(), "start.bat".to_string()], backup.path()).unwrap();
        assert_eq!(done.restored.len(), 2);
        assert!(std::fs::read_to_string(dir.join("run.bat")).unwrap().contains("run v1"));
        assert!(dir.join("start.bat").exists());
        assert_eq!(std::fs::read_to_string(backup.path().join("run.bat")).unwrap(), "mine\n");
        assert!(dir.join("notes.txt").exists(), "files that were not named are left alone");
        assert!(discard_changes(dir, &["app.py".to_string()], backup.path()).is_err());
        assert!(discard_changes(dir, &[], backup.path()).is_err());
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

    #[test]
    fn the_repository_can_be_above_or_away_from_the_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let repo_path = root.path().join("repo");
        let work = repo_path.join("src").join("app");
        std::fs::create_dir_all(&work).unwrap();
        let repo = init_repo(&repo_path);
        let w = work.to_str().unwrap();
        let same =
            |a: PathBuf, b: &Path| std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap();

        assert_eq!(repo_dir_of(w, None), work, "empty: the working directory");
        assert!(same(repo_dir_of(w, Some("auto")), &repo_path), "auto: found above");
        assert!(same(repo_dir_of(w, Some("../..")), &repo_path), "relative to the working directory");
        assert_eq!(
            repo_dir_of(w, Some(repo_path.join(".git").to_str().unwrap())),
            repo_path,
            ".git means its parent"
        );
        assert!(inspect(&repo_dir_of(w, Some("auto")), "main").is_repo);

        assert!(check_repo_dir(w, "../..").is_ok());
        assert!(check_repo_dir(w, "auto").is_ok());
        assert!(check_repo_dir(w, ".").unwrap_err().contains("not a Git repository"));
        assert!(check_repo_dir(w, "missing").unwrap_err().contains("does not exist"));
        let lonely = tempfile::tempdir().unwrap();
        let l = lonely.path().to_str().unwrap();
        assert_eq!(repo_dir_of(l, Some("auto")), lonely.path(), "auto without a repository stays put");

        std::fs::write(repo_path.join(".env"), "A=1\n").unwrap();
        std::fs::write(repo_path.join(".env.example"), "A=\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".env")).unwrap();
        index.add_path(Path::new(".env.example")).unwrap();
        index.write().unwrap();
        assert_eq!(tracked_env_files(&repo_path), [".env".to_string()]);
        assert!(is_secret_env_name("prod.env") && !is_secret_env_name(".env.sample"));
    }
}

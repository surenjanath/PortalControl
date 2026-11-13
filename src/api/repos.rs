//! Cloning a repository for a new target, and the access tokens used for
//! private HTTPS remotes (stored sealed, never sent back to the browser).

use super::*;

/// Settings key of the saved tokens: JSON `{host: sealed token}`.
pub(crate) const TOKENS_KEY: &str = "git_tokens";
const TOKEN_AAD: &str = "git_token";

async fn stored_tokens(state: &AppState) -> HashMap<String, String> {
    state
        .db
        .get_setting(TOKENS_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

/// Decrypt the saved tokens and hand them to the Git layer. Called at start
/// and after every change.
pub(crate) async fn load_git_tokens(state: &AppState) {
    let plain: HashMap<String, String> = stored_tokens(state)
        .await
        .into_iter()
        .filter_map(|(host, sealed)| crate::secrets::open(&sealed, TOKEN_AAD).map(|t| (host, t)))
        .collect();
    git::set_tokens(plain);
}

pub(super) async fn tokens_list(State(state): State<Arc<AppState>>) -> AppResult {
    let mut hosts: Vec<String> = stored_tokens(&state).await.into_keys().collect();
    hosts.sort();
    Ok(ok(json!({ "hosts": hosts })))
}

#[derive(serde::Deserialize)]
pub(super) struct TokenRequest {
    pub(super) host: String,
    /// Empty removes the token for this host.
    #[serde(default)]
    pub(super) token: String,
}

pub(super) async fn tokens_put(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TokenRequest>,
) -> AppResult {
    // Accept a bare host or a whole URL.
    let host = git::url_host(&req.host)
        .or_else(|| git::url_host(&format!("https://{}", req.host.trim())))
        .filter(|h| h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "enter a host name such as github.com"))?;
    let token = req.token.trim();
    if token.len() > 500 || token.contains(char::is_whitespace) {
        return Err(fail(StatusCode::BAD_REQUEST, "that does not look like an access token"));
    }
    let mut map = stored_tokens(&state).await;
    if token.is_empty() {
        map.remove(&host);
    } else {
        map.insert(host.clone(), crate::secrets::seal(token, TOKEN_AAD));
    }
    let value = (!map.is_empty()).then(|| serde_json::to_string(&map).unwrap_or_default());
    state.db.set_setting(TOKENS_KEY, value).await.map_err(internal)?;
    load_git_tokens(&state).await;
    let what = if token.is_empty() { "removed" } else { "saved" };
    state.activity("info", "git", None, format!("Access token for {host} {what}"));
    Ok(ok(json!({ "host": host, "saved": !token.is_empty() })))
}

#[derive(serde::Deserialize)]
pub(super) struct CloneRequest {
    pub(super) url: String,
    /// Existing folder the repository goes into.
    pub(super) parent: String,
    /// Name of the new folder; defaults to the repository's name.
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) branch: String,
}

/// Clone into `<parent>/<name>`. Progress arrives as `clone_progress` events;
/// the answer comes when the clone is complete.
pub(super) async fn clone(State(state): State<Arc<AppState>>, Json(req): Json<CloneRequest>) -> AppResult {
    git::check_clone_url(&req.url).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let parent = std::path::PathBuf::from(req.parent.trim());
    if !parent.is_absolute() || !parent.is_dir() {
        return Err(fail(StatusCode::BAD_REQUEST, "choose an existing folder to clone into"));
    }
    let name = if req.name.trim().is_empty() {
        git::repo_folder_name(&req.url)
    } else {
        req.name.trim().to_string()
    };
    if name == "." || name == ".." || name.contains(['/', '\\', ':']) || name.chars().count() > 120 {
        return Err(fail(StatusCode::BAD_REQUEST, "the folder name cannot contain / \\ or :"));
    }
    let dest = parent.join(&name);
    let shown = git::strip_credentials(req.url.trim());
    state.activity("info", "git", None, format!("Cloning {shown} into {}", dest.display()));
    let (url, branch, d, st) = (req.url.clone(), req.branch.clone(), dest.clone(), state.clone());
    let dest_text = dest.display().to_string();
    let result = tokio::task::spawn_blocking(move || {
        git::clone_repo(&url, &d, Some(&branch), |received, total| {
            st.emit(
                json!({"type": "clone_progress", "dest": dest_text, "received": received, "total": total}),
            );
        })
    })
    .await
    .map_err(|e| internal(e.to_string()))?;
    match result {
        Ok(c) => {
            state.activity(
                "ok",
                "git",
                None,
                format!(
                    "Cloned {shown} into {} ({})",
                    c.dir.display(),
                    c.branch.as_deref().unwrap_or("detached")
                ),
            );
            Ok(ok(json!(c)))
        }
        Err(e) => {
            state.activity("error", "git", None, format!("Cloning {shown} failed: {e}"));
            Err(fail(StatusCode::BAD_REQUEST, e))
        }
    }
}

//! SQLite database snapshots of a target: list, create, delete, restore.

use super::*;

pub(super) fn data_dir(state: &Arc<AppState>) -> Result<std::path::PathBuf, Response> {
    state.data_dir.get().cloned().ok_or_else(|| internal("data directory is not configured".into()))
}

pub(super) async fn list_backups(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    load_target(&state, id).await?;
    let dir = data_dir(&state)?;
    let shown = backup::dir_for(&dir, id).display().to_string();
    let list = tokio::task::spawn_blocking(move || backup::list(&dir, id))
        .await
        .map_err(|e| internal(e.to_string()))?;
    Ok(ok(json!({"backups": list, "directory": shown, "keep": backup::KEEP})))
}

pub(super) async fn create_backup(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> AppResult {
    let t = load_target(&state, id).await?;
    let dir = data_dir(&state)?;
    let made = tokio::task::spawn_blocking(move || backup::create(&dir, &t))
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    state.activity("ok", "backup", Some(id), format!("Database backup created: {}", made.file));
    Ok((StatusCode::CREATED, Json(json!(made))).into_response())
}

pub(super) async fn delete_backup(
    State(state): State<Arc<AppState>>,
    Path((id, file)): Path<(i64, String)>,
) -> AppResult {
    load_target(&state, id).await?;
    let dir = data_dir(&state)?;
    let name = file.clone();
    tokio::task::spawn_blocking(move || backup::delete(&dir, id, &name))
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    Ok(ok(json!({"deleted": file})))
}

pub(super) async fn restore_backup(
    State(state): State<Arc<AppState>>,
    Path((id, file)): Path<(i64, String)>,
) -> AppResult {
    let t = load_target(&state, id).await?;
    {
        let r = state.running.lock().await;
        if r.contains_key(&id) || r.contains_key(&runner::command_slot(id)) {
            return Err(fail(
                StatusCode::CONFLICT,
                "stop the target (and any running command) before restoring its database",
            ));
        }
    }
    let dir = data_dir(&state)?;
    let name = file.clone();
    let safety = tokio::task::spawn_blocking(move || backup::restore(&dir, &t, &name))
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    state.activity(
        "warn",
        "backup",
        Some(id),
        format!("Database restored from {file}; the previous state was saved as {}", safety.file),
    );
    Ok(ok(json!({"restored": file, "safety_backup": safety})))
}

//! SQLite backups of a target's database.
//!
//! A backup is a consistent snapshot taken with `VACUUM INTO` through a
//! read-only connection, so it is safe while the application is running and
//! never modifies the source. Snapshots live in PortalControl's data
//! directory (`backups/<target id>/`), not in the project folder.

use crate::db::Target;
use crate::runner;
use crate::state::AppState;
use crate::util::{fmt_utc, now_secs};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

/// Snapshots kept per target; older ones are deleted after each backup.
pub const KEEP: usize = 14;
const EXT: &str = "sqlite3";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BackupInfo {
    pub file: String,
    pub size: u64,
    /// Unix seconds.
    pub created: u64,
}

/// The database path must be a plain relative path inside the working
/// directory (same rules as a script name) and must exist.
pub fn resolve_db(working_directory: &str, db_file: &str) -> Result<PathBuf, String> {
    runner::validate_script_name(db_file).map_err(|e| e.replace("script name", "database file"))?;
    let path = Path::new(working_directory).join(db_file);
    if !path.is_file() {
        return Err(format!("database file '{db_file}' was not found in {working_directory}"));
    }
    Ok(path)
}

pub fn dir_for(data_dir: &Path, target_id: i64) -> PathBuf {
    data_dir.join("backups").join(target_id.to_string())
}

/// A backup file name is only ever one we generated: no path separators.
fn checked_name(file: &str) -> Result<&str, String> {
    let ok = !file.is_empty()
        && file.len() <= 200
        && file.ends_with(&format!(".{EXT}"))
        && !file.starts_with('.')
        && file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(file)
    } else {
        Err("invalid backup file name".into())
    }
}

/// All snapshots of a target, newest first.
pub fn list(data_dir: &Path, target_id: i64) -> Vec<BackupInfo> {
    // Sorted on the full-precision modification time, so several snapshots
    // taken within one second still come out in the order they were made.
    let mut out: Vec<(Duration, BackupInfo)> = std::fs::read_dir(dir_for(data_dir, target_id))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let file = e.file_name().to_string_lossy().into_owned();
                    checked_name(&file).ok()?;
                    let meta = e.metadata().ok()?;
                    let modified = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .unwrap_or_default();
                    Some((modified, BackupInfo { file, size: meta.len(), created: modified.as_secs() }))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.file.cmp(&a.1.file)));
    out.into_iter().map(|(_, info)| info).collect()
}

/// Take a snapshot now and prune old ones. Blocking: call off the runtime.
pub fn create(data_dir: &Path, target: &Target) -> Result<BackupInfo, String> {
    let db_file = target.db_file.as_deref().ok_or("no database file is configured for this target")?;
    let source = resolve_db(&target.working_directory, db_file)?;
    let dest_dir = dir_for(data_dir, target.id);
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("cannot create {}: {e}", dest_dir.display()))?;

    let stem: String = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "db".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    // "2026-01-31 12:00:00" -> "20260131-120000"
    let stamp = fmt_utc(now_secs()).replace(['-', ':'], "").replace(' ', "-");
    let mut name = format!("{stem}-{stamp}.{EXT}");
    let mut n = 2;
    while dest_dir.join(&name).exists() {
        name = format!("{stem}-{stamp}-{n}.{EXT}");
        n += 1;
    }
    let dest = dest_dir.join(&name);

    let conn = Connection::open_with_flags(
        &source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("cannot open {db_file}: {e}"))?;
    let _ = conn.busy_timeout(Duration::from_secs(15));
    if let Err(e) = conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()]) {
        let _ = std::fs::remove_file(&dest);
        return Err(format!("backup of {db_file} failed: {e}"));
    }
    drop(conn);

    for old in list(data_dir, target.id).into_iter().skip(KEEP) {
        if old.file != name {
            let _ = std::fs::remove_file(dest_dir.join(old.file));
        }
    }
    list(data_dir, target.id)
        .into_iter()
        .find(|b| b.file == name)
        .ok_or_else(|| "backup was written but could not be read back".to_string())
}

pub fn delete(data_dir: &Path, target_id: i64, file: &str) -> Result<(), String> {
    let path = dir_for(data_dir, target_id).join(checked_name(file)?);
    if !path.is_file() {
        return Err("backup not found".into());
    }
    std::fs::remove_file(&path).map_err(|e| format!("cannot delete backup: {e}"))
}

/// Replace the target's database with a snapshot. The current database is
/// snapshotted first, so a restore can itself be undone. The caller must
/// make sure the target is not running.
pub fn restore(data_dir: &Path, target: &Target, file: &str) -> Result<BackupInfo, String> {
    let backup = dir_for(data_dir, target.id).join(checked_name(file)?);
    if !backup.is_file() {
        return Err("backup not found".into());
    }
    let db_file = target.db_file.as_deref().ok_or("no database file is configured for this target")?;
    let live = resolve_db(&target.working_directory, db_file)?;
    let safety = create(data_dir, target)
        .map_err(|e| format!("could not snapshot the current database first: {e}"))?;
    std::fs::copy(&backup, &live).map_err(|e| format!("restore failed: {e}"))?;
    // Stale journal files belong to the database that was just replaced.
    for suffix in ["-wal", "-shm"] {
        let mut side = live.clone().into_os_string();
        side.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(side));
    }
    Ok(safety)
}

/// Snapshot before a run when the target asks for it. A failure is reported
/// but never blocks the run.
pub async fn before_run(state: &Arc<AppState>, target: &Target) {
    let Some(data_dir) = state.data_dir.get().cloned() else { return };
    if target.db_file.is_none() {
        return;
    }
    let t = target.clone();
    let result = tokio::task::spawn_blocking(move || create(&data_dir, &t)).await;
    match result {
        Ok(Ok(b)) => state.activity(
            "info",
            "backup",
            Some(target.id),
            format!("Database backed up before the run: {} ({} KB)", b.file, b.size / 1024),
        ),
        Ok(Err(e)) => state.activity(
            "warn",
            "backup",
            Some(target.id),
            format!("Database backup before the run failed: {e}"),
        ),
        Err(e) => state.activity("warn", "backup", Some(target.id), format!("Backup task failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(dir: &Path, db_file: Option<&str>) -> Target {
        Target {
            id: 7,
            name: "t".into(),
            working_directory: dir.to_string_lossy().into_owned(),
            script_name: "deploy.sh".into(),
            git_branch: "main".into(),
            current_git_status: "UP_TO_DATE".into(),
            created_at: String::new(),
            health_url: None,
            run_mode: "TASK".into(),
            autostart: false,
            timeout_secs: 300,
            description: String::new(),
            project: "Default".into(),
            env_vars: String::new(),
            schedule: None,
            auto_deploy: false,
            auto_restart: false,
            webhook_token: None,
            db_file: db_file.map(str::to_string),
            backup_on_deploy: true,
            start_order: 0,
            python_env: None,
        }
    }

    fn rows(path: &Path) -> Vec<String> {
        let c = Connection::open(path).unwrap();
        let mut st = c.prepare("SELECT name FROM claims ORDER BY id").unwrap();
        let out = st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        out
    }

    #[test]
    fn backup_restore_round_trip() {
        let project = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let db = project.path().join("db.sqlite3");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("CREATE TABLE claims (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO claims (name) VALUES ('first');")
                .unwrap();
        }
        let t = target(project.path(), Some("db.sqlite3"));

        let first = create(data.path(), &t).unwrap();
        assert!(first.size > 0 && first.file.starts_with("db-") && first.file.ends_with(".sqlite3"));
        assert_eq!(rows(&dir_for(data.path(), 7).join(&first.file)), vec!["first"]);

        Connection::open(&db).unwrap().execute("INSERT INTO claims (name) VALUES ('second')", []).unwrap();
        assert_eq!(rows(&db), vec!["first", "second"]);

        // Restoring brings back the old content and keeps a snapshot of what was replaced.
        let safety = restore(data.path(), &t, &first.file).unwrap();
        assert_eq!(rows(&db), vec!["first"]);
        assert_ne!(safety.file, first.file);
        assert_eq!(rows(&dir_for(data.path(), 7).join(&safety.file)), vec!["first", "second"]);
        assert_eq!(list(data.path(), 7).len(), 2);

        delete(data.path(), 7, &first.file).unwrap();
        assert_eq!(list(data.path(), 7).len(), 1);
        assert!(delete(data.path(), 7, &first.file).is_err());
    }

    #[test]
    fn old_backups_are_pruned_and_bad_input_is_rejected() {
        let project = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        Connection::open(project.path().join("db.sqlite3"))
            .unwrap()
            .execute_batch("CREATE TABLE claims (id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        let t = target(project.path(), Some("db.sqlite3"));
        for _ in 0..(KEEP + 3) {
            create(data.path(), &t).unwrap();
        }
        assert_eq!(list(data.path(), 7).len(), KEEP);

        assert!(create(data.path(), &target(project.path(), None)).is_err());
        assert!(create(data.path(), &target(project.path(), Some("missing.sqlite3"))).is_err());
        assert!(resolve_db(&t.working_directory, "../db.sqlite3").is_err());
        std::fs::write(project.path().join("notes.txt"), "not a database").unwrap();
        assert!(create(data.path(), &target(project.path(), Some("notes.txt"))).is_err());
        assert_eq!(list(data.path(), 7).len(), KEEP, "a failed backup leaves nothing behind");

        for bad in ["../x.sqlite3", "a/b.sqlite3", "x.txt", "", ".hidden.sqlite3"] {
            assert!(delete(data.path(), 7, bad).is_err(), "{bad:?}");
            assert!(restore(data.path(), &t, bad).is_err(), "{bad:?}");
        }
    }
}

//! Installing PortalControl on this computer.
//!
//! A release is one program file (Windows, Linux) or one application bundle
//! (macOS). Installing copies it to one fixed place per user and replaces the
//! copy that is there. A new version therefore always takes the place of the
//! previous one, and everything that starts PortalControl (the login entry,
//! the Start Menu shortcut) keeps pointing at the same path:
//!
//! * Windows: `%LOCALAPPDATA%\PortalControl\bin\portalcontrol.exe`
//! * macOS:   `/Applications/PortalControl.app` (`~/Applications` when that
//!   folder cannot be written); a bare binary goes to `~/.local/bin`
//! * Linux:   `~/.local/bin/portalcontrol`
//!
//! `PORTALCONTROL_INSTALL_DIR` names another folder. The data folder is not
//! touched: targets, history and settings stay as they are.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Folder to install into instead of the usual one.
pub const INSTALL_DIR_ENV: &str = "PORTALCONTROL_INSTALL_DIR";
/// Set to skip the question a double-clicked copy asks when it is not installed.
pub const NO_OFFER_ENV: &str = "PORTALCONTROL_NO_INSTALL";

const BUNDLE_NAME: &str = "PortalControl.app";
const PROGRAM_NAME: &str = if cfg!(windows) { "portalcontrol.exe" } else { "portalcontrol" };

/// A copy of PortalControl on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    /// What is copied when installing: the program file, or on macOS the
    /// whole `.app` folder.
    pub root: PathBuf,
    /// The program file itself.
    pub exe: PathBuf,
}

/// `/x/PortalControl.app/Contents/MacOS/portalcontrol` -> `/x/PortalControl.app`.
fn bundle_root(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && app.extension()? == "app")
        .then(|| app.to_path_buf())
}

/// The copy that is running now.
pub fn running() -> Result<Location, String> {
    let exe = crate::autostart::current_exe()?;
    let bundle = if cfg!(target_os = "macos") { bundle_root(&exe) } else { None };
    Ok(Location { root: bundle.unwrap_or_else(|| exe.clone()), exe })
}

/// Where `from` ends up when it is installed into `dir`.
fn place(from: &Location, dir: &Path) -> Location {
    match from.exe.strip_prefix(&from.root) {
        // An application bundle: the program keeps its place inside it.
        Ok(inside) if !inside.as_os_str().is_empty() => {
            let root = dir.join(BUNDLE_NAME);
            Location { exe: root.join(inside), root }
        }
        _ => {
            let exe = dir.join(PROGRAM_NAME);
            Location { root: exe.clone(), exe }
        }
    }
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".portalcontrol-{}", std::process::id()));
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Where the copy `from` belongs once it is installed.
pub fn destination(from: &Location) -> Result<Location, String> {
    if let Some(dir) = std::env::var_os(INSTALL_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(place(from, Path::new(&dir)));
    }
    if cfg!(windows) {
        return Ok(place(from, &crate::default_data_dir().join("bin")));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set")?;
    let dir = if from.root == from.exe {
        home.join(".local/bin")
    } else if writable(Path::new("/Applications")) {
        PathBuf::from("/Applications")
    } else {
        home.join("Applications")
    };
    Ok(place(from, &dir))
}

/// Whether two paths name the same file.
pub fn same_path(a: &Path, b: &Path) -> bool {
    let full = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let (a, b) = (full(a), full(b));
    if cfg!(windows) {
        a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// Whether the running copy is the installed one.
pub fn is_installed() -> bool {
    running().is_ok_and(|me| destination(&me).is_ok_and(|to| same_path(&me.exe, &to.exe)))
}

/// macOS runs an application that was opened straight from a download from
/// a temporary, read-only copy ("app translocation"). That path is gone
/// after a restart, so nothing may be registered to start from it.
pub fn is_temporary(exe: &Path) -> bool {
    exe.to_string_lossy().contains("/AppTranslocation/")
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

/// Copy a file, or a folder with everything in it.
fn copy_any(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(std::fs::read_link(from)?, to);
        #[cfg(not(unix))]
        return std::fs::copy(from, to).map(|_| ());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_any(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Copies that were moved aside by an install, next to `to`.
fn leftovers(to: &Path) -> Vec<PathBuf> {
    let (Some(parent), Some(name)) = (to.parent(), to.file_name()) else { return Vec::new() };
    let prefix = format!(".{}.old", name.to_string_lossy());
    std::fs::read_dir(parent)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

/// Delete the copies an install moved aside. One that is still running
/// cannot be deleted on Windows; `false` says some are left.
fn clear_leftovers(to: &Path) -> bool {
    let mut all_gone = true;
    for old in leftovers(to) {
        all_gone &= remove_any(&old).is_ok();
    }
    all_gone
}

/// Put a copy of `from` at `to`, replacing what is there.
///
/// The new copy is written completely before anything is touched, and the
/// old one is moved aside instead of overwritten, which also works while it
/// is still running (Windows locks a running program against writing, not
/// against renaming). If the last step fails the old copy is put back.
pub fn replace(from: &Path, to: &Path) -> Result<(), String> {
    let parent = to.parent().ok_or("invalid install path")?;
    let name = to.file_name().ok_or("invalid install path")?.to_string_lossy().into_owned();
    std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;

    let fresh = parent.join(format!(".{name}.new"));
    let _ = remove_any(&fresh);
    if let Err(e) = copy_any(from, &fresh) {
        let _ = remove_any(&fresh);
        return Err(format!("cannot copy {} to {}: {e}", from.display(), parent.display()));
    }

    let had = std::fs::symlink_metadata(to).is_ok();
    if had {
        // A copy left from an earlier install may still be running.
        let old = if clear_leftovers(to) {
            parent.join(format!(".{name}.old"))
        } else {
            parent.join(format!(".{name}.old-{}", std::process::id()))
        };
        if let Err(e) = std::fs::rename(to, &old) {
            let _ = remove_any(&fresh);
            return Err(format!("cannot move the installed copy {} aside: {e}", to.display()));
        }
        if let Err(e) = std::fs::rename(&fresh, to) {
            let _ = std::fs::rename(&old, to);
            let _ = remove_any(&fresh);
            return Err(format!("cannot put the new copy at {}: {e}", to.display()));
        }
    } else if let Err(e) = std::fs::rename(&fresh, to) {
        let _ = remove_any(&fresh);
        return Err(format!("cannot put the new copy at {}: {e}", to.display()));
    }
    clear_leftovers(to);
    Ok(())
}

/// Wait until the copy that `replace` moved aside can be deleted, which on
/// Windows means the program has ended. `false` when `limit` ran out first.
pub fn wait_until_released(to: &Path, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    loop {
        if clear_leftovers(to) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// What the person chose when a copy that is not installed was double-clicked.
#[derive(Debug, PartialEq)]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub enum Offer {
    Install,
    RunHere,
    Quit,
}

fn offer_text(to: &Path, replacing: bool) -> String {
    format!(
        "Install PortalControl {} on this computer?\n\nIt is copied to\n{}\n{}and starts from there from now on, also at login when that is turned on. Your targets, history and settings are kept.",
        env!("CARGO_PKG_VERSION"),
        to.display(),
        if replacing { "where it replaces the version that is installed now, " } else { "" },
    )
}

/// Ask whether to install, when this copy was started by a double click and
/// is not the installed one. Anything that stands in the way of asking means
/// "just run".
pub fn offer() -> Offer {
    if cfg!(debug_assertions) || std::env::var_os(NO_OFFER_ENV).is_some() {
        return Offer::RunHere;
    }
    let Ok(me) = running() else { return Offer::RunHere };
    let Ok(to) = destination(&me) else { return Offer::RunHere };
    if same_path(&me.exe, &to.exe) {
        return Offer::RunHere;
    }
    ask(&offer_text(&to.root, to.exe.exists()))
}

#[cfg(windows)]
fn message_box(text: &str, flags: u32) -> i32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, title) = (wide(text), wide("PortalControl"));
    // SAFETY: both strings are NUL-terminated and live until the call returns.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), flags) }
}

#[cfg(windows)]
fn ask(text: &str) -> Offer {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDNO, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNOCANCEL,
    };
    let text = format!("{text}\n\nYes: install it.\nNo: only run it from this folder this time.");
    match message_box(&text, MB_YESNOCANCEL | MB_ICONQUESTION | MB_SETFOREGROUND) {
        IDYES => Offer::Install,
        IDNO => Offer::RunHere,
        _ => Offer::Quit,
    }
}

/// A string literal for AppleScript.
#[cfg(any(target_os = "macos", test))]
fn applescript_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped.replace('\n', "\" & return & \""))
}

#[cfg(target_os = "macos")]
fn ask(text: &str) -> Offer {
    let script = format!(
        "display dialog {} with title \"PortalControl\" buttons {{\"Quit\", \"Run From Here\", \"Install\"}} default button \"Install\" cancel button \"Quit\"",
        applescript_string(text)
    );
    match std::process::Command::new("osascript").args(["-e", &script]).output() {
        Ok(out) if out.status.success() => {
            if String::from_utf8_lossy(&out.stdout).contains("Run From Here") {
                Offer::RunHere
            } else {
                Offer::Install
            }
        }
        // The dialog was cancelled.
        Ok(_) => Offer::Quit,
        Err(_) => Offer::RunHere,
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn ask(_text: &str) -> Offer {
    Offer::RunHere
}

/// Show the outcome of an install that was started by a double click, where
/// there is no terminal to print it in.
pub fn tell(text: &str, failed: bool) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND,
        };
        let icon = if failed { MB_ICONERROR } else { MB_ICONINFORMATION };
        message_box(text, MB_OK | icon | MB_SETFOREGROUND);
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display dialog {} with title \"PortalControl\" buttons {{\"OK\"}} default button \"OK\"{}",
            applescript_string(text),
            if failed { " with icon stop" } else { "" }
        );
        let _ = std::process::Command::new("osascript").args(["-e", &script]).output();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = failed;
        eprintln!("{text}");
    }
}

/// Windows: a Start Menu entry for the installed program, so it can be found
/// and started by name. Skipped for an install into a folder of one's own.
pub fn add_shortcut(exe: &Path) {
    if std::env::var_os(INSTALL_DIR_ENV).is_some() {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let Some(appdata) = std::env::var_os("APPDATA") else { return };
        let link = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\PortalControl.lnk");
        // The paths travel in environment variables, so no quoting can go wrong.
        let script = "$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:PC_LINK);\
                      $s.TargetPath=$env:PC_EXE;\
                      $s.WorkingDirectory=(Split-Path -Parent $env:PC_EXE);\
                      $s.Description='PortalControl';$s.Save()";
        let done = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("PC_LINK", &link)
            .env("PC_EXE", exe)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .status();
        if !done.is_ok_and(|s| s.success()) {
            eprintln!("note: the Start Menu shortcut could not be created");
        }
    }
    #[cfg(not(windows))]
    let _ = exe;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_inside_an_app_bundle_is_recognised() {
        assert_eq!(
            bundle_root(Path::new("/Users/a/Downloads/PortalControl.app/Contents/MacOS/portalcontrol")),
            Some(PathBuf::from("/Users/a/Downloads/PortalControl.app"))
        );
        assert_eq!(bundle_root(Path::new("/Users/a/.local/bin/portalcontrol")), None);
        assert_eq!(bundle_root(Path::new("/x/MacOS/portalcontrol")), None);
    }

    #[test]
    fn install_place_of_a_program_and_of_a_bundle() {
        let dir = Path::new("/opt/apps");
        let bare = Location { root: "/tmp/dl/pc-0.9".into(), exe: "/tmp/dl/pc-0.9".into() };
        let placed = place(&bare, dir);
        assert_eq!(placed.exe, dir.join(PROGRAM_NAME));
        assert_eq!(placed.root, placed.exe);

        let bundle = Location {
            root: "/tmp/dl/PortalControl.app".into(),
            exe: "/tmp/dl/PortalControl.app/Contents/MacOS/portalcontrol".into(),
        };
        let placed = place(&bundle, dir);
        assert_eq!(placed.root, dir.join("PortalControl.app"));
        assert_eq!(placed.exe, dir.join("PortalControl.app/Contents/MacOS/portalcontrol"));
    }

    #[test]
    fn replace_installs_and_then_overwrites_a_program_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (v1, v2) = (tmp.path().join("v1"), tmp.path().join("v2"));
        std::fs::write(&v1, "one").unwrap();
        std::fs::write(&v2, "two").unwrap();
        let to = tmp.path().join("installed dir").join(PROGRAM_NAME);

        replace(&v1, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "one");
        replace(&v2, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "two");
        // The download is left alone and nothing temporary stays behind.
        assert_eq!(std::fs::read_to_string(&v2).unwrap(), "two");
        assert_eq!(std::fs::read_dir(to.parent().unwrap()).unwrap().count(), 1);
        assert!(wait_until_released(&to, Duration::ZERO));
        assert!(same_path(&to, &to.parent().unwrap().join(".").join(PROGRAM_NAME)));
        assert!(!same_path(&to, &v2));
    }

    #[test]
    fn replace_swaps_a_whole_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let make = |name: &str, body: &str, extra: bool| {
            let app = tmp.path().join(name).join("PortalControl.app");
            std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            std::fs::write(app.join("Contents/MacOS/portalcontrol"), body).unwrap();
            if extra {
                std::fs::write(app.join("Contents/old-only.txt"), "x").unwrap();
            }
            app
        };
        let (old, new) = (make("a", "old", true), make("b", "new", false));
        let to = tmp.path().join("Applications/PortalControl.app");
        replace(&old, &to).unwrap();
        assert!(to.join("Contents/old-only.txt").exists());
        replace(&new, &to).unwrap();
        assert_eq!(std::fs::read_to_string(to.join("Contents/MacOS/portalcontrol")).unwrap(), "new");
        // A file that only the old version had is gone: it is a swap, not a merge.
        assert!(!to.join("Contents/old-only.txt").exists());
        assert_eq!(std::fs::read_dir(to.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_copy_leaves_the_installed_program_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let to = tmp.path().join(PROGRAM_NAME);
        std::fs::write(&to, "installed").unwrap();
        assert!(replace(&tmp.path().join("missing"), &to).is_err());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "installed");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn texts_for_the_dialogs() {
        assert_eq!(applescript_string("a \"b\"\nc\\d"), "\"a \\\"b\\\"\" & return & \"c\\\\d\"");
        let text = offer_text(Path::new("/Applications/PortalControl.app"), true);
        assert!(text.contains("/Applications/PortalControl.app") && text.contains("replaces"));
        assert!(!offer_text(Path::new("/x"), false).contains("replaces"));
        assert!(is_temporary(Path::new(
            "/private/var/folders/ab/T/AppTranslocation/1F2E/d/PortalControl.app/Contents/MacOS/portalcontrol"
        )));
        assert!(!is_temporary(Path::new("/Applications/PortalControl.app/Contents/MacOS/portalcontrol")));
    }
}

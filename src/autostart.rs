//! Register / unregister the manager to start automatically at user login.
//!
//! * Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (no admin rights needed)
//! * macOS:   a launchd agent in `~/Library/LaunchAgents`
//! * Linux:   a systemd user unit in `~/.config/systemd/user`

use std::path::PathBuf;

#[cfg(any(windows, test))]
const RUN_VALUE: &str = "PortalControl";
#[cfg(any(unix, test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LAUNCHD_LABEL: &str = "com.portalcontrol.core";

pub(crate) fn current_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    Ok(exe.canonicalize().map(strip_verbatim).unwrap_or(exe))
}

/// `canonicalize` on Windows yields `\\?\C:\...`; the shell wants plain paths.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

/// Quote one command-line argument for a Windows command line.
#[cfg(any(windows, test))]
fn win_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                backslashes = 0;
                out.push('"');
                continue;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
            }
        }
        if c != '\\' {
            out.push(c);
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(any(windows, test))]
fn windows_command_line(exe: &str, serve_args: &[String]) -> String {
    let mut parts = vec![format!("\"{exe}\""), "serve".to_string(), "--detach".to_string()];
    parts.extend(serve_args.iter().map(|a| win_quote(a)));
    parts.join(" ")
}

/// The program a login entry starts: the first, quoted part of its command
/// line (`"C:\x\portalcontrol.exe" serve --detach`).
#[cfg(any(not(target_os = "macos"), test))]
fn quoted_exe(command: &str) -> Option<&str> {
    command.trim_start().strip_prefix('"')?.split_once('"').map(|(exe, _)| exe)
}

/// The same command line starting another program; the options stay.
#[cfg(any(not(target_os = "macos"), test))]
fn with_quoted_exe(command: &str, exe: &str) -> Option<String> {
    let (_, rest) = command.trim_start().strip_prefix('"')?.split_once('"')?;
    Some(format!("\"{exe}\"{rest}"))
}

#[cfg(any(unix, test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(any(unix, test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Where the program's path sits in a launchd plist: the first string of
/// `ProgramArguments`.
#[cfg(any(unix, test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_exe_span(plist: &str) -> Option<std::ops::Range<usize>> {
    let args = plist.find("<key>ProgramArguments</key>")?;
    let open = args + plist[args..].find("<string>")? + "<string>".len();
    let close = open + plist[open..].find("</string>")?;
    Some(open..close)
}

#[cfg(any(unix, test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_exe(plist: &str) -> Option<String> {
    plist_exe_span(plist).map(|span| xml_unescape(&plist[span]))
}

#[cfg(any(unix, test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_with_exe(plist: &str, exe: &str) -> Option<String> {
    let span = plist_exe_span(plist)?;
    Some(format!("{}{}{}", &plist[..span.start], xml_escape(exe), &plist[span.end..]))
}

#[cfg(any(unix, test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launchd_plist(exe: &str, serve_args: &[String], log_path: &str) -> String {
    let mut args = format!("        <string>{}</string>\n        <string>serve</string>\n", xml_escape(exe));
    for a in serve_args {
        args.push_str(&format!("        <string>{}</string>\n", xml_escape(a)));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        log = xml_escape(log_path)
    )
}

#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn systemd_unit(exe: &str, serve_args: &[String]) -> String {
    let mut cmd = format!("\"{exe}\" serve");
    for a in serve_args {
        cmd.push_str(&format!(" \"{}\"", a.replace('"', "\\\"")));
    }
    format!(
        "[Unit]\nDescription=PortalControl-Core automation supervisor\nAfter=network.target\n\n\
         [Service]\nExecStart={cmd}\nRestart=on-failure\nRestartSec=5\n\n\
         [Install]\nWantedBy=default.target\n"
    )
}

// ---------------------------------------------------------------- Windows --

#[cfg(windows)]
mod platform {
    use super::*;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    use winreg::RegKey;

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn install(serve_args: &[String]) -> Result<String, String> {
        let exe = current_exe()?;
        let command = windows_command_line(&exe.to_string_lossy(), serve_args);
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(RUN_KEY)
            .map_err(|e| format!("cannot open the Run registry key: {e}"))?;
        key.set_value(RUN_VALUE, &command)
            .map_err(|e| format!("cannot write the Run registry value: {e}"))?;
        Ok(format!(
            "Autostart installed. PortalControl will start in the background at every login.\n  \
             HKCU\\{RUN_KEY}\\{RUN_VALUE}\n  = {command}"
        ))
    }

    pub fn uninstall() -> Result<String, String> {
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
            .map_err(|e| format!("cannot open the Run registry key: {e}"))?;
        match key.delete_value(RUN_VALUE) {
            Ok(()) => Ok("Autostart removed.".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok("Autostart was not installed; nothing to remove.".into())
            }
            Err(e) => Err(format!("cannot remove the Run registry value: {e}")),
        }
    }

    pub fn status() -> Result<Option<String>, String> {
        let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_READ) {
            Ok(k) => k,
            Err(_) => return Ok(None),
        };
        Ok(key.get_value::<String, _>(RUN_VALUE).ok())
    }

    pub fn registered_exe() -> Option<PathBuf> {
        status().ok().flatten().and_then(|command| quoted_exe(&command).map(PathBuf::from))
    }

    pub fn repoint(exe: &std::path::Path) -> Result<(), String> {
        let command = status()?.ok_or("start at login is not turned on")?;
        let command = with_quoted_exe(&command, &exe.to_string_lossy())
            .ok_or("the login entry was not written by PortalControl")?;
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(RUN_KEY)
            .map_err(|e| format!("cannot open the Run registry key: {e}"))?;
        key.set_value(RUN_VALUE, &command).map_err(|e| format!("cannot write the Run registry value: {e}"))
    }
}

// ------------------------------------------------------------------ macOS --

/// The launchd agent logic. It is plain file + process code, so it is compiled
/// on every Unix: macOS uses it for real, and the tests execute it on Linux
/// too with a temporary home directory and a stand-in `launchctl`.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod launchd {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    pub fn plist_path(home: &Path) -> PathBuf {
        home.join("Library/LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist"))
    }

    pub fn install_at(
        home: &Path,
        exe: &Path,
        serve_args: &[String],
        log: &Path,
        launchctl: &str,
    ) -> Result<String, String> {
        let path = plist_path(home);
        let dir = path.parent().ok_or("invalid LaunchAgents path")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        if let Some(p) = log.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let plist = launchd_plist(&exe.to_string_lossy(), serve_args, &log.to_string_lossy());
        // Reload cleanly if an older agent is already registered.
        if path.exists() {
            let _ = Command::new(launchctl).arg("unload").arg(&path).output();
        }
        std::fs::write(&path, plist).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let loaded = Command::new(launchctl)
            .args(["load", "-w"])
            .arg(&path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        Ok(format!(
            "Autostart installed: {}\n{}",
            path.display(),
            if loaded {
                "The agent is loaded and PortalControl is running now; it will start at every login."
            } else {
                "It will start at next login (launchctl load did not succeed right now)."
            }
        ))
    }

    pub fn uninstall_at(home: &Path, launchctl: &str) -> Result<String, String> {
        let path = plist_path(home);
        if !path.exists() {
            return Ok("Autostart was not installed; nothing to remove.".into());
        }
        let _ = Command::new(launchctl).args(["unload", "-w"]).arg(&path).output();
        std::fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
        Ok("Autostart removed.".into())
    }

    pub fn status_at(home: &Path) -> Option<String> {
        let path = plist_path(home);
        path.exists().then(|| path.to_string_lossy().into_owned())
    }

    pub fn registered_exe_at(home: &Path) -> Option<PathBuf> {
        plist_exe(&std::fs::read_to_string(plist_path(home)).ok()?).map(PathBuf::from)
    }

    /// Only the file is rewritten. launchd reads it at the next login;
    /// reloading the agent now would start a second manager.
    pub fn repoint_at(home: &Path, exe: &Path) -> Result<(), String> {
        let path = plist_path(home);
        let plist =
            std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let plist = plist_with_exe(&plist, &exe.to_string_lossy())
            .ok_or("the login agent was not written by PortalControl")?;
        std::fs::write(&path, plist).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn home() -> Result<PathBuf, String> {
        std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "HOME is not set".to_string())
    }

    pub fn install(serve_args: &[String]) -> Result<String, String> {
        let exe = current_exe()?;
        if crate::install::is_temporary(&exe) {
            return Err("PortalControl was opened straight from the download, so macOS runs it from a \
                        temporary place that is gone after a restart. Install it first (open it again and \
                        choose Install, or move it to Applications and open it from there), then turn on \
                        start at login."
                .into());
        }
        let log = crate::default_data_dir().join("portalcontrol.launchd.log");
        launchd::install_at(&home()?, &exe, serve_args, &log, "launchctl")
    }

    pub fn registered_exe() -> Option<PathBuf> {
        launchd::registered_exe_at(&home().ok()?)
    }

    pub fn repoint(exe: &std::path::Path) -> Result<(), String> {
        launchd::repoint_at(&home()?, exe)
    }

    pub fn uninstall() -> Result<String, String> {
        launchd::uninstall_at(&home()?, "launchctl")
    }

    pub fn status() -> Result<Option<String>, String> {
        Ok(launchd::status_at(&home()?))
    }
}

// ------------------------------------------------------------------ Linux --

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::*;
    use std::process::Command;

    const UNIT: &str = "portalcontrol.service";

    fn unit_path() -> Result<PathBuf, String> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok_or("HOME is not set")?;
        Ok(base.join("systemd/user").join(UNIT))
    }

    pub fn install(serve_args: &[String]) -> Result<String, String> {
        let exe = current_exe()?;
        let path = unit_path()?;
        let dir = path.parent().ok_or("invalid systemd path")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::write(&path, systemd_unit(&exe.to_string_lossy(), serve_args))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
        let enabled = Command::new("systemctl")
            .args(["--user", "enable", "--now", UNIT])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        Ok(format!(
            "Autostart installed: {}\n{}",
            path.display(),
            if enabled {
                "The unit is enabled and running."
            } else {
                "Run `systemctl --user enable --now portalcontrol.service` to activate it."
            }
        ))
    }

    pub fn uninstall() -> Result<String, String> {
        let path = unit_path()?;
        if !path.exists() {
            return Ok("Autostart was not installed; nothing to remove.".into());
        }
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", UNIT]).output();
        std::fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
        Ok("Autostart removed.".into())
    }

    pub fn status() -> Result<Option<String>, String> {
        let path = unit_path()?;
        Ok(path.exists().then(|| path.to_string_lossy().into_owned()))
    }

    fn exec_line(unit: &str) -> Option<&str> {
        unit.lines().find_map(|l| l.strip_prefix("ExecStart="))
    }

    pub fn registered_exe() -> Option<PathBuf> {
        let unit = std::fs::read_to_string(unit_path().ok()?).ok()?;
        quoted_exe(exec_line(&unit)?).map(PathBuf::from)
    }

    pub fn repoint(exe: &std::path::Path) -> Result<(), String> {
        let path = unit_path()?;
        let unit =
            std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let old = exec_line(&unit).ok_or("the unit has no ExecStart line")?;
        let new = with_quoted_exe(old, &exe.to_string_lossy())
            .ok_or("the login unit was not written by PortalControl")?;
        std::fs::write(&path, unit.replacen(old, &new, 1))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
        Ok(())
    }
}

pub use platform::{install, registered_exe, repoint, status, uninstall};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_command_line_quotes_paths_with_spaces() {
        let cmd = windows_command_line(
            r"C:\Program Files\PortalControl\portalcontrol.exe",
            &["--port".into(), "9000".into(), "--data-dir".into(), r"C:\My Data\pc".into()],
        );
        assert_eq!(
            cmd,
            r#""C:\Program Files\PortalControl\portalcontrol.exe" serve --detach --port 9000 --data-dir "C:\My Data\pc""#
        );
        assert_eq!(win_quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(win_quote("plain"), "plain");
        assert_eq!(RUN_VALUE, "PortalControl");
    }

    #[test]
    fn launchd_plist_is_well_formed() {
        let p = launchd_plist(
            "/Users/ict/bin/portal & control",
            &["--port".into(), "9000".into()],
            "/tmp/pc.log",
        );
        assert!(p.contains("<string>com.portalcontrol.core</string>"));
        assert!(p.contains("<string>/Users/ict/bin/portal &amp; control</string>"));
        assert!(p.contains(
            "<string>serve</string>\n        <string>--port</string>\n        <string>9000</string>"
        ));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert_eq!(p.matches("<dict>").count(), p.matches("</dict>").count());
    }

    /// Executes the real macOS install/uninstall code against a temporary
    /// home directory, with a script standing in for `launchctl` that records
    /// how it was called.
    #[cfg(unix)]
    #[test]
    fn launchd_install_and_uninstall_roundtrip() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home dir");
        std::fs::create_dir_all(&home).unwrap();
        let calls = tmp.path().join("launchctl.calls");
        let fake = tmp.path().join("launchctl");
        std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", calls.display())).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake = fake.to_string_lossy().into_owned();
        let exe = PathBuf::from("/Users/ict/.local/bin/portalcontrol");
        let log = home.join("Library/Application Support/PortalControl/portalcontrol.launchd.log");

        assert_eq!(launchd::status_at(&home), None);
        let msg = launchd::install_at(&home, &exe, &["--port".into(), "9000".into()], &log, &fake).unwrap();
        assert!(msg.contains("loaded and PortalControl is running"), "{msg}");

        let plist_path = home.join("Library/LaunchAgents/com.portalcontrol.core.plist");
        assert_eq!(launchd::status_at(&home), Some(plist_path.to_string_lossy().into_owned()));
        let plist = std::fs::read_to_string(&plist_path).unwrap();
        assert!(plist.contains("<string>/Users/ict/.local/bin/portalcontrol</string>"));
        assert!(plist.contains("<string>9000</string>"));
        assert!(log.parent().unwrap().is_dir());
        let recorded = std::fs::read_to_string(&calls).unwrap();
        assert_eq!(recorded.trim(), format!("load -w {}", plist_path.display()));

        // Re-installing unloads the old agent first, then loads the new one.
        launchd::install_at(&home, &exe, &[], &log, &fake).unwrap();
        let recorded = std::fs::read_to_string(&calls).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("unload ") && lines[2].starts_with("load -w "));
        assert!(!std::fs::read_to_string(&plist_path).unwrap().contains("9000"));

        // Pointing the agent at another copy rewrites the file and leaves launchd alone.
        assert_eq!(launchd::registered_exe_at(&home), Some(exe.clone()));
        let installed = PathBuf::from("/Applications/PortalControl.app/Contents/MacOS/portalcontrol");
        launchd::repoint_at(&home, &installed).unwrap();
        assert_eq!(launchd::registered_exe_at(&home), Some(installed));
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 3);

        assert_eq!(launchd::uninstall_at(&home, &fake).unwrap(), "Autostart removed.");
        assert!(!plist_path.exists());
        assert!(std::fs::read_to_string(&calls).unwrap().lines().last().unwrap().starts_with("unload -w "));
        assert!(launchd::uninstall_at(&home, &fake).unwrap().contains("nothing to remove"));

        // A missing launchctl must not fail the install; it reports "next login".
        let msg = launchd::install_at(&home, &exe, &[], &log, "/nonexistent/launchctl").unwrap();
        assert!(msg.contains("next login"), "{msg}");
    }

    #[test]
    fn a_login_entry_can_be_pointed_at_another_program_keeping_its_options() {
        let old = windows_command_line(
            r"C:\Users\a b\Downloads\PortalControl-0.8.0-windows-x64\portalcontrol.exe",
            &["--port".into(), "9000".into(), "--data-dir".into(), r"C:\My Data\pc".into()],
        );
        assert_eq!(
            quoted_exe(&old),
            Some(r"C:\Users\a b\Downloads\PortalControl-0.8.0-windows-x64\portalcontrol.exe")
        );
        let new = with_quoted_exe(&old, r"C:\Users\a b\AppData\Local\PortalControl\bin\portalcontrol.exe");
        assert_eq!(
            new.as_deref(),
            Some(
                r#""C:\Users\a b\AppData\Local\PortalControl\bin\portalcontrol.exe" serve --detach --port 9000 --data-dir "C:\My Data\pc""#
            )
        );
        // Something a person typed by hand is left alone.
        assert_eq!(quoted_exe("portalcontrol.exe serve"), None);
        assert_eq!(with_quoted_exe("portalcontrol.exe serve", "x"), None);

        let unit = systemd_unit("/old dir/portalcontrol", &["--port".into(), "9000".into()]);
        let line = unit.lines().find_map(|l| l.strip_prefix("ExecStart=")).unwrap();
        assert_eq!(quoted_exe(line), Some("/old dir/portalcontrol"));
        assert_eq!(
            with_quoted_exe(line, "/new/portalcontrol").as_deref(),
            Some("\"/new/portalcontrol\" serve \"--port\" \"9000\"")
        );

        let plist = launchd_plist("/old/A & B.app/Contents/MacOS/portalcontrol", &["--no-tray".into()], "/l");
        assert_eq!(plist_exe(&plist).as_deref(), Some("/old/A & B.app/Contents/MacOS/portalcontrol"));
        let moved = plist_with_exe(&plist, "/Applications/P<1>.app/Contents/MacOS/portalcontrol").unwrap();
        assert_eq!(plist_exe(&moved).as_deref(), Some("/Applications/P<1>.app/Contents/MacOS/portalcontrol"));
        assert!(moved.contains("<string>serve</string>\n        <string>--no-tray</string>"));
        assert_eq!(
            moved.len() - "/Applications/P&lt;1&gt;.app".len(),
            plist.len() - "/old/A &amp; B.app".len()
        );
        assert_eq!(plist_exe("<plist></plist>"), None);
    }

    #[test]
    fn systemd_unit_has_exec_and_install_sections() {
        let u = systemd_unit("/opt/pc/portalcontrol", &["--port".into(), "9000".into()]);
        assert!(u.contains("ExecStart=\"/opt/pc/portalcontrol\" serve \"--port\" \"9000\""));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\C:\a\b.exe")), PathBuf::from(r"C:\a\b.exe"));
        assert_eq!(strip_verbatim(PathBuf::from("/usr/bin/x")), PathBuf::from("/usr/bin/x"));
    }
}

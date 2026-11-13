//! Tray icon: the notification area on Windows, the menu bar on macOS.
//!
//! Both systems want their user-interface loop on the main thread, so when
//! the tray is on, the manager itself runs on a second thread and reports
//! back here: when it is listening (the icon appears), whenever its status
//! changes (tooltip and first menu line), and when it has stopped (the icon
//! goes away and the process exits).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::sync::Notify;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const OPEN: &str = "open";
const SETTINGS: &str = "settings";
const AUTOSTART: &str = "autostart";
const DATA: &str = "data";
const LOG: &str = "log";
const STOP: &str = "stop";
#[cfg(windows)]
const ABOUT: &str = "about";

/// What the tray needs to know about this manager.
pub struct Config {
    /// Dashboard address on this computer, ending in `/`. The manager
    /// replaces it once it knows which port it listens on.
    pub url: String,
    /// The `serve` options in use, remembered when "Start at login" is ticked.
    pub serve_args: Vec<String>,
    pub data_dir: PathBuf,
}

/// What the manager thread gets to talk to the tray.
pub struct Link {
    /// Notified when the user picks "Stop" in the tray menu.
    pub quit: Arc<Notify>,
    proxy: Mutex<EventLoopProxy<Signal>>,
}

impl Link {
    fn send(&self, signal: Signal) {
        if let Ok(p) = self.proxy.lock() {
            let _ = p.send_event(signal);
        }
    }

    /// The manager is listening at `url`: show the icon.
    pub fn ready(&self, url: String) {
        self.send(Signal::Ready(url))
    }

    /// New status: `tip` is shown when hovering over the icon, `line` is the
    /// first line of the menu.
    pub fn status(&self, tip: String, line: String) {
        self.send(Signal::Status { tip, line })
    }
}

enum Signal {
    Ready(String),
    Status { tip: String, line: String },
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Done(Result<(), String>),
}

/// The icon and the menu entries that change after they are built.
struct Ui {
    tray: TrayIcon,
    status: MenuItem,
    autostart: CheckMenuItem,
}

/// Run `server` on its own thread and the tray on this (the main) thread.
/// Never returns: the process exits when `server` has finished.
pub fn run(mut config: Config, server: impl FnOnce(Link) -> Result<(), String> + Send + 'static) -> ! {
    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<Signal>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        // Menu-bar only: no Dock icon and no application menu.
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let proxy = event_loop.create_proxy();
    let p = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(Signal::Menu(e));
    }));
    let p = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(Signal::Tray(e));
    }));

    let quit = Arc::new(Notify::new());
    let link = Link { quit: quit.clone(), proxy: Mutex::new(proxy.clone()) };
    std::thread::spawn(move || {
        let result = server(link);
        let _ = proxy.send_event(Signal::Done(result));
    });

    let mut ui: Option<Ui> = None;
    // Status that arrived before the icon existed.
    let mut pending: Option<(String, String)> = None;
    let mut last_open: Option<Instant> = None;
    event_loop.run(move |event, target, flow| {
        *flow = ControlFlow::Wait;
        let Event::UserEvent(signal) = event else { return };
        match signal {
            Signal::Ready(url) => {
                config.url = url;
                let scale = target.primary_monitor().map(|m| m.scale_factor()).unwrap_or(1.0);
                match build(scale) {
                    Ok(built) => {
                        if let Some((tip, line)) = pending.take() {
                            show_status(&built, &tip, &line);
                        }
                        ui = Some(built);
                    }
                    Err(e) => crate::util::log(&format!("no tray icon: {e}")),
                }
            }
            Signal::Status { tip, line } => match &ui {
                Some(ui) => show_status(ui, &tip, &line),
                None => pending = Some((tip, line)),
            },
            Signal::Menu(e) => match e.id.0.as_str() {
                OPEN => crate::open_browser(&config.url),
                SETTINGS => crate::open_browser(&format!("{}#/settings", config.url)),
                DATA => crate::open_browser(&config.data_dir.to_string_lossy()),
                LOG => crate::open_browser(&config.data_dir.join("portalcontrol.log").to_string_lossy()),
                AUTOSTART => {
                    if let Some(ui) = &ui {
                        toggle_autostart(&ui.autostart, &config.serve_args);
                    }
                }
                STOP => quit.notify_one(),
                #[cfg(windows)]
                ABOUT => {
                    let status = ui.as_ref().map(|ui| ui.status.text()).unwrap_or_default();
                    about_window::show(&config.url, &status);
                }
                _ => {}
            },
            // Windows: a left click opens the dashboard. A double click
            // arrives as two clicks, so the second one is ignored.
            Signal::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }) if cfg!(windows) => {
                if last_open.is_none_or(|t| t.elapsed() > Duration::from_millis(700)) {
                    last_open = Some(Instant::now());
                    crate::open_browser(&config.url);
                }
            }
            Signal::Tray(_) => {}
            Signal::Done(result) => {
                drop(ui.take()); // removes the icon
                let code = match result {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("error: {e}");
                        1
                    }
                };
                *flow = ControlFlow::ExitWithCode(code);
            }
        }
    })
}

fn show_status(ui: &Ui, tip: &str, line: &str) {
    let _ = ui.tray.set_tooltip(Some(tip));
    ui.status.set_text(line);
}

/// The menu has already flipped the tick; make the system follow, or flip it
/// back when that fails.
fn toggle_autostart(item: &CheckMenuItem, serve_args: &[String]) {
    let wanted = item.is_checked();
    let outcome = if wanted { crate::autostart::install(serve_args) } else { crate::autostart::uninstall() };
    match outcome {
        Ok(_) => crate::util::log(if wanted {
            "start at login turned on from the tray"
        } else {
            "start at login turned off from the tray"
        }),
        Err(e) => {
            crate::util::log(&format!("could not change start at login: {e}"));
            item.set_checked(!wanted);
        }
    }
}

fn build(scale: f64) -> Result<Ui, String> {
    let text = |e: tray_icon::menu::Error| e.to_string();
    // First line: what is running. Not clickable.
    let status = MenuItem::new("Starting\u{2026}", false, None);
    let open = MenuItem::with_id(OPEN, "Open dashboard", true, None);
    let settings = MenuItem::with_id(SETTINGS, "Settings\u{2026}", true, None);
    let at_login = crate::autostart::status().ok().flatten().is_some();
    let autostart = CheckMenuItem::with_id(AUTOSTART, "Start at login", true, at_login, None);
    let data = MenuItem::with_id(DATA, "Open data folder", true, None);
    let log = MenuItem::with_id(LOG, "View log", true, None);
    // Stopping also stops every supervised service, so it sits one level
    // down where a stray click cannot reach it.
    let stop = MenuItem::with_id(STOP, "Stop PortalControl and its services", true, None);
    let quit = Submenu::with_items("Quit", true, &[&stop]).map_err(text)?;
    // About opens a window of its own, not the browser. Windows: the dialog
    // in `about_window`. macOS: the system's standard About panel.
    #[cfg(windows)]
    let about = MenuItem::with_id(ABOUT, "About PortalControl", true, None);
    #[cfg(not(windows))]
    let about = PredefinedMenuItem::about(
        Some("About PortalControl"),
        Some(tray_icon::menu::AboutMetadata {
            name: Some("PortalControl".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            authors: Some(vec![crate::AUTHOR.into()]),
            comments: Some(crate::TAGLINE.into()),
            copyright: Some(format!("Created by {}", crate::AUTHOR)),
            website: Some(crate::HOMEPAGE.into()),
            ..Default::default()
        }),
    );
    let menu = Menu::new();
    menu.append_items(&[
        &status,
        &PredefinedMenuItem::separator(),
        &open,
        &settings,
        &PredefinedMenuItem::separator(),
        &autostart,
        &data,
        &log,
        &PredefinedMenuItem::separator(),
        &about,
        &quit,
    ])
    .map_err(text)?;

    let builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("PortalControl");
    // macOS: the icon is a black shape the system tints for light and dark
    // menu bars, and a click opens the menu as on every Mac app.
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_templated(icon(scale)?).with_menu_on_left_click(true);
    #[cfg(not(target_os = "macos"))]
    let builder = builder.with_icon(icon(scale)?).with_menu_on_left_click(false);
    let tray = builder.build().map_err(|e| e.to_string())?;
    Ok(Ui { tray, status, autostart })
}

/// The About window on Windows: a native dialog with the application icon,
/// the version, who made it and what is running, with clickable links.
#[cfg(windows)]
mod about_window {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Controls::{
        TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOGCONFIG_1, TASKDIALOG_BUTTON,
        TDCBF_CLOSE_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION, TDF_ENABLE_HYPERLINKS, TDF_USE_HICON_MAIN,
        TDN_HYPERLINK_CLICKED,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, IMAGE_ICON, LR_DEFAULTCOLOR, SM_CXICON, SM_CYICON,
    };

    /// Only one About window at a time.
    static OPEN: AtomicBool = AtomicBool::new(false);
    const DASHBOARD_BUTTON: i32 = 100;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A link in the dialog was clicked: `lparam` points at its address.
    unsafe extern "system" fn on_event(_: HWND, msg: i32, _: WPARAM, lparam: LPARAM, _: isize) -> i32 {
        if msg == TDN_HYPERLINK_CLICKED && lparam != 0 {
            let start = lparam as *const u16;
            // SAFETY: the dialog passes a NUL-terminated string that outlives this call.
            let len = (0..2048).take_while(|i| unsafe { *start.add(*i) } != 0).count();
            let url = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(start, len) });
            if url.starts_with("http://") || url.starts_with("https://") {
                crate::open_browser(&url);
            }
        }
        0 // S_OK
    }

    /// `url` is the dashboard address; `status` is the tray's status line.
    /// Returns the thread the window runs on, or `None` when one is already open.
    pub fn show(url: &str, status: &str) -> Option<std::thread::JoinHandle<()>> {
        if OPEN.swap(true, Ordering::SeqCst) {
            return None;
        }
        let link = |address: &str| {
            let shown =
                address.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
            format!("<A HREF=\"{address}\">{shown}</A>")
        };
        let title = wide("About PortalControl");
        let heading = wide(&format!("PortalControl {}", env!("CARGO_PKG_VERSION")));
        let content = wide(&format!(
            "{}\n\nCreated by {}\n{}\n\nDashboard: {}\n{}",
            crate::TAGLINE,
            crate::AUTHOR,
            link(crate::HOMEPAGE),
            link(url),
            status
        ));
        let footer = wide(
            &[
                crate::build_stamp(),
                format!("SQLite {}", rusqlite::version()),
                format!("libgit2 {}", crate::git::library_version()),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("  \u{00b7}  "),
        );
        let button = wide("More details in the dashboard");
        let details = format!("{url}#/about");

        // The dialog runs its own loop until it is closed, so it gets a thread.
        Some(std::thread::spawn(move || {
            let buttons = [TASKDIALOG_BUTTON { nButtonID: DASHBOARD_BUTTON, pszButtonText: button.as_ptr() }];
            // SAFETY: every pointer below refers to a buffer that lives until
            // TaskDialogIndirect returns; the struct is fully initialised.
            let pressed = unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                // Resource 1 is the application icon embedded by build.rs.
                let icon = LoadImageW(
                    module,
                    1 as _,
                    IMAGE_ICON,
                    GetSystemMetrics(SM_CXICON),
                    GetSystemMetrics(SM_CYICON),
                    LR_DEFAULTCOLOR,
                );
                let mut config: TASKDIALOGCONFIG = std::mem::zeroed();
                config.cbSize = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
                config.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION
                    | TDF_ENABLE_HYPERLINKS
                    | if icon.is_null() { 0 } else { TDF_USE_HICON_MAIN };
                config.dwCommonButtons = TDCBF_CLOSE_BUTTON;
                config.pszWindowTitle = title.as_ptr();
                config.Anonymous1 = TASKDIALOGCONFIG_0 { hMainIcon: icon as _ };
                config.pszMainInstruction = heading.as_ptr();
                config.pszContent = content.as_ptr();
                config.cButtons = buttons.len() as u32;
                config.pButtons = buttons.as_ptr();
                config.Anonymous2 = TASKDIALOGCONFIG_1 { pszFooterIcon: std::ptr::null() };
                config.pszFooter = footer.as_ptr();
                config.pfCallback = Some(on_event);
                let mut pressed = 0;
                TaskDialogIndirect(&config, &mut pressed, std::ptr::null_mut(), std::ptr::null_mut());
                pressed
            };
            OPEN.store(false, Ordering::SeqCst);
            if pressed == DASHBOARD_BUTTON {
                crate::open_browser(&details);
            }
        }))
    }
}

/// `portalcontrol about`: show the About window and wait until it is closed.
#[cfg(windows)]
pub fn about_and_wait(url: &str) {
    if let Some(window) = about_window::show(url, "") {
        let _ = window.join();
    }
}

#[cfg(windows)]
fn icon(scale: f64) -> Result<Icon, String> {
    // The picture embedded by build.rs, at the size the taskbar draws it.
    let side = (16.0 * scale).round().clamp(16.0, 64.0) as u32;
    Icon::from_resource(1, Some((side, side)))
        .or_else(|_| Icon::from_rgba(include_bytes!("../assets/tray.rgba").to_vec(), 32, 32))
        .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn icon(_scale: f64) -> Result<Icon, String> {
    Icon::from_rgba(include_bytes!("../assets/tray-mac.rgba").to_vec(), 44, 44).map_err(|e| e.to_string())
}

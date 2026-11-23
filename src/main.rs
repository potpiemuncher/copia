// No console window behind the tray icon on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod icon;
mod providers;

use providers::{Config, Provider};
use serde::Serialize;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, State, WebviewWindow, WindowEvent};

const TRAY_ID: &str = "copia";

#[derive(Serialize, Clone)]
struct Snapshot {
    providers: Vec<Provider>,
    /// Epoch seconds of the last finished refresh.
    updated_at: Option<u64>,
    loading: bool,
    demo: bool,
    /// Fractions left below which a bar turns amber / red (see icon.rs).
    warn_below: f64,
    low_below: f64,
}

impl Snapshot {
    fn new(providers: Vec<Provider>, updated_at: Option<u64>, loading: bool, demo: bool) -> Self {
        Snapshot {
            providers,
            updated_at,
            loading,
            demo,
            warn_below: icon::WARN_BELOW,
            low_below: icon::LOW_BELOW,
        }
    }
}

/// Where the tray icon is, in physical pixels, so the popover can sit by it.
#[derive(Clone, Copy)]
struct Anchor {
    centre_x: f64,
    top: f64,
    bottom: f64,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
    refresh: Mutex<mpsc::Sender<()>>,
    anchor: Mutex<Option<Anchor>>,
    /// The popover's size in logical pixels, as last asked for by `fit`.
    /// Placement works from this rather than the window's current size,
    /// which can lag behind (GTK resizes asynchronously; Windows rescales a
    /// window after it moves to a monitor with another scale factor).
    size: Mutex<(f64, f64)>,
    hidden_at: Mutex<Instant>,
}

/// The starting size in tauri.conf.json, until the page reports its own.
const INITIAL_SIZE: (f64, f64) = (320.0, 250.0);

#[tauri::command]
fn get_snapshot(shared: State<Shared>) -> Snapshot {
    shared.snapshot.lock().unwrap().clone()
}

#[tauri::command]
fn refresh(shared: State<Shared>) {
    request_refresh(&shared);
}

/// A refresh the person asked for (button, shortcut, menu), not the timer.
fn request_refresh(shared: &Shared) {
    providers::ask_keychain_again();
    let _ = shared.refresh.lock().unwrap().send(());
}

/// Escape in the popover.
#[tauri::command]
fn hide(window: WebviewWindow) {
    let _ = window.hide();
}

/// The page tells us how big its content is; size the popover to match.
#[tauri::command]
fn fit(window: WebviewWindow, shared: State<Shared>, width: f64, height: f64) {
    let size = (width.clamp(180.0, 900.0), height.clamp(120.0, 600.0));
    *shared.size.lock().unwrap() = size;
    let _ = window.set_size(tauri::LogicalSize::new(size.0, size.1));
    place(&window, &shared);
}

/// Put the popover by the tray icon (or in a corner when there is no click
/// position), on the monitor that icon is on.
fn place(window: &WebviewWindow, shared: &Shared) {
    let anchor = *shared.anchor.lock().unwrap();
    let (logical_width, logical_height) = *shared.size.lock().unwrap();
    let monitor = anchor
        .and_then(|a| window.monitor_from_point(a.centre_x, a.top).ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else { return };
    let scale = monitor.scale_factor();
    // The size the window will have on that monitor, not the size it has now.
    let (width, height) = (logical_width * scale, logical_height * scale);
    let (left, top) = (monitor.position().x as f64, monitor.position().y as f64);
    let (right, bottom) = (left + monitor.size().width as f64, top + monitor.size().height as f64);
    let margin = 8.0 * scale;

    let (x, y) = match anchor {
        // Tray at the top of the screen (macOS, some Linux): hang below it.
        Some(a) if a.top < top + (bottom - top) / 2.0 => (a.centre_x - width / 2.0, a.bottom + margin / 2.0),
        // Tray at the bottom (Windows): sit above it.
        Some(a) => (a.centre_x - width / 2.0, a.top - height - margin / 2.0),
        // No click position (Linux menus): top-right corner.
        None => (right - width - margin, top + margin * 4.0),
    };
    let x = x.clamp(left + margin, (right - width - margin).max(left + margin));
    let y = y.clamp(top, (bottom - height - margin).max(top));
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

fn show(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else { return };
    place(&window, &app.state::<Shared>());
    let _ = window.show();
    let _ = window.set_focus();
}

fn toggle(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else { return };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    // Clicking the tray while the popover is open first blurs (and hides) it;
    // don't let that same click reopen it.
    if app.state::<Shared>().hidden_at.lock().unwrap().elapsed() < Duration::from_millis(250) {
        return;
    }
    show(app);
}

fn update_tray(app: &AppHandle, providers: &[Provider]) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let shown: Vec<&Provider> = providers.iter().filter(|p| p.configured).collect();

    // One bar per provider: what is left of its tightest quota.
    let levels: Vec<Option<f64>> = shown
        .iter()
        .map(|p| p.quotas.iter().map(|q| 1.0 - q.used).reduce(f64::min))
        .collect();
    let template = cfg!(target_os = "macos");
    let (rgba, width, height) = icon::render(&levels, template);
    let _ = tray.set_icon(Some(Image::new_owned(rgba, width, height)));
    let _ = tray.set_icon_as_template(template);

    let _ = tray.set_tooltip(Some(tooltip(&shown, cfg!(windows))));
}

/// Windows tray tooltips hold 127 UTF-16 units, after which they are cut off.
const WINDOWS_TOOLTIP_UNITS: usize = 127;

/// One line per provider. `compact` (Windows) gives each provider only its
/// tightest quota and no error detail, and lists problems and the providers
/// with the least left first, so that if there are too many to fit, the
/// lines cut off are the healthy ones.
fn tooltip(shown: &[&Provider], compact: bool) -> String {
    // (how urgent: lower first, line)
    let mut lines: Vec<(f64, String)> = shown
        .iter()
        .map(|p| match (&p.error, p.quotas.is_empty()) {
            (Some(_), _) if compact => (-1.0, format!("{}: problem", p.name)),
            (Some(error), _) => (-1.0, format!("{}: {error}", p.name)),
            (None, true) => (f64::INFINITY, format!("{}: no data", p.name)),
            (None, false) if compact => {
                let tightest = p.quotas.iter().max_by(|a, b| a.used.total_cmp(&b.used));
                let q = tightest.expect("quotas is not empty");
                let left = 1.0 - q.used;
                (left, format!("{} {:.0}% left ({})", p.name, left * 100.0, q.label))
            }
            (None, false) => {
                let parts: Vec<String> = p
                    .quotas
                    .iter()
                    .map(|q| format!("{} {:.0}%", q.label, (1.0 - q.used) * 100.0))
                    .collect();
                (0.0, format!("{}: {} left", p.name, parts.join(", ")))
            }
        })
        .collect();
    if compact {
        lines.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    let lines: Vec<String> = lines.into_iter().map(|(_, line)| line).collect();
    let text = if lines.is_empty() {
        "Copia — no providers found".to_string()
    } else {
        lines.join("\n")
    };
    if compact {
        clip_utf16(&text, WINDOWS_TOOLTIP_UNITS)
    } else {
        text
    }
}

/// `text` cut to at most `max` UTF-16 units, ending in "…" when cut.
fn clip_utf16(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 1; // room for the ellipsis
    for c in text.chars() {
        used += c.len_utf16();
        if used > max {
            break;
        }
        out.push(c);
    }
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::Quota;

    fn provider(name: &str, quotas: &[(&str, f64)], error: Option<&str>) -> Provider {
        Provider {
            id: name.to_lowercase(),
            name: name.into(),
            configured: true,
            quotas: quotas
                .iter()
                .map(|&(label, used)| Quota { label: label.into(), used, resets_at: None })
                .collect(),
            error: error.map(String::from),
            experimental: false,
        }
    }

    #[test]
    fn windows_tooltip_drops_healthy_providers_first() {
        let mut all: Vec<Provider> = (1..=9)
            .map(|i| provider(&format!("Custom tool {i}"), &[("day", 0.1)], None))
            .collect();
        all.push(provider("Nearly out", &[("day", 0.97)], None));
        all.push(provider("Broken", &[], Some("network: no route to host")));
        let shown: Vec<&Provider> = all.iter().collect();
        let text = tooltip(&shown, true);
        assert!(text.encode_utf16().count() <= WINDOWS_TOOLTIP_UNITS, "{text}");
        assert!(text.starts_with("Broken: problem\nNearly out 3% left (day)"), "{text}");
        assert!(text.ends_with('…'), "{text}");
    }

    #[test]
    fn windows_tooltip_fits_four_providers() {
        let all = [
            provider("Claude", &[("5h", 0.38), ("week", 0.61), ("opus", 0.2), ("sonnet", 0.3)], None),
            provider("Gemini", &[], Some("sign-in expired — run `gemini` once to renew it")),
            provider("Codex", &[("5h", 0.12), ("week", 0.87)], None),
            provider("OpenCode Go", &[("5h", 0.05), ("week", 0.44), ("month", 0.7)], None),
        ];
        let shown: Vec<&Provider> = all.iter().collect();
        let text = tooltip(&shown, true);
        assert!(text.encode_utf16().count() <= WINDOWS_TOOLTIP_UNITS, "{text}");
        for p in &all {
            assert!(text.contains(&p.name), "{text}");
        }
        assert!(text.contains("Claude 39% left (week)"), "{text}");
        assert!(text.contains("Codex 13% left (week)"), "{text}");
    }

    #[test]
    fn long_text_is_clipped_with_an_ellipsis() {
        let clipped = clip_utf16(&"é".repeat(200), WINDOWS_TOOLTIP_UNITS);
        assert_eq!(clipped.encode_utf16().count(), WINDOWS_TOOLTIP_UNITS);
        assert!(clipped.ends_with('…'));
        assert_eq!(clip_utf16("short", WINDOWS_TOOLTIP_UNITS), "short");
    }
}

fn spawn_worker(app: AppHandle, wake: mpsc::Receiver<()>, demo: bool) {
    std::thread::spawn(move || loop {
        let shared = app.state::<Shared>();
        shared.snapshot.lock().unwrap().loading = true;
        let _ = app.emit("snapshot", ());

        // Re-read the config every round so edits apply without a restart.
        let config = Config::load();
        let providers = if demo { providers::demo() } else { providers::fetch_all(&config) };
        update_tray(&app, &providers);
        *shared.snapshot.lock().unwrap() =
            Snapshot::new(providers, Some(providers::now_secs()), false, demo);
        let _ = app.emit("snapshot", ());

        let minutes = config.refresh_minutes.unwrap_or(5).clamp(1, 240);
        let _ = wake.recv_timeout(Duration::from_secs(minutes * 60));
        while wake.try_recv().is_ok() {}
    });
}

/// Release builds on Windows are GUI programs with no console of their own,
/// so `--print` run from a terminal would print nowhere. Borrow the
/// terminal's console, unless stdout already goes somewhere (a pipe or file).
#[cfg(windows)]
fn attach_console() {
    use std::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    const FILE_TYPE_UNKNOWN: u32 = 0;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetFileType(file: *mut c_void) -> u32;
        fn AttachConsole(process: u32) -> i32;
    }
    // SAFETY: GetStdHandle and AttachConsole take plain integers;
    // GetFileType is only given a handle that is neither null nor invalid.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        // No handle, or one that leads nowhere usable: borrow the console.
        let usable = !out.is_null() && out as isize != -1 && GetFileType(out) != FILE_TYPE_UNKNOWN;
        if !usable {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() {
    let demo = std::env::args().any(|a| a == "--demo") || std::env::var_os("COPIA_DEMO").is_some();
    // `copia --print`: fetch once, print the quotas as JSON, and exit. No tray.
    if std::env::args().any(|a| a == "--print") {
        attach_console();
        let providers = if demo { providers::demo() } else { providers::fetch_all(&Config::load()) };
        println!("{}", serde_json::to_string_pretty(&providers).unwrap_or_default());
        return;
    }
    // Development aid: keep the popover open when it loses focus.
    let pinned = std::env::var_os("COPIA_PIN").is_some();
    let (refresh_tx, refresh_rx) = mpsc::channel();

    tauri::Builder::default()
        .manage(Shared {
            snapshot: Mutex::new(Snapshot::new(Vec::new(), None, true, demo)),
            refresh: Mutex::new(refresh_tx),
            anchor: Mutex::new(None),
            size: Mutex::new(INITIAL_SIZE),
            hidden_at: Mutex::new(Instant::now() - Duration::from_secs(1)),
        })
        .invoke_handler(tauri::generate_handler![get_snapshot, refresh, fit, hide])
        .on_window_event(move |window, event| match event {
            WindowEvent::Focused(false) if !pinned => {
                *window.state::<Shared>().hidden_at.lock().unwrap() = Instant::now();
                let _ = window.hide();
            }
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            // Moving to a monitor with another scale factor resizes the
            // window, and Windows moves it to its own suggested spot after
            // this handler returns. Place it again once that is done: from
            // another thread, the calls queue behind the resize.
            WindowEvent::ScaleFactorChanged { .. } => {
                let app = window.app_handle().clone();
                std::thread::spawn(move || {
                    if let Some(popover) = app.get_webview_window("main") {
                        place(&popover, &app.state::<Shared>());
                    }
                });
            }
            _ => {}
        })
        .setup(move |app| {
            // A tray app: no Dock icon, no app switcher entry.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let menu = Menu::with_items(
                app,
                &[
                    &MenuItem::with_id(app, "show", "Show quotas", true, None::<&str>)?,
                    &MenuItem::with_id(app, "refresh", "Refresh now", true, None::<&str>)?,
                    &MenuItem::with_id(app, "quit", "Quit Copia", true, None::<&str>)?,
                ],
            )?;
            let template = cfg!(target_os = "macos");
            let (rgba, width, height) = icon::render(&[], template);
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(Image::new_owned(rgba, width, height))
                .icon_as_template(template)
                .tooltip("Copia")
                .menu(&menu)
                // Left click opens the bars; the menu is on right click.
                // (Linux trays only offer the menu, hence "Show quotas".)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show(app),
                    "refresh" => request_refresh(&app.state::<Shared>()),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        let position = rect.position.to_physical::<f64>(1.0);
                        let size = rect.size.to_physical::<f64>(1.0);
                        *app.state::<Shared>().anchor.lock().unwrap() = Some(Anchor {
                            centre_x: position.x + size.width / 2.0,
                            top: position.y,
                            bottom: position.y + size.height,
                        });
                        toggle(app);
                    }
                })
                .build(app)?;

            spawn_worker(app.handle().clone(), refresh_rx, demo);
            if pinned {
                show(app.handle());
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Copia");
}

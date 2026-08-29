//! The tray icon and its menu.
//!
//! # Why the settings pane is a separate process
//!
//! winit permits exactly one event loop per process, and the tray already owns
//! one. Rather than thread an egui window through the tray's loop -- which ties
//! the two together and makes a settings crash take the tray with it -- the
//! menu re-executes this binary with `--settings`.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use crate::daemon;
use crate::status::Status;

/// How often the menu is rebuilt from the daemon.
///
/// The tray has no push channel -- the daemon's socket is request/response --
/// so this is a poll. Two seconds is well under the time it takes to notice a
/// stale menu, and the query is a few bytes over a Unix socket.
const REFRESH: Duration = Duration::from_secs(2);

const ID_SETTINGS: &str = "settings";
const ID_START: &str = "start";
const ID_STOP: &str = "stop";
const ID_RESTART: &str = "restart";
const ID_QUIT: &str = "quit";

/// Decode the embedded PNG into the RGBA the tray wants.
fn icon() -> Result<tray_icon::Icon> {
    const PNG: &[u8] = include_bytes!("../../../assets/tray.png");
    let decoder = png::Decoder::new(PNG);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    tray_icon::Icon::from_rgba(buf, info.width, info.height)
        .map_err(|e| anyhow!("building tray icon: {e}"))
}

fn build_menu(status: &Status) -> Menu {
    let menu = Menu::new();

    if !status.daemon_running {
        // Not installed and not started are different problems with different
        // fixes. Saying "not running" for both sends the user looking in the
        // wrong place.
        if status.daemon_installed {
            let _ = menu.append(&MenuItem::new("scurry is not running", false, None));
        } else {
            let _ = menu.append(&MenuItem::new("scurry is not installed", false, None));
            let _ = menu.append(&MenuItem::new(daemon::install_hint(), false, None));
        }
    } else {
        let where_ = if status.focus == 0 {
            "this machine".to_string()
        } else {
            status.screen_name(status.focus)
        };
        let _ = menu.append(&MenuItem::new(format!("Pointer on {where_}"), false, None));

        let connected = status.slots.iter().filter(|s| s.connected).count();
        let _ = menu.append(&MenuItem::new(
            match connected {
                0 => "No machines connected".to_string(),
                1 => "1 machine connected".to_string(),
                n => format!("{n} machines connected"),
            },
            false,
            None,
        ));

        if !status.screens.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
            for screen in &status.screens {
                // Node 0 is this machine; it has no connection slot.
                let mark = if screen.node == 0 {
                    "  (this machine)"
                } else if status
                    .slots
                    .iter()
                    .any(|s| s.slot + 1 == screen.node && s.connected)
                {
                    "  connected"
                } else {
                    "  offline"
                };
                let _ = menu.append(&MenuItem::new(
                    format!("{} — {}x{}{}", screen.name, screen.width, screen.height, mark),
                    false,
                    None,
                ));
            }
        }
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(
        MenuId::new(ID_SETTINGS),
        "Settings…",
        status.daemon_running,
        None,
    ));

    if daemon::SUPPORTED {
        let _ = menu.append(&PredefinedMenuItem::separator());
        // Disabled until the installer has run: pressing it would only report
        // a missing unit, which the menu already says above.
        let _ = menu.append(&MenuItem::with_id(
            MenuId::new(ID_START),
            "Start",
            status.daemon_installed && !status.daemon_running,
            None,
        ));
        let _ = menu.append(&MenuItem::with_id(
            MenuId::new(ID_STOP),
            "Stop",
            status.daemon_running,
            None,
        ));
        let _ = menu.append(&MenuItem::with_id(
            MenuId::new(ID_RESTART),
            "Restart",
            status.daemon_running,
            None,
        ));
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id(MenuId::new(ID_QUIT), "Quit", true, None));
    menu
}

struct App {
    tray: Option<TrayIcon>,
    next_refresh: Instant,
}

impl App {
    fn refresh(&mut self) {
        let status = Status::fetch();
        let menu = build_menu(&status);
        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(menu)));
            let _ = tray.set_tooltip(Some(if !status.daemon_installed {
                "scurry — not installed".to_string()
            } else if status.daemon_running {
                format!("scurry — pointer on {}", if status.focus == 0 {
                    "this machine".to_string()
                } else {
                    status.screen_name(status.focus)
                })
            } else {
                "scurry — not running".to_string()
            }));
        }
        self.next_refresh = Instant::now() + REFRESH;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // The tray must be created after the event loop is running: on macOS
        // NSStatusItem needs an initialised NSApplication, and building it
        // earlier yields an icon that never appears.
        if self.tray.is_none() {
            let status = Status::fetch();
            match icon() {
                Ok(ic) => {
                    match TrayIconBuilder::new()
                        .with_menu(Box::new(build_menu(&status)))
                        .with_tooltip("scurry")
                        .with_icon(ic)
                        .build()
                    {
                        Ok(t) => self.tray = Some(t),
                        Err(e) => eprintln!("could not create tray icon: {e}"),
                    }
                }
                Err(e) => eprintln!("could not load tray icon: {e}"),
            }
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.as_ref() {
                ID_SETTINGS => spawn_settings(),
                ID_START => report("start", daemon::start()),
                ID_STOP => report("stop", daemon::stop()),
                ID_RESTART => report("restart", daemon::restart()),
                ID_QUIT => {
                    event_loop.exit();
                    return;
                }
                _ => {}
            }
            // Service actions change what the menu should say; do not wait for
            // the next poll to reflect it.
            self.refresh();
        }

        if Instant::now() >= self.next_refresh {
            self.refresh();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_refresh));
    }
}

fn report(action: &str, result: Result<()>) {
    if let Err(e) = result {
        eprintln!("could not {action} the daemon: {e}");
    }
}

/// Re-exec ourselves for the settings window. See the module docs for why this
/// is a process rather than a window in this one.
fn spawn_settings() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot locate own binary to open settings: {e}");
            return;
        }
    };
    if let Err(e) = std::process::Command::new(exe).arg("--settings").spawn() {
        eprintln!("could not open settings: {e}");
    }
}

pub fn run() -> Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App { tray: None, next_refresh: Instant::now() };
    event_loop.run_app(&mut app)?;
    Ok(())
}

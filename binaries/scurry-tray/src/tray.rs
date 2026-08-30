//! The tray icon, and the app itself.
//!
//! This process *is* scurry: it owns the dongle's serial port, captures input,
//! and shows the menu. There is no daemon to install and no service manager
//! involved, because winit's event loop is a CFRunLoop and the input tap can be
//! installed straight into it. Dragging the app to Applications is the whole
//! installation.
//!
//! It also serves the control socket, so the settings window -- a separate
//! process, since winit permits one event loop per process -- can reach the
//! dongle without opening the port a second time.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use scurry_ctl::ipc::DaemonState;
use scurry_ctl::transport::Dongle;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use crate::login;
use crate::status::{spawn_poller, Snapshot};

/// How often the menu is rebuilt, and how often a missing dongle is retried.
const REFRESH: Duration = Duration::from_secs(2);

const ID_SETTINGS: &str = "settings";
const ID_LOGIN: &str = "login";
const ID_QUIT: &str = "quit";

fn icon() -> Result<tray_icon::Icon> {
    const PNG: &[u8] = include_bytes!("../../../assets/tray.png");
    let mut reader = png::Decoder::new(PNG).read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    tray_icon::Icon::from_rgba(buf, info.width, info.height)
        .map_err(|e| anyhow!("building tray icon: {e}"))
}

struct App {
    tray: Option<TrayIcon>,
    /// The dongle link, once one has been found. Absent means unplugged, which
    /// is an ordinary state rather than an error: the app waits for it.
    link: Option<Arc<Mutex<Dongle>>>,
    state: Arc<DaemonState>,
    /// Kept alive because dropping the tap stops input capture.
    #[cfg(target_os = "macos")]
    _tap: Option<scurry_ctl::capture::CaptureHandle>,
    socket_started: bool,
    /// Most recent answer from the dongle. Written by a background poller so
    /// the menu never blocks on serial I/O.
    snapshot: Arc<Mutex<Snapshot>>,
    /// What the menu was last built from.
    ///
    /// Replacing a tray menu dismisses it if the user has it open, so the poll
    /// must not rebuild unconditionally -- doing that made the menu close the
    /// moment it was opened, roughly every two seconds. Rebuild only when
    /// something a person would see has actually changed.
    rendered: Option<(Snapshot, bool)>,
    next_refresh: Instant,
}

impl App {
    fn new() -> Self {
        Self {
            tray: None,
            link: None,
            state: Arc::new(DaemonState::default()),
            #[cfg(target_os = "macos")]
            _tap: None,
            socket_started: false,
            snapshot: Arc::new(Mutex::new(Snapshot::NoDongle)),
            rendered: None,
            next_refresh: Instant::now(),
        }
    }

    /// Try to attach to a dongle, and start capture once one is there.
    fn try_attach(&mut self) {
        // Already attached, but capture never started -- retry it, so granting
        // Accessibility takes effect on the next poll rather than needing the
        // app to be restarted.
        #[cfg(target_os = "macos")]
        if self.link.is_some() && self._tap.is_none() {
            if let Some(link) = &self.link {
                if let Ok(tap) =
                    scurry_ctl::capture::install(Arc::clone(link), Arc::clone(&self.state))
                {
                    eprintln!("capture started");
                    self._tap = Some(tap);
                }
            }
        }
        if self.link.is_some() {
            return;
        }
        let Ok(path) = Dongle::autodetect() else { return };
        let Ok(dongle) = Dongle::open(&path) else { return };
        eprintln!("dongle: {path}");
        let link = Arc::new(Mutex::new(dongle));

        #[cfg(target_os = "macos")]
        {
            match scurry_ctl::capture::install(Arc::clone(&link), Arc::clone(&self.state)) {
                Ok(tap) => self._tap = Some(tap),
                Err(e) => {
                    // Almost always missing Accessibility permission. Do NOT
                    // give up here: returning early left the app holding the
                    // serial port while serving nothing -- capture dead, no
                    // control socket, and the CLI locked out of a port it could
                    // otherwise have used. Carry on without capture; settings
                    // and status still work, and the next poll retries.
                    eprintln!("capture unavailable, continuing without it: {e}");
                }
            }
        }

        // The settings window is a separate process and cannot open the port,
        // which this one holds. Serve it here instead.
        if !self.socket_started {
            let socket_link = Arc::clone(&link);
            let socket_state = Arc::clone(&self.state);
            std::thread::spawn(move || {
                if let Err(e) = scurry_ctl::ipc::serve(socket_link, socket_state) {
                    eprintln!("control socket: {e}");
                }
            });
            self.socket_started = true;
        }

        spawn_poller(Arc::clone(&link), Arc::clone(&self.state), Arc::clone(&self.snapshot));
        self.link = Some(link);
    }

    fn build_menu(&self, snap: &Snapshot) -> Menu {
        let menu = Menu::new();

        match snap {
            Snapshot::NoDongle => {
                let _ = menu.append(&MenuItem::new("No dongle connected", false, None));
            }
            #[cfg(target_os = "macos")]
            Snapshot::NeedsPermission => {
                let _ = menu.append(&MenuItem::new("Waiting for Accessibility access", false, None));
            }
            Snapshot::Ready { focus, screens, slots } => {
                let here = *focus == 0;
                let name = screens
                    .iter()
                    .find(|s| s.node == *focus)
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| format!("node {focus}"));
                let _ = menu.append(&MenuItem::new(
                    if here { "Pointer here".to_string() } else { format!("Pointer on {name}") },
                    false,
                    None,
                ));

                if !screens.is_empty() {
                    let _ = menu.append(&PredefinedMenuItem::separator());
                    for screen in screens {
                        let mark = if screen.node == 0 {
                            "this machine"
                        } else if slots.iter().any(|s| s.slot + 1 == screen.node && s.connected) {
                            "connected"
                        } else {
                            "offline"
                        };
                        let _ = menu.append(&MenuItem::new(
                            format!("{}  ·  {}×{}  ·  {mark}", screen.name, screen.width, screen.height),
                            false,
                            None,
                        ));
                    }
                }
            }
        }

        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(
            MenuId::new(ID_SETTINGS),
            "Settings…",
            matches!(snap, Snapshot::Ready { .. }),
            None,
        ));
        let _ = menu.append(&CheckMenuItem::with_id(
            MenuId::new(ID_LOGIN),
            "Open at Login",
            true,
            login::enabled(),
            None,
        ));
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&MenuItem::with_id(MenuId::new(ID_QUIT), "Quit scurry", true, None));
        menu
    }

    fn refresh(&mut self) {
        self.try_attach();

        let snap = if self.link.is_none() {
            Snapshot::NoDongle
        } else {
            #[cfg(target_os = "macos")]
            if !scurry_ctl::capture::accessibility_trusted(false) {
                Snapshot::NeedsPermission
            } else {
                self.snapshot.lock().map(|s| s.clone()).unwrap_or(Snapshot::NoDongle)
            }
            #[cfg(not(target_os = "macos"))]
            {
                self.snapshot.lock().map(|s| s.clone()).unwrap_or(Snapshot::NoDongle)
            }
        };

        // login::enabled() reads the filesystem and is part of what the menu
        // shows, so it belongs in the comparison.
        let state = (snap, login::enabled());
        if self.rendered.as_ref() != Some(&state) {
            let menu = self.build_menu(&state.0);
            if let Some(tray) = &self.tray {
                tray.set_menu(Some(Box::new(menu)));
                let _ = tray.set_tooltip(Some(state.0.tooltip()));
            }
            self.rendered = Some(state);
        }
        self.next_refresh = Instant::now() + REFRESH;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        // Ask for Accessibility with the system's own dialog, which offers to
        // open the right settings pane. Printing instructions instead would put
        // the burden on the user to find a switch they have never seen.
        #[cfg(target_os = "macos")]
        if !scurry_ctl::capture::accessibility_trusted(true) {
            eprintln!("waiting for Accessibility access");
        }

        // The tray must be built after the loop is running: on macOS
        // NSStatusItem needs an initialised NSApplication, and an icon created
        // earlier never appears.
        match icon() {
            Ok(ic) => {
                let builder = TrayIconBuilder::new().with_tooltip("scurry").with_icon(ic);
                // A template image: macOS reads only the alpha channel and
                // tints the glyph to match the menu bar, so it follows light
                // and dark mode without shipping two icons. The asset is
                // deliberately background-free -- a plate would be filled in
                // solid and look nothing like the icons beside it.
                #[cfg(target_os = "macos")]
                let builder = builder.with_icon_as_template(true);
                match builder.build() {
                    Ok(t) => self.tray = Some(t),
                    Err(e) => eprintln!("could not create tray icon: {e}"),
                }
            }
            Err(e) => eprintln!("could not load tray icon: {e}"),
        }
        self.refresh();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.as_ref() {
                ID_SETTINGS => spawn_settings(),
                ID_LOGIN => {
                    if let Err(e) = login::toggle() {
                        eprintln!("could not change the login item: {e}");
                    }
                }
                ID_QUIT => {
                    event_loop.exit();
                    return;
                }
                _ => {}
            }
            // A menu action changes what the menu should say, and the user has
            // just dismissed it by clicking, so rebuilding now is safe.
            self.rendered = None;
            self.refresh();
        }

        if Instant::now() >= self.next_refresh {
            self.refresh();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_refresh));
    }
}

/// The settings window runs as its own process: winit permits one event loop
/// per process and this one is the tray's.
fn spawn_settings() {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("cannot locate own binary to open settings");
        return;
    };
    if let Err(e) = std::process::Command::new(exe).arg("--settings").spawn() {
        eprintln!("could not open settings: {e}");
    }
}

pub fn run() -> Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut App::new())?;
    Ok(())
}

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

/// Ceiling on the wait between capture retries once one has failed for a reason
/// other than missing permission.
#[cfg(target_os = "macos")]
const MAX_CAPTURE_BACKOFF: Duration = Duration::from_secs(60);

const ID_SETTINGS: &str = "settings";
const ID_LOGIN: &str = "login";
const ID_QUIT: &str = "quit";

/// What a poll should do about the dongle link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attach {
    /// A working link is held. Leave it alone.
    Hold,
    /// Nothing is attached. Look for a dongle.
    Open,
    /// A link is held but the device behind it is gone. Reopen the port into
    /// the handle every consumer already holds.
    Reopen,
}

/// Decide from the only two facts that matter, so the rule can be tested
/// without a dongle to unplug.
///
/// The case this exists for: after an unplug the app held a live `Arc` around a
/// dead fd, so "is something attached?" answered yes forever and the port was
/// never reopened. Attachment alone is not health.
fn decide_attach(attached: bool, link_failed: bool) -> Attach {
    match (attached, link_failed) {
        (false, _) => Attach::Open,
        (true, true) => Attach::Reopen,
        (true, false) => Attach::Hold,
    }
}

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
    /// Set when the reader reports the device gone, and cleared only once the
    /// port has actually been reopened. Sticky, because the user may take a
    /// while to plug the dongle back in and there is nothing to do until they
    /// do.
    link_failed: bool,
    /// When capture may next be attempted. Retrying on every poll meant
    /// `CGEventTapCreate` was called every two seconds for as long as the user
    /// took to grant Accessibility.
    #[cfg(target_os = "macos")]
    next_capture_try: Instant,
    #[cfg(target_os = "macos")]
    capture_backoff: Duration,
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
            link_failed: false,
            #[cfg(target_os = "macos")]
            next_capture_try: Instant::now(),
            #[cfg(target_os = "macos")]
            capture_backoff: REFRESH,
        }
    }

    /// Start capture, if there is a dongle to capture for and it is not already
    /// running.
    ///
    /// Capture is not required for the app to be useful -- settings and status
    /// work without it -- so a failure here is never fatal. Giving up on it
    /// used to leave the app holding the serial port while serving nothing:
    /// capture dead, no control socket, and the CLI locked out of a port it
    /// could otherwise have used.
    #[cfg(target_os = "macos")]
    fn try_start_capture(&mut self) {
        let Some(link) = &self.link else { return };
        if self._tap.is_some() || Instant::now() < self.next_capture_try {
            return;
        }
        // Ask macOS first. Until permission is granted `CGEventTapCreate` can
        // only fail, and calling it every two seconds for however long the user
        // takes to find the switch is pure noise; the trust check answers the
        // same question without touching the event system.
        if !scurry_ctl::capture::accessibility_trusted(false) {
            self.next_capture_try = Instant::now() + REFRESH;
            return;
        }
        match scurry_ctl::capture::install(Arc::clone(link), Arc::clone(&self.state)) {
            Ok(tap) => {
                eprintln!("capture started");
                self._tap = Some(tap);
                self.capture_backoff = REFRESH;
            }
            Err(e) => {
                // Trusted and still failing means something other than
                // permission, which is unlikely to clear up within two seconds.
                // Back off, so a persistent failure does not repeat the same
                // complaint forever.
                eprintln!(
                    "capture unavailable, retrying in {:?}: {e}",
                    self.capture_backoff
                );
                self.next_capture_try = Instant::now() + self.capture_backoff;
                self.capture_backoff = (self.capture_backoff * 2).min(MAX_CAPTURE_BACKOFF);
            }
        }
    }

    /// Try to attach to a dongle, and start capture once one is there.
    fn try_attach(&mut self) {
        // Latched, not level-triggered, and taken on every poll so a failure
        // reported while nothing was attached cannot linger and trigger a
        // pointless reopen later.
        self.link_failed |= scurry_ctl::capture::take_link_failure();

        match decide_attach(self.link.is_some(), self.link_failed) {
            Attach::Hold => {}
            Attach::Reopen => {
                self.reopen();
                return;
            }
            Attach::Open => {
                self.open();
                return;
            }
        }

        // Attached and healthy. Capture may still be waiting on Accessibility,
        // so keep trying: granting it takes effect on a later poll rather than
        // needing the app to be restarted.
        #[cfg(target_os = "macos")]
        self.try_start_capture();
    }

    /// Reopen the serial port into the handle everything already holds.
    ///
    /// The control socket thread, the status poller and the event tap's
    /// callback each captured a clone of the `Arc` when they started, and none
    /// of them can be handed a replacement. So a replug swaps the `Dongle`
    /// *inside* the mutex rather than the `Arc` around it, and every one of them
    /// picks the new device up on its next operation -- no second socket server,
    /// no second poller, no second reader, and no tap to tear down and rebuild.
    ///
    /// Quiet when there is nothing to open: a dongle that is still unplugged is
    /// an ordinary state, and this runs every two seconds.
    fn reopen(&mut self) {
        let Some(link) = &self.link else { return };
        let Ok(path) = Dongle::autodetect() else {
            return;
        };
        let Ok(fresh) = Dongle::open(&path) else {
            return;
        };
        let Ok(mut guard) = link.lock() else {
            eprintln!("dongle handle poisoned; cannot reattach");
            return;
        };
        // Assigning drops the dead handle, closing its fd.
        *guard = fresh;
        drop(guard);
        self.link_failed = false;
        eprintln!("dongle: {path} (reattached)");

        // The old reader exited when the device vanished, which is what set the
        // failure flag in the first place. Start one on the new device.
        let link = Arc::clone(link);
        if let Err(e) = scurry_ctl::capture::watch(&link, &self.state) {
            eprintln!("could not read from the reattached dongle: {e}");
        }
    }

    /// Open a dongle for the first time and start everything that hangs off it.
    fn open(&mut self) {
        let Ok(path) = Dongle::autodetect() else {
            return;
        };
        let Ok(dongle) = Dongle::open(&path) else {
            return;
        };
        eprintln!("dongle: {path}");
        let link = Arc::new(Mutex::new(dongle));

        // Started here rather than left to `capture::install`, because the
        // control socket and the status poller both need their replies handed
        // back by this thread. Deferring it to capture would leave the settings
        // window and the CLI timing out for as long as macOS withheld
        // Accessibility.
        if let Err(e) = scurry_ctl::capture::watch(&link, &self.state) {
            eprintln!("could not read from the dongle: {e}");
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

        spawn_poller(
            Arc::clone(&link),
            Arc::clone(&self.state),
            Arc::clone(&self.snapshot),
        );
        self.link = Some(link);

        // Now that there is a link to capture for, and without waiting for the
        // next poll.
        #[cfg(target_os = "macos")]
        self.try_start_capture();
    }

    fn build_menu(&self, snap: &Snapshot) -> Menu {
        let menu = Menu::new();

        match snap {
            Snapshot::NoDongle => {
                let _ = menu.append(&MenuItem::new("No dongle connected", false, None));
            }
            #[cfg(target_os = "macos")]
            Snapshot::NeedsPermission => {
                let _ = menu.append(&MenuItem::new(
                    "Waiting for Accessibility access",
                    false,
                    None,
                ));
            }
            Snapshot::Ready {
                focus,
                screens,
                slots,
            } => {
                let here = *focus == 0;
                let name = screens
                    .iter()
                    .find(|s| s.node == *focus)
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| format!("node {focus}"));
                let _ = menu.append(&MenuItem::new(
                    if here {
                        "Pointer here".to_string()
                    } else {
                        format!("Pointer on {name}")
                    },
                    false,
                    None,
                ));

                if !screens.is_empty() {
                    let _ = menu.append(&PredefinedMenuItem::separator());
                    for screen in screens {
                        let mark = if screen.node == 0 {
                            "this machine"
                        } else if slots
                            .iter()
                            .any(|s| s.slot + 1 == screen.node && s.connected)
                        {
                            "connected"
                        } else {
                            "offline"
                        };
                        let _ = menu.append(&MenuItem::new(
                            format!(
                                "{}  ·  {}×{}  ·  {mark}",
                                screen.name, screen.width, screen.height
                            ),
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
        let _ = menu.append(&MenuItem::with_id(
            MenuId::new(ID_QUIT),
            "Quit scurry",
            true,
            None,
        ));
        menu
    }

    fn refresh(&mut self) {
        self.try_attach();

        // A dead link reads as no dongle. The poller cannot tell the difference
        // on its own: its requests to a vanished device simply time out, and it
        // would go on publishing a `Ready` with an empty screen list, so the
        // menu would claim the pointer was here on a machine that is no longer
        // connected to anything.
        let snap = if self.link.is_none() || self.link_failed {
            Snapshot::NoDongle
        } else {
            #[cfg(target_os = "macos")]
            if !scurry_ctl::capture::accessibility_trusted(false) {
                Snapshot::NeedsPermission
            } else {
                self.snapshot
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or(Snapshot::NoDongle)
            }
            #[cfg(not(target_os = "macos"))]
            {
                self.snapshot
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or(Snapshot::NoDongle)
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
        // Build the menu up front rather than waiting for the first refresh, so
        // the icon is never briefly present with nothing behind it.
        let initial = Snapshot::NoDongle;
        match icon() {
            Ok(ic) => {
                let builder = TrayIconBuilder::new()
                    .with_tooltip("scurry")
                    .with_menu(Box::new(self.build_menu(&initial)))
                    .with_icon(ic);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_attached_looks_for_a_dongle() {
        assert_eq!(decide_attach(false, false), Attach::Open);
    }

    #[test]
    fn a_healthy_link_is_left_alone() {
        // Reopening a working port on every poll would drop the fd the control
        // socket and the tap are using, twice a second.
        assert_eq!(decide_attach(true, false), Attach::Hold);
    }

    #[test]
    fn a_dead_link_is_reopened_rather_than_held() {
        // The bug: unplug and replug left a live Arc around a dead fd, the
        // "already attached" check passed forever, and the app never held a
        // descriptor on the device again.
        assert_eq!(decide_attach(true, true), Attach::Reopen);
    }

    #[test]
    fn a_failure_without_a_link_still_opens_from_scratch() {
        // A failure latched from a link that was never replaced must not stop
        // the ordinary first-attach path from running.
        assert_eq!(decide_attach(false, true), Attach::Open);
    }
}

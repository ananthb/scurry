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

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use scurry_ctl::ipc::{ack_message, DaemonState};
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

/// How long a wireless scan runs before giving up and letting the next poll
/// start another.
const SCAN_FOR: Duration = Duration::from_secs(6);

/// How long to wait between wireless scans when none has succeeded. Scanning is
/// not free -- it keeps the radio listening -- and a dongle that is simply
/// switched off should not cost a continuous scan all day.
const SCAN_EVERY: Duration = Duration::from_secs(20);

const ID_SETTINGS: &str = "settings";
const ID_PAIR: &str = "pair";
const ID_LOGIN: &str = "login";
const ID_QUIT: &str = "quit";

/// What a poll should do about the dongle link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attach {
    /// A working link is held. Leave it alone.
    Hold,
    /// Nothing is attached. Look for a dongle.
    Open,
    /// A link is held but the device behind it is gone, or a better one has
    /// turned up. Reopen into the handle every consumer already holds.
    Reopen,
}

/// Decide from the only facts that matter, so the rule can be tested without a
/// dongle to unplug.
///
/// The case this exists for: after an unplug the app held a live `Arc` around a
/// dead fd, so "is something attached?" answered yes forever and the port was
/// never reopened. Attachment alone is not health.
///
/// `cable_available` is separate from health because a working wireless link is
/// not a reason to ignore a cable that has just been plugged in. The cable is
/// fifty times faster, so a healthy link is not necessarily the right one.
fn decide_attach(attached: bool, link_failed: bool, cable_available: bool) -> Attach {
    match (attached, link_failed, cable_available) {
        (false, _, _) => Attach::Open,
        (true, true, _) => Attach::Reopen,
        (true, false, true) => Attach::Reopen,
        (true, false, false) => Attach::Hold,
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
    rendered: Option<(Snapshot, bool, bool)>,
    next_refresh: Instant,
    /// True while the held link is over the air rather than the cable. Cached
    /// rather than read from the link, so building the menu never waits behind
    /// a pointer report holding the same mutex.
    wireless: bool,
    /// A wireless scan in flight. Scanning takes seconds and the menu is built
    /// on this thread, so it cannot happen inline -- a six-second freeze of the
    /// menu bar every poll is worse than not finding the dongle.
    searching: Option<mpsc::Receiver<Option<Dongle>>>,
    /// When the next wireless scan may start.
    next_scan: Instant,
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
            wireless: false,
            searching: None,
            next_scan: Instant::now(),
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

        // Only interesting while on the radio: on the cable already, or with
        // nothing attached, the answer changes nothing and probing the serial
        // ports every two seconds would be pure noise.
        let cable_available = self.wireless && Dongle::autodetect().is_ok();

        match decide_attach(self.link.is_some(), self.link_failed, cable_available) {
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

    /// Produce a link, preferring the cable.
    ///
    /// The cable first because it opens in microseconds and is two orders of
    /// magnitude faster once open, so a dongle sitting on a desk with a cable in
    /// it behaves exactly as it always did. Wireless is the fallback for when
    /// the dongle is somewhere else entirely.
    ///
    /// Returns `None` while a wireless scan is still running. That is not a
    /// failure -- the next poll asks again.
    fn find_link(&mut self) -> Option<Dongle> {
        if let Ok(path) = Dongle::autodetect() {
            match Dongle::open(&path) {
                Ok(d) => {
                    self.searching = None;
                    return Some(d);
                }
                // Present but unopenable is usually another process holding it.
                // Not worth a wireless scan; the next poll will retry.
                Err(e) => {
                    eprintln!("found {path} but could not open it: {e}");
                    return None;
                }
            }
        }

        if let Some(rx) = &self.searching {
            return match rx.try_recv() {
                Ok(found) => {
                    self.searching = None;
                    if found.is_none() {
                        self.next_scan = Instant::now() + SCAN_EVERY;
                    }
                    found
                }
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.searching = None;
                    self.next_scan = Instant::now() + SCAN_EVERY;
                    None
                }
            };
        }

        if Instant::now() < self.next_scan {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Dongle::open_wireless(SCAN_FOR).ok());
        });
        self.searching = Some(rx);
        None
    }

    /// Reopen the link into the handle everything already holds.
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
        if self.link.is_none() {
            return;
        }
        let Some(fresh) = self.find_link() else { return };

        // The reader holds its own clone, so swapping what is inside the mutex
        // would not reach it: it would go on reading the link being replaced.
        // A reader whose device vanished has already exited and this is a
        // no-op; one being replaced deliberately has not.
        scurry_ctl::capture::stop_reader();

        let described = fresh.describe();
        let wireless = fresh.is_wireless();
        let Some(link) = &self.link else { return };
        let Ok(mut guard) = link.lock() else {
            eprintln!("dongle handle poisoned; cannot reattach");
            return;
        };
        // Assigning drops the dead handle, closing its fd.
        *guard = fresh;
        drop(guard);
        self.link_failed = false;
        self.wireless = wireless;
        eprintln!("dongle: {described} (reattached)");

        // The old reader exited when the device vanished, which is what set the
        // failure flag in the first place. Start one on the new device.
        let link = Arc::clone(link);
        if let Err(e) = scurry_ctl::capture::watch(&link, &self.state) {
            eprintln!("could not read from the reattached dongle: {e}");
        }
    }

    /// Open a dongle for the first time and start everything that hangs off it.
    fn open(&mut self) {
        let Some(dongle) = self.find_link() else { return };
        eprintln!("dongle: {}", dongle.describe());
        self.wireless = dongle.is_wireless();
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

    /// Ask the dongle to accept a wireless controller for the next minute.
    ///
    /// On a worker thread: the request waits on the dongle for up to three
    /// seconds, and this is called from the event loop that draws the menu.
    fn open_pairing_window(&self) {
        let Some(link) = &self.link else { return };
        let link = Arc::clone(link);
        let state = Arc::clone(&self.state);
        std::thread::spawn(move || {
            let payload = [scurry_proto::wireless_op::PAIR, 60];
            match state.request(
                &link,
                scurry_proto::kind::SET_WIRELESS,
                &payload,
                scurry_proto::kind::ACK,
                Duration::from_secs(3),
            ) {
                Ok((_, p))
                    if p.first().copied() == Some(scurry_proto::ack::OK) =>
                {
                    eprintln!(
                        "pairing window open for 60s -- start a controller with --wireless now"
                    );
                }
                Ok((_, p)) => {
                    eprintln!("the dongle refused to open the window: {}", ack_message(&p))
                }
                Err(e) => eprintln!("could not open the pairing window: {e}"),
            }
        });
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

                let _ = menu.append(&MenuItem::new(
                    if self.wireless {
                        "Connected over Bluetooth"
                    } else {
                        "Connected by cable"
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
        // Only over the cable. The dongle refuses to change pairing over the
        // wireless link, because authorising a device that can type on every
        // machine here should take physical access -- so offering it while
        // connected over the air would be offering something that cannot work.
        let attached = matches!(snap, Snapshot::Ready { .. });
        let _ = menu.append(&MenuItem::with_id(
            MenuId::new(ID_PAIR),
            if self.wireless {
                "Pair a wireless controller (needs the cable)"
            } else {
                "Pair a wireless controller…"
            },
            attached && !self.wireless,
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
        let state = (snap, login::enabled(), self.wireless);
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
                ID_PAIR => self.open_pairing_window(),
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
    fn a_cable_appearing_beats_a_healthy_wireless_link() {
        // The whole point of the fourth argument: 0.3ms is available and the
        // app is using 17ms. Health is not the same as being the right link.
        assert!(matches!(decide_attach(true, false, true), Attach::Reopen));
    }

    #[test]
    fn a_healthy_link_with_no_cable_is_left_alone() {
        assert!(matches!(decide_attach(true, false, false), Attach::Hold));
    }

    #[test]
    fn nothing_attached_looks_for_a_dongle() {
        assert_eq!(decide_attach(false, false, false), Attach::Open);
    }

    #[test]
    fn a_healthy_link_is_left_alone() {
        // Reopening a working port on every poll would drop the fd the control
        // socket and the tap are using, twice a second.
        assert_eq!(decide_attach(true, false, false), Attach::Hold);
    }

    #[test]
    fn a_dead_link_is_reopened_rather_than_held() {
        // The bug: unplug and replug left a live Arc around a dead fd, the
        // "already attached" check passed forever, and the app never held a
        // descriptor on the device again.
        assert_eq!(decide_attach(true, true, false), Attach::Reopen);
    }

    #[test]
    fn a_failure_without_a_link_still_opens_from_scratch() {
        // A failure latched from a link that was never replaced must not stop
        // the ordinary first-attach path from running.
        assert_eq!(decide_attach(false, true, false), Attach::Open);
    }
}

//! The firmware section of the settings pane.
//!
//! Updates arrive from the project's own releases by default, because the
//! alternative is asking somebody to find a `.bin` on the internet and trust
//! it, which is a worse habit than any convenience is worth. A local file is
//! still accepted, for the case this is really for: you built the firmware and
//! want it on the dongle.
//!
//! Everything slow happens on a worker thread. Downloading an image takes
//! seconds and pushing it takes tens of them, and eframe redraws on the thread
//! that would otherwise be doing it -- the window would be frozen for the whole
//! update, which is exactly the period somebody most wants to see progress.

use std::sync::{Arc, Mutex};

use eframe::egui;
use scurry_ctl::ipc::Client;
use scurry_ctl::update::{self, Release};
use scurry_proto::FirmwareInfo;

/// A link to the dongle through the running app, which owns the serial port.
struct SocketLink(Client);

impl update::Link for SocketLink {
    fn request(&mut self, kind: u8, payload: &[u8], want: u8) -> anyhow::Result<Vec<u8>> {
        let (k, p) = self.0.request(kind, payload)?;
        if k == want {
            return Ok(p);
        }
        if k == scurry_proto::kind::ACK {
            // Hand the ack back rather than turning it into an error here: the
            // update module knows which codes mean what for each step, and
            // "not authorised" deserves a better sentence than a number.
            return Ok(p);
        }
        anyhow::bail!("the app answered with a {k:#04x} message")
    }
}

#[derive(Debug, Clone)]
pub enum Progress {
    Idle,
    Working(String),
    Sending { sent: u32, total: u32 },
    Done(String),
    Failed(String),
}

pub struct FirmwarePane {
    info: Option<FirmwareInfo>,
    latest: Option<Release>,
    /// Set when the release check failed, so the pane can say why rather than
    /// silently offering nothing.
    check_error: Option<String>,
    local_path: String,
    progress: Arc<Mutex<Progress>>,
}

impl Default for FirmwarePane {
    fn default() -> Self {
        Self {
            info: None,
            latest: None,
            check_error: None,
            local_path: String::new(),
            progress: Arc::new(Mutex::new(Progress::Idle)),
        }
    }
}

impl FirmwarePane {
    fn busy(&self) -> bool {
        matches!(
            *self.progress.lock().unwrap(),
            Progress::Working(_) | Progress::Sending { .. }
        )
    }

    /// Ask the dongle what it runs, and GitHub what it could run.
    pub fn refresh(&mut self) {
        self.check_error = None;
        match Client::connect().and_then(|c| {
            let mut link = SocketLink(c);
            update::firmware_info(&mut link)
        }) {
            Ok(info) => self.info = Some(info),
            Err(e) => {
                self.info = None;
                self.check_error = Some(format!("Could not ask the dongle: {e}"));
                return;
            }
        }
        match update::latest_release() {
            Ok(r) => self.latest = Some(r),
            Err(e) => self.check_error = Some(format!("Could not check for releases: {e}")),
        }
    }

    fn start(&mut self, source: Source) {
        let progress = Arc::clone(&self.progress);
        *progress.lock().unwrap() = Progress::Working("Preparing…".into());

        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let image = match source {
                    Source::Release(rel) => {
                        *progress.lock().unwrap() =
                            Progress::Working(format!("Downloading {}…", rel.tag));
                        update::download_firmware(&rel)?
                    }
                    Source::File(path) => {
                        *progress.lock().unwrap() = Progress::Working("Reading the image…".into());
                        std::fs::read(&path)
                            .map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?
                    }
                };

                let mut c = Client::connect()?;
                // Erasing a slot and validating the image both take seconds.
                c.set_timeout(std::time::Duration::from_secs(45))?;
                let mut link = SocketLink(c);
                update::flash(&mut link, &image, &mut |sent, total| {
                    *progress.lock().unwrap() = Progress::Sending { sent, total };
                })?;
                Ok(())
            })();

            *progress.lock().unwrap() = match result {
                Ok(()) => Progress::Done(
                    "Installed. The dongle is rebooting into the new firmware.".into(),
                ),
                Err(e) => Progress::Failed(format!("{e}")),
            };
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Firmware");

        // A dropped file is the quickest way to install something you just
        // built, and costs no dependency: egui reports drops already.
        if let Some(path) = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .find_map(|f| f.path.as_ref().map(|p| p.display().to_string()))
        }) {
            self.local_path = path;
        }

        match &self.info {
            None => {
                ui.label(
                    self.check_error
                        .clone()
                        .unwrap_or_else(|| "Not connected to a dongle.".into()),
                );
                if ui.button("Check again").clicked() {
                    self.refresh();
                }
                return;
            }
            Some(info) => {
                let running = info.version_str();
                ui.label(format!(
                    "Dongle is running {}",
                    if running.is_empty() { "an unversioned build" } else { running }
                ));
                if info.pending_verify {
                    ui.label(
                        "This image has not confirmed itself yet. It will be kept once it \
                         has been running a little longer.",
                    );
                }
                if !info.ota_capable {
                    ui.label(
                        "This build has a single app slot, so it has nowhere to put a second \
                         image. Flash it once over the cable; after that it can update itself.",
                    );
                    return;
                }
            }
        }

        let running = self.info.as_ref().map(|i| i.version_str().to_string()).unwrap_or_default();
        let busy = self.busy();

        ui.separator();

        match self.latest.clone() {
            Some(rel) if update::is_newer(&rel.tag, &running) => {
                ui.label(format!("{} is available.", rel.tag));
                if ui.add_enabled(!busy, egui::Button::new("Install update")).clicked() {
                    self.start(Source::Release(rel));
                }
            }
            Some(rel) => {
                ui.label(format!("Up to date. Latest release is {}.", rel.tag));
                // Reinstalling the same version is how somebody recovers from a
                // half-finished update, so it stays available rather than being
                // hidden behind "you don't need this".
                if ui.add_enabled(!busy, egui::Button::new("Reinstall")).clicked() {
                    self.start(Source::Release(rel));
                }
            }
            None => {
                ui.label(
                    self.check_error
                        .clone()
                        .unwrap_or_else(|| "Release not checked yet.".into()),
                );
                if ui.add_enabled(!busy, egui::Button::new("Check for updates")).clicked() {
                    self.refresh();
                }
            }
        }

        ui.separator();
        ui.label("Or install a local image — drag a .bin onto this window, or type its path:");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.local_path)
                    .hint_text("scurry-dongle-esp32c3.bin")
                    .desired_width(320.0),
            );
            let ready = !busy && !self.local_path.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new("Install file")).clicked() {
                self.start(Source::File(self.local_path.trim().to_string()));
            }
        });

        let progress = self.progress.lock().unwrap().clone();
        match progress {
            Progress::Idle => {}
            Progress::Working(msg) => {
                ui.separator();
                ui.label(msg);
                ui.spinner();
                ctx.request_repaint();
            }
            Progress::Sending { sent, total } => {
                ui.separator();
                let f = sent as f32 / total.max(1) as f32;
                ui.add(egui::ProgressBar::new(f).show_percentage());
                ui.label("Do not unplug the dongle.");
                // Nothing else drives a redraw while a worker thread makes the
                // progress, so without this the bar would sit still.
                ctx.request_repaint();
            }
            Progress::Done(msg) => {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(60, 160, 60), msg);
            }
            Progress::Failed(msg) => {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(200, 80, 80), msg);
            }
        }
    }
}

enum Source {
    Release(Release),
    File(String),
}

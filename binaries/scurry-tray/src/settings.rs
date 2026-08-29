//! The settings pane.
//!
//! Edits the virtual desktop and writes it back to the dongle. Nothing is
//! stored locally: the layout is read from the dongle on open and pushed back
//! on save, so two controller machines cannot disagree about it.
//!
//! Runs as its own process, launched by the tray. See `tray.rs` for why.

use eframe::egui;
use scurry_ctl::config::{Config, ScreenConfig};

use crate::status;

pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 460.0])
            .with_min_inner_size([480.0, 320.0])
            .with_title("scurry settings"),
        ..Default::default()
    };
    eframe::run_native(
        "scurry settings",
        options,
        Box::new(|_cc| Ok(Box::new(SettingsApp::new()))),
    )
    .map_err(|e| anyhow::anyhow!("settings window: {e}"))
}

struct SettingsApp {
    screens: Vec<ScreenConfig>,
    /// Result of the last load or save, shown inline. `Ok` messages are
    /// transient reassurance; `Err` messages are the whole reason this pane can
    /// be trusted, since the dongle validates independently.
    message: Option<Result<String, String>>,
}

impl SettingsApp {
    fn new() -> Self {
        let mut app = Self { screens: Vec::new(), message: None };
        app.load();
        app
    }

    fn load(&mut self) {
        match status::load_config() {
            Ok(cfg) => {
                self.screens = cfg.screens;
                self.message = Some(Ok(format!("Loaded {} screens", self.screens.len())));
            }
            Err(e) => self.message = Some(Err(format!("{e}"))),
        }
    }

    fn save(&mut self) {
        let cfg = Config { screens: self.screens.clone() };
        match status::save_config(&cfg) {
            Ok(()) => self.message = Some(Ok("Saved to the dongle".into())),
            Err(e) => self.message = Some(Err(format!("{e}"))),
        }
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("head").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("Virtual desktop");
            ui.label(
                "Screens are rectangles in one shared coordinate space. Pushing the pointer \
                 off an edge moves it to whichever screen is there.",
            );
            ui.add_space(8.0);
        });

        egui::TopBottomPanel::bottom("foot").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save to dongle").clicked() {
                    self.save();
                }
                if ui.button("Reload").clicked() {
                    self.load();
                }
                if ui.button("Add screen").clicked() {
                    // Next free node id, so a new row is usable without the
                    // user working out which slots are taken.
                    let node = (1..=4u8)
                        .find(|n| !self.screens.iter().any(|s| s.node == *n))
                        .unwrap_or(1);
                    self.screens.push(ScreenConfig {
                        name: format!("machine {node}"),
                        node,
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                        address: None,
                    });
                }
            });
            if let Some(msg) = &self.message {
                ui.add_space(4.0);
                match msg {
                    Ok(text) => ui.colored_label(egui::Color32::from_rgb(0x4c, 0xaf, 0x50), text),
                    Err(text) => ui.colored_label(egui::Color32::from_rgb(0xe5, 0x53, 0x53), text),
                };
            }
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut remove = None;
                for (i, screen) in self.screens.iter_mut().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label("Name");
                            ui.add(egui::TextEdit::singleline(&mut screen.name).desired_width(140.0));
                            ui.label("Node");
                            ui.add(egui::DragValue::new(&mut screen.node).range(0..=4));
                            if screen.node == 0 {
                                ui.label("(this machine)");
                            }
                            // Node 0 is the controller's own screen; a layout
                            // without one is rejected, so do not offer to
                            // delete it by accident.
                            if screen.node != 0 && ui.button("Remove").clicked() {
                                remove = Some(i);
                            }
                        });
                        if screen.node != 0 {
                            ui.horizontal(|ui| {
                                ui.label("Address");
                                let mut text = screen.address.clone().unwrap_or_default();
                                let changed = ui
                                    .add(
                                        egui::TextEdit::singleline(&mut text)
                                            .desired_width(180.0)
                                            .hint_text("aa:bb:cc:dd:ee:ff"),
                                    )
                                    .changed();
                                if changed {
                                    // Empty means unpinned: whichever machine
                                    // connects into this slot first.
                                    screen.address =
                                        (!text.trim().is_empty()).then(|| text.trim().to_string());
                                }
                                if screen.address.is_none() {
                                    ui.label("unpinned — first to connect");
                                }
                            });
                        }
                        ui.horizontal(|ui| {
                            ui.label("Position");
                            ui.add(egui::DragValue::new(&mut screen.x).prefix("x "));
                            ui.add(egui::DragValue::new(&mut screen.y).prefix("y "));
                            ui.separator();
                            ui.label("Size");
                            ui.add(egui::DragValue::new(&mut screen.width).prefix("w ").range(1..=32767));
                            ui.add(egui::DragValue::new(&mut screen.height).prefix("h ").range(1..=32767));
                        });
                    });
                    ui.add_space(4.0);
                }
                if let Some(i) = remove {
                    self.screens.remove(i);
                }
            });
        });
    }
}

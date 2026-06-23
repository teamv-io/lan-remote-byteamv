use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossbeam_channel::Receiver;
use eframe::egui;

use crate::viewer::{egui_key_to_wire, ViewerHandle};

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("lan-remote")
            .with_inner_size([400.0, 340.0])
            .with_min_inner_size([320.0, 280.0]),
        ..Default::default()
    };

    eframe::run_native(
        "lan-remote",
        options,
        Box::new(|_cc| {
            Ok(Box::new(App::default()) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

// ─── Tab ──────────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Tab {
    Host,
    View,
}

// ─── Mode ─────────────────────────────────────────────────────────────────────

enum Mode {
    Idle,
    Hosting {
        status: Arc<Mutex<String>>,
        stop: Arc<AtomicBool>,
    },
    Connecting {
        result_rx: Receiver<Result<ViewerHandle>>,
    },
    Viewing(ViewerHandle),
}

// ─── App ──────────────────────────────────────────────────────────────────────

struct App {
    tab: Tab,
    fps: u32,
    bitrate: u32,
    host_ip: String,
    local_ip: String,
    mode: Mode,
    texture: Option<egui::TextureHandle>,
    screen_rect: egui::Rect,
    connect_error: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        let local_ip = detect_local_ip();
        Self {
            tab: Tab::Host,
            fps: 30,
            bitrate: 8,
            host_ip: String::new(),
            local_ip,
            mode: Mode::Idle,
            texture: None,
            screen_rect: egui::Rect::ZERO,
            connect_error: None,
        }
    }
}

fn detect_local_ip() -> String {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").ok();
    if let Some(s) = sock {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "unknown".to_string()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Check if we're in Viewing mode
        let is_viewing = matches!(self.mode, Mode::Viewing(_));

        if is_viewing {
            self.render_viewer(ctx);
        } else {
            self.render_menu(ctx);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Stop any running host or viewer threads
        match &self.mode {
            Mode::Hosting { stop, .. } => stop.store(true, Ordering::Relaxed),
            Mode::Viewing(h) => h.stop.store(true, Ordering::Relaxed),
            _ => {}
        }
    }
}

impl App {
    fn render_menu(&mut self, ctx: &egui::Context) {
        // If we're connecting, poll for the result.
        // We extract the receiver first to avoid holding a borrow on self.mode
        // while mutating it.
        if matches!(self.mode, Mode::Connecting { .. }) {
            let poll = if let Mode::Connecting { result_rx } = &self.mode {
                Some(result_rx.try_recv())
            } else {
                None
            };
            match poll {
                Some(Ok(Ok(handle))) => {
                    self.mode = Mode::Viewing(handle);
                    self.connect_error = None;
                    ctx.request_repaint();
                    return;
                }
                Some(Ok(Err(e))) => {
                    self.connect_error = Some(format!("Connection failed: {e:#}"));
                    self.mode = Mode::Idle;
                    ctx.request_repaint();
                }
                Some(Err(crossbeam_channel::TryRecvError::Empty)) => {
                    // Still connecting — keep polling
                    ctx.request_repaint();
                }
                Some(Err(crossbeam_channel::TryRecvError::Disconnected)) => {
                    self.connect_error = Some("Connection thread panicked".to_string());
                    self.mode = Mode::Idle;
                }
                None => {}
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(16.0);

            // Title
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new("lan-remote")
                        .heading()
                        .strong(),
                );
                ui.label(
                    egui::RichText::new("Direct LAN · Max Performance")
                        .small()
                        .color(egui::Color32::GRAY),
                );
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            // Tab bar (centered)
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Host, "  HOST  ");
                    ui.selectable_value(&mut self.tab, Tab::View, "  VIEW  ");
                });
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(12.0);

            match self.tab {
                Tab::Host => self.show_host_tab(ui),
                Tab::View => self.show_view_tab(ui, ctx),
            }
        });
    }

    fn show_host_tab(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("host_grid")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label("Your LAN IP:");
                ui.label(
                    egui::RichText::new(&self.local_ip)
                        .monospace()
                        .color(egui::Color32::from_rgb(80, 200, 80)),
                );
                ui.end_row();

                ui.label("FPS:");
                ui.add(egui::Slider::new(&mut self.fps, 5..=60));
                ui.end_row();

                ui.label("Bitrate:");
                ui.add(
                    egui::Slider::new(&mut self.bitrate, 1..=50)
                        .suffix(" Mbps"),
                );
                ui.end_row();
            });

        ui.add_space(16.0);

        // Start / Stop button (centered)
        // Extract the stop handle before entering the layout closure to avoid
        // holding a borrow on self.mode while we mutate it.
        let is_hosting = matches!(self.mode, Mode::Hosting { .. });
        let hosting_stop: Option<Arc<AtomicBool>> = if let Mode::Hosting { stop, .. } = &self.mode {
            Some(stop.clone())
        } else {
            None
        };

        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            if let Some(stop) = hosting_stop {
                if ui
                    .add(
                        egui::Button::new("⬛  Stop")
                            .min_size(egui::vec2(180.0, 36.0)),
                    )
                    .clicked()
                {
                    stop.store(true, Ordering::Relaxed);
                    self.mode = Mode::Idle;
                }
            } else if !is_hosting {
                if ui
                    .add(
                        egui::Button::new("▶  Start Hosting")
                            .min_size(egui::vec2(180.0, 36.0)),
                    )
                    .clicked()
                {
                    let status = Arc::new(Mutex::new("Starting…".to_string()));
                    let stop = Arc::new(AtomicBool::new(false));
                    let status_clone = status.clone();
                    let stop_clone = stop.clone();
                    let fps = self.fps;
                    let bitrate = self.bitrate;

                    std::thread::Builder::new()
                        .name("host-run".into())
                        .spawn(move || {
                            if let Err(e) = crate::host::run_with_stop(
                                "0.0.0.0",
                                7272,
                                fps,
                                bitrate,
                                status_clone,
                                stop_clone,
                            ) {
                                tracing::error!("Host error: {e:#}");
                            }
                        })
                        .ok();

                    self.mode = Mode::Hosting { status, stop };
                }
            }
        });

        ui.add_space(12.0);

        // Status line
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            match &self.mode {
                Mode::Hosting { status, .. } => {
                    let s = status.lock().unwrap().clone();
                    ui.label(
                        egui::RichText::new(format!("● {s}"))
                            .color(egui::Color32::from_rgb(80, 200, 80))
                            .small(),
                    );
                }
                _ => {
                    ui.label(
                        egui::RichText::new("● Idle")
                            .color(egui::Color32::GRAY)
                            .small(),
                    );
                }
            }

            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Share your IP with the viewer")
                    .small()
                    .color(egui::Color32::GRAY),
            );
        });
    }

    fn show_view_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Grid::new("view_grid")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label("Host IP:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.host_ip)
                        .hint_text("192.168.1.x")
                        .desired_width(160.0),
                );
                ui.end_row();
            });

        ui.add_space(16.0);

        let is_connecting = matches!(self.mode, Mode::Connecting { .. });
        let can_connect = !self.host_ip.is_empty() && !is_connecting;

        // Connect button (centered)
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            let btn = egui::Button::new("▶  Connect").min_size(egui::vec2(180.0, 36.0));
            if ui.add_enabled(can_connect, btn).clicked() {
                self.connect_error = None;
                let ip = self.host_ip.clone();
                let ctx2 = ctx.clone();
                let (result_tx, result_rx) = crossbeam_channel::bounded(1);

                std::thread::Builder::new()
                    .name("viewer-connect".into())
                    .spawn(move || {
                        let result = crate::viewer::spawn_threads(&ip, 7272, ctx2);
                        result_tx.send(result).ok();
                    })
                    .ok();

                self.mode = Mode::Connecting { result_rx };
            }

            if is_connecting {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Connecting…")
                        .color(egui::Color32::GRAY)
                        .small(),
                );
                ctx.request_repaint();
            }
        });

        // Error message
        if let Some(err) = &self.connect_error {
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(err)
                        .color(egui::Color32::from_rgb(220, 60, 60))
                        .small(),
                );
            });
        }

        ui.add_space(12.0);
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new("● Not connected")
                    .color(egui::Color32::GRAY)
                    .small(),
            );
        });
    }

    fn render_viewer(&mut self, ctx: &egui::Context) {
        // Drain latest decoded frames
        if let Mode::Viewing(handle) = &self.mode {
            while let Ok(frame) = handle.frame_rx.try_recv() {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.data,
                );
                match self.texture.as_mut() {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.texture = Some(ctx.load_texture(
                            "remote_screen_gui",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
            }
        }

        // Render the remote screen
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                if let Some(tex) = &self.texture {
                    let available = ui.available_size();
                    let resp = ui.add(
                        egui::Image::new(tex)
                            .fit_to_exact_size(available)
                            .sense(egui::Sense::hover()),
                    );
                    self.screen_rect = resp.rect;
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Connecting…")
                                .color(egui::Color32::WHITE),
                        );
                    });
                }
            });

        // Disconnect overlay button (top-right)
        let mut disconnect = false;
        egui::Area::new(egui::Id::new("overlay"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 8.0))
            .show(ctx, |ui| {
                if ui.button("✖  Disconnect").clicked() {
                    disconnect = true;
                }
            });

        if disconnect {
            if let Mode::Viewing(h) = &self.mode {
                h.stop.store(true, Ordering::Relaxed);
            }
            self.mode = Mode::Idle;
            self.texture = None;
            self.screen_rect = egui::Rect::ZERO;
            return;
        }

        // Forward input events to the remote host
        if self.texture.is_some() && self.screen_rect != egui::Rect::ZERO {
            if let Mode::Viewing(handle) = &self.mode {
                let events = ctx.input(|i| i.events.clone());
                let hover = ctx.input(|i| i.pointer.hover_pos());
                let scroll = ctx.input(|i| i.smooth_scroll_delta);

                // Mouse move
                if let Some(pos) = hover {
                    if self.screen_rect.contains(pos) {
                        let nx = ((pos.x - self.screen_rect.min.x) / self.screen_rect.width())
                            .clamp(0.0, 1.0);
                        let ny = ((pos.y - self.screen_rect.min.y) / self.screen_rect.height())
                            .clamp(0.0, 1.0);
                        handle
                            .input_tx
                            .try_send(crate::proto::ControlMsg::MouseMove { nx, ny })
                            .ok();
                    }
                }

                // Scroll
                if scroll.length() > 0.1 {
                    handle
                        .input_tx
                        .try_send(crate::proto::ControlMsg::MouseScroll {
                            dx: scroll.x / 20.0,
                            dy: scroll.y / 20.0,
                        })
                        .ok();
                }

                // Keyboard + pointer buttons
                for event in &events {
                    match event {
                        egui::Event::PointerButton { button, pressed, .. } => {
                            let btn = match button {
                                egui::PointerButton::Primary => 0u8,
                                egui::PointerButton::Secondary => 1,
                                egui::PointerButton::Middle => 2,
                                _ => continue,
                            };
                            handle
                                .input_tx
                                .try_send(crate::proto::ControlMsg::MouseButton {
                                    btn,
                                    pressed: *pressed,
                                })
                                .ok();
                        }
                        egui::Event::Key { key, pressed, .. } => {
                            if let Some(kc) = egui_key_to_wire(*key) {
                                handle
                                    .input_tx
                                    .try_send(crate::proto::ControlMsg::KeyPress {
                                        keycode: kc,
                                        pressed: *pressed,
                                    })
                                    .ok();
                            }
                        }
                        egui::Event::Text(text) => {
                            for ch in text.chars() {
                                if !ch.is_control() {
                                    handle
                                        .input_tx
                                        .try_send(crate::proto::ControlMsg::KeyChar {
                                            ch: ch as u32,
                                        })
                                        .ok();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

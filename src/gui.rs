use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossbeam_channel::Receiver;
use eframe::egui;

use crate::viewer::{egui_key_to_wire, ViewerHandle};

/// Accent color used across the UI (matches the app icon).
const ACCENT: egui::Color32 = egui::Color32::from_rgb(67, 196, 99);

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rust P2P Viewer")
            .with_inner_size([460.0, 440.0])
            .with_min_inner_size([380.0, 360.0])
            .with_icon(std::sync::Arc::new(load_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "Rust P2P Viewer",
        options,
        Box::new(|cc| {
            setup_theme(&cc.egui_ctx);
            Ok(Box::new(App::default()) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Window/dock icon — raw 64×64 RGBA generated at build time (no decoder needed).
fn load_icon() -> egui::IconData {
    let rgba = include_bytes!("../icons/icon_64.rgba").to_vec();
    egui::IconData {
        rgba,
        width: 64,
        height: 64,
    }
}

/// Apply a polished dark theme with the accent color and comfortable spacing.
fn setup_theme(ctx: &egui::Context) {
    use egui::{Color32, FontFamily, FontId, Stroke, TextStyle};

    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(27, 31, 40);
    v.window_fill = Color32::from_rgb(27, 31, 40);
    v.extreme_bg_color = Color32::from_rgb(17, 20, 27);
    v.selection.bg_fill = Color32::from_rgb(34, 64, 45);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.widgets.inactive.bg_fill = Color32::from_rgb(38, 43, 54);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(38, 43, 54);
    v.widgets.hovered.bg_fill = Color32::from_rgb(48, 54, 67);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.slider_width = 150.0;
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(26.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(14.0, FontFamily::Monospace),
        ),
    ]
    .into();
    ctx.set_style(style);
}

// ─── Tab ────────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Tab {
    Host,
    Connect,
}

// ─── Host state ───────────────────────────────────────────────────────────────

enum HostState {
    Idle,
    Hosting {
        status: Arc<Mutex<String>>,
        stop: Arc<AtomicBool>,
    },
}

// ─── Pending connection ─────────────────────────────────────────────────────

/// A connection attempt in flight (handshake runs on a worker thread).
struct Pending {
    label: String,
    rx: Receiver<Result<ViewerHandle>>,
}

// ─── Session ──────────────────────────────────────────────────────────────────

/// An active viewer session, rendered in its own OS window (egui viewport).
struct Session {
    id: egui::ViewportId,
    label: String,
    handle: ViewerHandle,
    texture: Option<egui::TextureHandle>,
    screen_rect: egui::Rect,
    /// Last normalized cursor position sent, to avoid flooding the host with
    /// identical MouseMove events every frame (which would pin the host cursor).
    last_mouse: Option<(f32, f32)>,
}

// ─── App ──────────────────────────────────────────────────────────────────────

struct App {
    tab: Tab,
    // Host settings
    fps: u32,
    bitrate: u32,
    host_password: String,
    host_clipboard: bool,
    local_ip: String,
    host: HostState,
    // Connect form
    host_ip: String,
    view_password: String,
    connect_error: Option<String>,
    // Multi-session state
    pending: Vec<Pending>,
    sessions: Vec<Session>,
    next_session: u64,
}

impl Default for App {
    fn default() -> Self {
        Self {
            tab: Tab::Connect,
            fps: 30,
            bitrate: 8,
            host_password: String::new(),
            host_clipboard: false,
            local_ip: detect_local_ip(),
            host: HostState::Idle,
            host_ip: String::new(),
            view_password: String::new(),
            connect_error: None,
            pending: Vec::new(),
            sessions: Vec::new(),
            next_session: 0,
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Promote any finished connection attempts into live session windows.
        self.poll_pending(&ctx);

        // Main control window.
        self.render_main(ui);

        // Each active session renders in its own OS window.
        self.render_sessions(&ctx);

        // Keep the event loop ticking while anything is live so frames update.
        if !self.sessions.is_empty() || !self.pending.is_empty() {
            ctx.request_repaint();
        }
    }

    fn on_exit(&mut self) {
        if let HostState::Hosting { stop, .. } = &self.host {
            stop.store(true, Ordering::Relaxed);
        }
        for s in &self.sessions {
            s.handle.stop.store(true, Ordering::Relaxed);
        }
    }
}

impl App {
    fn poll_pending(&mut self, ctx: &egui::Context) {
        let mut still = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            match p.rx.try_recv() {
                Ok(Ok(handle)) => {
                    let id = self.next_session;
                    self.next_session += 1;
                    self.sessions.push(Session {
                        id: egui::ViewportId::from_hash_of(("rpv-session", id)),
                        label: p.label.clone(),
                        handle,
                        texture: None,
                        screen_rect: egui::Rect::ZERO,
                        last_mouse: None,
                    });
                    self.connect_error = None;
                    ctx.request_repaint();
                }
                Ok(Err(e)) => {
                    self.connect_error = Some(format!("Connect to {} failed: {e:#}", p.label));
                }
                Err(crossbeam_channel::TryRecvError::Empty) => still.push(p),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.connect_error = Some(format!("Connect to {} failed", p.label));
                }
            }
        }
        self.pending = still;
    }

    fn render_main(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.add_space(16.0);

            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Rust P2P Viewer").heading().strong());
                ui.label(
                    egui::RichText::new("Direct LAN · Low Latency")
                        .small()
                        .color(egui::Color32::from_gray(140)),
                );
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(12.0);

            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let tab_size = egui::vec2(110.0, 30.0);
                    if ui
                        .add_sized(
                            tab_size,
                            egui::SelectableLabel::new(self.tab == Tab::Connect, "CONNECT"),
                        )
                        .clicked()
                    {
                        self.tab = Tab::Connect;
                    }
                    if ui
                        .add_sized(
                            tab_size,
                            egui::SelectableLabel::new(self.tab == Tab::Host, "HOST"),
                        )
                        .clicked()
                    {
                        self.tab = Tab::Host;
                    }
                });
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(16.0);

            match self.tab {
                Tab::Host => self.show_host_tab(ui),
                Tab::Connect => self.show_connect_tab(ui),
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
                        .color(ACCENT),
                );
                ui.end_row();

                ui.label("Password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.host_password)
                        .password(true)
                        .hint_text("required to connect")
                        .desired_width(160.0),
                );
                ui.end_row();

                ui.label("FPS:");
                ui.add(egui::Slider::new(&mut self.fps, 5..=60));
                ui.end_row();

                ui.label("Bitrate:");
                ui.add(egui::Slider::new(&mut self.bitrate, 1..=50).suffix(" Mbps"));
                ui.end_row();

                ui.label("Clipboard:");
                ui.checkbox(&mut self.host_clipboard, "Share clipboard with viewer");
                ui.end_row();
            });

        ui.add_space(16.0);

        let hosting_stop: Option<Arc<AtomicBool>> = match &self.host {
            HostState::Hosting { stop, .. } => Some(stop.clone()),
            HostState::Idle => None,
        };

        ui.vertical_centered(|ui| {
            if let Some(stop) = hosting_stop {
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("⬛  Stop").strong())
                            .fill(egui::Color32::from_rgb(200, 70, 60))
                            .min_size(egui::vec2(200.0, 40.0)),
                    )
                    .clicked()
                {
                    stop.store(true, Ordering::Relaxed);
                    self.host = HostState::Idle;
                }
            } else if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("▶  Start Hosting")
                            .color(egui::Color32::from_rgb(15, 22, 16))
                            .strong(),
                    )
                    .fill(ACCENT)
                    .min_size(egui::vec2(200.0, 40.0)),
                )
                .clicked()
            {
                let status = Arc::new(Mutex::new("Starting…".to_string()));
                let stop = Arc::new(AtomicBool::new(false));
                let status_clone = status.clone();
                let stop_clone = stop.clone();
                let fps = self.fps;
                let bitrate = self.bitrate;
                let password = self.host_password.clone();
                let clipboard = self.host_clipboard;

                std::thread::Builder::new()
                    .name("host-run".into())
                    .spawn(move || {
                        if let Err(e) = crate::host::run_with_stop(
                            "0.0.0.0",
                            7272,
                            fps,
                            bitrate,
                            password,
                            clipboard,
                            status_clone,
                            stop_clone,
                        ) {
                            tracing::error!("Host error: {e:#}");
                        }
                    })
                    .ok();

                self.host = HostState::Hosting { status, stop };
            }
        });

        ui.add_space(12.0);

        ui.vertical_centered(|ui| match &self.host {
            HostState::Hosting { status, .. } => {
                let s = status.lock().unwrap().clone();
                ui.label(egui::RichText::new(format!("● {s}")).color(ACCENT).small());
            }
            HostState::Idle => {
                ui.label(
                    egui::RichText::new("● Idle")
                        .color(egui::Color32::GRAY)
                        .small(),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("Share your IP + password with the viewer")
                        .small()
                        .color(egui::Color32::from_gray(120)),
                );
            }
        });
    }

    fn show_connect_tab(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("connect_grid")
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

                ui.label("Password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.view_password)
                        .password(true)
                        .hint_text("host's password")
                        .desired_width(160.0),
                );
                ui.end_row();
            });

        ui.add_space(14.0);

        let can_connect = !self.host_ip.trim().is_empty();
        ui.vertical_centered(|ui| {
            let btn = egui::Button::new(
                egui::RichText::new("▶  Connect")
                    .color(egui::Color32::from_rgb(15, 22, 16))
                    .strong(),
            )
            .fill(ACCENT)
            .min_size(egui::vec2(200.0, 40.0));
            if ui.add_enabled(can_connect, btn).clicked() {
                self.start_connect(ui.ctx().clone());
            }
        });

        if let Some(err) = &self.connect_error {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(err)
                        .color(egui::Color32::from_rgb(220, 80, 80))
                        .small(),
                );
            });
        }

        if !self.pending.is_empty() {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(format!("Connecting… ({})", self.pending.len()))
                        .color(egui::Color32::from_gray(150))
                        .small(),
                );
            });
        }

        // Active session list with per-session disconnect.
        if !self.sessions.is_empty() {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(format!("Active sessions ({})", self.sessions.len())).strong(),
            );
            ui.add_space(4.0);

            let mut close: Vec<egui::ViewportId> = Vec::new();
            for s in &self.sessions {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("●").color(ACCENT));
                    ui.label(&s.label);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Disconnect").clicked() {
                            close.push(s.id);
                        }
                        let mut clip = s.handle.clipboard_enabled.load(Ordering::Relaxed);
                        if ui
                            .checkbox(&mut clip, "clipboard")
                            .on_hover_text("Sync clipboard with this host")
                            .changed()
                        {
                            s.handle.clipboard_enabled.store(clip, Ordering::Relaxed);
                        }
                    });
                });
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Tip: drag a file onto a session window to send it")
                    .small()
                    .color(egui::Color32::from_gray(120)),
            );
            if !close.is_empty() {
                self.sessions.retain(|s| {
                    if close.contains(&s.id) {
                        s.handle.stop.store(true, Ordering::Relaxed);
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    fn start_connect(&mut self, ctx: egui::Context) {
        self.connect_error = None;
        let ip = self.host_ip.trim().to_string();
        let pw = self.view_password.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::Builder::new()
            .name("viewer-connect".into())
            .spawn(move || {
                let result = crate::viewer::spawn_threads(&ip, 7272, &pw, ctx);
                tx.send(result).ok();
            })
            .ok();

        self.pending.push(Pending {
            label: self.host_ip.trim().to_string(),
            rx,
        });
    }

    fn render_sessions(&mut self, ctx: &egui::Context) {
        // Take the list out so each session can be borrowed mutably inside the
        // viewport callback without conflicting with `self`.
        let mut sessions = std::mem::take(&mut self.sessions);
        sessions.retain_mut(|s| render_one_session(ctx, s));
        // Preserve any sessions that were created while we were rendering.
        self.sessions.append(&mut sessions);
    }
}

/// Render a single session in its own OS window. Returns `false` if the window
/// was closed (so the caller drops the session).
fn render_one_session(ctx: &egui::Context, s: &mut Session) -> bool {
    // Pull the newest decoded frames into the texture (done outside the viewport
    // callback so the borrow of `s` stays simple).
    while let Ok(frame) = s.handle.frame_rx.try_recv() {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.data,
        );
        match s.texture.as_mut() {
            Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
            None => {
                s.texture = Some(ctx.load_texture(
                    format!("remote-{:?}", s.id),
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
    }

    let builder = egui::ViewportBuilder::default()
        .with_title(format!("{} — Rust P2P Viewer", s.label))
        .with_inner_size([1280.0, 720.0]);

    let mut keep = true;
    ctx.show_viewport_immediate(s.id, builder, |vctx, _class| {
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK))
            .show(vctx, |ui| {
                if let Some(tex) = &s.texture {
                    s.screen_rect = paint_remote(ui, tex);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("Connecting…").color(egui::Color32::WHITE));
                    });
                }

                // Show a hint while a file is being dragged over the window.
                if vctx.input(|i| !i.raw.hovered_files.is_empty()) {
                    ui.painter().rect_filled(
                        ui.max_rect(),
                        0.0,
                        egui::Color32::from_black_alpha(160),
                    );
                    ui.painter().text(
                        ui.max_rect().center(),
                        egui::Align2::CENTER_CENTER,
                        "Drop to send file to host",
                        egui::FontId::proportional(28.0),
                        ACCENT,
                    );
                }
            });

        forward_input(vctx, s);

        // Files dropped on the window are sent to the host.
        let dropped = vctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            if let Some(path) = f.path {
                send_file(path, s.handle.input_tx.clone());
            }
        }

        if vctx.input(|i| i.viewport().close_requested()) {
            keep = false;
        }
        vctx.request_repaint();
    });

    if !keep {
        s.handle.stop.store(true, Ordering::Relaxed);
    }
    keep
}

/// Forward mouse/keyboard/scroll events from a session window to its remote host.
fn forward_input(vctx: &egui::Context, s: &mut Session) {
    if s.texture.is_none() || s.screen_rect == egui::Rect::ZERO {
        return;
    }
    use crate::proto::ControlMsg;

    let events = vctx.input(|i| i.events.clone());
    let hover = vctx.input(|i| i.pointer.hover_pos());
    let scroll = vctx.input(|i| i.smooth_scroll_delta);

    if let Some(pos) = hover {
        if s.screen_rect.contains(pos) {
            let nx = ((pos.x - s.screen_rect.min.x) / s.screen_rect.width()).clamp(0.0, 1.0);
            let ny = ((pos.y - s.screen_rect.min.y) / s.screen_rect.height()).clamp(0.0, 1.0);
            // Only send when the position actually changed — sending the same
            // position every frame would continuously pin the host's own cursor.
            let changed = match s.last_mouse {
                Some((lx, ly)) => (nx - lx).abs() > 0.0005 || (ny - ly).abs() > 0.0005,
                None => true,
            };
            if changed {
                s.last_mouse = Some((nx, ny));
                s.handle
                    .input_tx
                    .try_send(ControlMsg::MouseMove { nx, ny })
                    .ok();
            }
        }
    }

    if scroll.length() > 0.1 {
        s.handle
            .input_tx
            .try_send(ControlMsg::MouseScroll {
                dx: scroll.x / 20.0,
                dy: scroll.y / 20.0,
            })
            .ok();
    }

    for event in &events {
        match event {
            egui::Event::PointerButton {
                button, pressed, ..
            } => {
                let btn = match button {
                    egui::PointerButton::Primary => 0u8,
                    egui::PointerButton::Secondary => 1,
                    egui::PointerButton::Middle => 2,
                    _ => continue,
                };
                s.handle
                    .input_tx
                    .try_send(ControlMsg::MouseButton {
                        btn,
                        pressed: *pressed,
                    })
                    .ok();
            }
            egui::Event::Key { key, pressed, .. } => {
                if let Some(kc) = egui_key_to_wire(*key) {
                    s.handle
                        .input_tx
                        .try_send(ControlMsg::KeyPress {
                            keycode: kc,
                            pressed: *pressed,
                        })
                        .ok();
                }
            }
            egui::Event::Text(text) => {
                for ch in text.chars() {
                    if !ch.is_control() {
                        s.handle
                            .input_tx
                            .try_send(ControlMsg::KeyChar { ch: ch as u32 })
                            .ok();
                    }
                }
            }
            _ => {}
        }
    }
}

/// Monotonic id generator for file transfers within this process.
fn next_file_id() -> u32 {
    use std::sync::atomic::AtomicU32;
    static N: AtomicU32 = AtomicU32::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

/// Read a file on a background thread and stream it to the host over the control
/// channel (blocking sends, so chunks are never dropped).
fn send_file(path: std::path::PathBuf, tx: crossbeam_channel::Sender<crate::proto::ControlMsg>) {
    use crate::proto::ControlMsg;
    use std::io::Read;

    std::thread::Builder::new()
        .name("file-send".into())
        .spawn(move || {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            let mut file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("Open {} failed: {e}", path.display());
                    return;
                }
            };
            let size = file.metadata().map(|m| m.len()).unwrap_or(0);
            let id = next_file_id();

            if tx
                .send(ControlMsg::FileStart {
                    id,
                    name: name.clone(),
                    size,
                })
                .is_err()
            {
                return;
            }
            let mut buf = vec![0u8; crate::sync::FILE_CHUNK];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx
                            .send(ControlMsg::FileChunk {
                                id,
                                data: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Read {} failed: {e}", path.display());
                        break;
                    }
                }
            }
            tx.send(ControlMsg::FileEnd { id }).ok();
            tracing::info!("Sent file '{name}' ({size} bytes)");
        })
        .ok();
}

/// Paint a texture into the current UI, aspect-correct and centered, and return
/// the exact rect the image occupies (for input coordinate mapping).
pub(crate) fn paint_remote(ui: &mut egui::Ui, tex: &egui::TextureHandle) -> egui::Rect {
    let panel = ui.available_rect_before_wrap();
    let img = tex.size_vec2();
    if img.x <= 0.0 || img.y <= 0.0 {
        return panel;
    }
    let scale = (panel.width() / img.x).min(panel.height() / img.y);
    let draw = img * scale;
    let rect = egui::Rect::from_center_size(panel.center(), draw);
    ui.painter().image(
        tex.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    rect
}

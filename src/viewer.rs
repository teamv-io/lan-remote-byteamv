use std::collections::HashMap;
use std::net::{TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use eframe::egui;
use tracing::{error, info, warn};

use crate::codec::VideoDecoder;
use crate::proto::{ControlMsg, InboundVideo, VIDEO_PORT};
use crate::transport::ControlChannel;

/// A decoded RGBA frame ready for display
pub struct RgbaFrame {
    pub data: Vec<u8>, // RGBA flat bytes
    pub width: u32,
    pub height: u32,
}

/// Handle returned by spawn_threads — holds channels and stop flag for GUI integration
pub struct ViewerHandle {
    pub frame_rx: Receiver<RgbaFrame>,
    pub input_tx: Sender<ControlMsg>,
    pub remote_w: u32,
    pub remote_h: u32,
    pub stop: Arc<AtomicBool>,
}

/// Connects + handshakes synchronously, then spawns decode pipeline threads.
/// Returns ViewerHandle with channels. Setting stop=true causes all threads to exit.
pub fn spawn_threads(host: &str, port: u16, ctx: egui::Context) -> Result<ViewerHandle> {
    let addr = format!("{host}:{port}");
    let stream = TcpStream::connect(&addr).context("connect to host")?;
    stream.set_nodelay(true)?;
    info!("Connected to {addr}");

    let mut ctrl = ControlChannel::new(stream);
    ctrl.send(&ControlMsg::Hello)?;

    let (remote_w, remote_h, fps) = match ctrl.recv()? {
        ControlMsg::Welcome { width, height, fps } => (width, height, fps),
        other => anyhow::bail!("expected Welcome, got {other:?}"),
    };
    info!("Remote screen {remote_w}×{remote_h} @ {fps} fps");

    let stop = Arc::new(AtomicBool::new(false));

    // Channel: assembled NAL data → decoder
    let (nal_tx, nal_rx) = bounded::<Vec<u8>>(4);
    // Channel: decoded RGBA frames → GUI
    let (frame_tx, frame_rx) = bounded::<RgbaFrame>(2);
    // Channel: input events from GUI → TCP sender
    let (input_tx, input_rx) = bounded::<ControlMsg>(64);

    // UDP receiver thread — assembles chunks into complete H.264 NALs
    {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("udp-recv".into())
            .spawn(move || udp_receiver(nal_tx, stop))?;
    }

    // Decoder thread
    {
        let stop = stop.clone();
        let frame_tx2 = frame_tx;
        let ctx2 = ctx.clone();
        std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || {
                let mut dec = match VideoDecoder::new() {
                    Ok(d) => d,
                    Err(e) => { error!("Decoder init: {e:#}"); return; }
                };
                loop {
                    if stop.load(Ordering::Relaxed) { break; }
                    match nal_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(nal) => {
                            match dec.decode(&nal) {
                                Ok(Some((data, w, h))) => {
                                    frame_tx2.try_send(RgbaFrame { data, width: w, height: h }).ok();
                                    ctx2.request_repaint();
                                }
                                Ok(None) => {}
                                Err(e) => warn!("Decode error: {e}"),
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
    }

    // Input sender thread — reads from input_rx, sends over TCP ctrl channel
    {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("input-send".into())
            .spawn(move || {
                loop {
                    if stop.load(Ordering::Relaxed) { break; }
                    match input_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(msg) => {
                            if ctrl.send(&msg).is_err() { break; }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
    }

    Ok(ViewerHandle {
        frame_rx,
        input_tx,
        remote_w,
        remote_h,
        stop,
    })
}

/// Receive UDP datagrams, reassemble chunks into complete H.264 NALs
fn udp_receiver(nal_tx: Sender<Vec<u8>>, stop: Arc<AtomicBool>) {
    let sock = match UdpSocket::bind(format!("0.0.0.0:{VIDEO_PORT}")) {
        Ok(s) => s,
        Err(e) => { error!("Bind UDP recv: {e}"); return; }
    };
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    info!("UDP video receiver on port {VIDEO_PORT}");

    // frame_id → (expected_chunks, chunks_received: HashMap<chunk_idx, data>)
    let mut pending: HashMap<u32, (u16, HashMap<u16, Vec<u8>>)> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    let mut last_seen_id: u32 = 0;

    loop {
        if stop.load(Ordering::Relaxed) { break; }

        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                   || e.kind() == std::io::ErrorKind::TimedOut => {
                continue;
            }
            Err(e) => { warn!("UDP recv: {e}"); continue; }
        };

        let Some(pkt) = InboundVideo::parse(&buf[..n]) else { continue };

        // Drop frames we've already passed
        if pkt.frame_id.wrapping_sub(last_seen_id) > 60 {
            continue;
        }

        let entry = pending
            .entry(pkt.frame_id)
            .or_insert_with(|| (pkt.total_chunks, HashMap::new()));

        entry.1.insert(pkt.chunk_idx, pkt.data);

        if entry.1.len() == entry.0 as usize {
            // All chunks received — reassemble in order
            let total = entry.0;
            let chunks = pending.remove(&pkt.frame_id).unwrap().1;
            let mut assembled = Vec::new();
            for i in 0..total {
                if let Some(d) = chunks.get(&i) {
                    assembled.extend_from_slice(d);
                }
            }
            last_seen_id = pkt.frame_id;
            nal_tx.try_send(assembled).ok();

            // Evict any stale pending frames older than this one
            pending.retain(|&id, _| id.wrapping_sub(last_seen_id) < 120);
        }
    }
}

// ─── CLI run path ──────────────────────────────────────────────────────────────

/// Used by the CLI `view` subcommand — opens its own eframe window
pub fn run(host: &str, port: u16) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("lan-remote viewer")
            .with_inner_size([1280.0, 720.0])
            .with_resizable(true),
        ..Default::default()
    };

    let host = host.to_string();
    eframe::run_native(
        "lan-remote viewer",
        options,
        Box::new(move |cc| {
            let handle = spawn_threads(&host, port, cc.egui_ctx.clone())
                .expect("Failed to connect to host");
            Ok(Box::new(ViewerWindow::new(handle)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// eframe App that renders the remote screen and forwards input events
pub struct ViewerWindow {
    handle: ViewerHandle,
    texture: Option<egui::TextureHandle>,
    screen_rect: egui::Rect,
}

impl ViewerWindow {
    pub fn new(handle: ViewerHandle) -> Self {
        Self {
            handle,
            texture: None,
            screen_rect: egui::Rect::ZERO,
        }
    }
}

impl eframe::App for ViewerWindow {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Drain latest frame from decoder
        while let Ok(frame) = self.handle.frame_rx.try_recv() {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.data,
            );
            match self.texture.as_mut() {
                Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ctx.load_texture(
                        "remote_screen",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::BLACK))
            .show_inside(ui, |ui| {
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
                        ui.label(egui::RichText::new("Connecting…").color(egui::Color32::WHITE));
                    });
                }
            });

        // Forward input if we have an active screen rect
        if self.texture.is_some() && self.screen_rect != egui::Rect::ZERO {
            let events = ctx.input(|i| i.events.clone());
            let hover = ctx.input(|i| i.pointer.hover_pos());
            let scroll = ctx.input(|i| i.smooth_scroll_delta);
            let _ = ctx; // suppress unused warning

            // Mouse move
            if let Some(pos) = hover {
                if self.screen_rect.contains(pos) {
                    let nx = ((pos.x - self.screen_rect.min.x) / self.screen_rect.width())
                        .clamp(0.0, 1.0);
                    let ny = ((pos.y - self.screen_rect.min.y) / self.screen_rect.height())
                        .clamp(0.0, 1.0);
                    self.handle.input_tx.try_send(ControlMsg::MouseMove { nx, ny }).ok();
                }
            }

            // Mouse scroll
            if scroll.length() > 0.1 {
                self.handle
                    .input_tx
                    .try_send(ControlMsg::MouseScroll {
                        dx: scroll.x / 20.0,
                        dy: scroll.y / 20.0,
                    })
                    .ok();
            }

            // Keyboard and pointer button events
            for event in &events {
                match event {
                    egui::Event::PointerButton { button, pressed, .. } => {
                        let btn = match button {
                            egui::PointerButton::Primary => 0u8,
                            egui::PointerButton::Secondary => 1,
                            egui::PointerButton::Middle => 2,
                            _ => continue,
                        };
                        self.handle
                            .input_tx
                            .try_send(ControlMsg::MouseButton { btn, pressed: *pressed })
                            .ok();
                    }
                    egui::Event::Key { key, pressed, .. } => {
                        if let Some(kc) = egui_key_to_wire(*key) {
                            self.handle
                                .input_tx
                                .try_send(ControlMsg::KeyPress { keycode: kc, pressed: *pressed })
                                .ok();
                        }
                    }
                    egui::Event::Text(text) => {
                        for ch in text.chars() {
                            if !ch.is_control() {
                                self.handle
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
    }

    fn on_exit(&mut self) {
        self.handle.stop.store(true, Ordering::Relaxed);
    }
}

/// Map egui::Key to our wire key codes (same table used by host.rs).
/// Note: Space is handled via Event::Text(" "), not as a Key variant.
pub fn egui_key_to_wire(key: egui::Key) -> Option<u32> {
    Some(match key {
        egui::Key::Enter => 0x00,
        egui::Key::Tab => 0x01,
        egui::Key::Backspace => 0x03,
        egui::Key::Delete => 0x04,
        egui::Key::Escape => 0x05,
        egui::Key::ArrowUp => 0x10,
        egui::Key::ArrowDown => 0x11,
        egui::Key::ArrowLeft => 0x12,
        egui::Key::ArrowRight => 0x13,
        egui::Key::Home => 0x14,
        egui::Key::End => 0x15,
        egui::Key::PageUp => 0x16,
        egui::Key::PageDown => 0x17,
        egui::Key::F1 => 0x30,
        egui::Key::F2 => 0x31,
        egui::Key::F3 => 0x32,
        egui::Key::F4 => 0x33,
        egui::Key::F5 => 0x34,
        egui::Key::F6 => 0x35,
        egui::Key::F7 => 0x36,
        egui::Key::F8 => 0x37,
        egui::Key::F9 => 0x38,
        egui::Key::F10 => 0x39,
        egui::Key::F11 => 0x3A,
        egui::Key::F12 => 0x3B,
        _ => return None,
    })
}

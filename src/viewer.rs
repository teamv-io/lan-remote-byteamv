use std::collections::HashMap;
use std::net::{TcpStream, UdpSocket};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::codec::VideoDecoder;
use crate::proto::{ControlMsg, InboundVideo, VIDEO_PORT};
use crate::transport::ControlChannel;

/// Decoded frame ready for display
struct DecodedFrame {
    pixels: Vec<u32>, // XRGB u32
    width: u32,
    height: u32,
}

pub fn run(host: &str, port: u16) -> Result<()> {
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

    // Channel: assembled NAL data → decoder
    let (nal_tx, nal_rx) = bounded::<Vec<u8>>(4);
    // Channel: decoded XRGB frames → winit render
    let (frame_tx, frame_rx) = bounded::<DecodedFrame>(2);
    // Channel: input events from winit → TCP sender
    let (input_tx, input_rx) = bounded::<ControlMsg>(64);

    // UDP receiver thread — assembles chunks into complete frames
    std::thread::Builder::new()
        .name("udp-recv".into())
        .spawn(move || udp_receiver(nal_tx))?;

    // Decoder thread
    std::thread::Builder::new()
        .name("decoder".into())
        .spawn(move || {
            let mut dec = match VideoDecoder::new() {
                Ok(d) => d,
                Err(e) => { error!("Decoder init: {e:#}"); return; }
            };
            while let Ok(nal) = nal_rx.recv() {
                match dec.decode(&nal) {
                    Ok(Some((pixels, w, h))) => {
                        frame_tx.try_send(DecodedFrame { pixels, width: w, height: h }).ok();
                    }
                    Ok(None) => {}
                    Err(e) => warn!("Decode error: {e}"),
                }
            }
        })?;

    // TCP input sender thread
    std::thread::Builder::new()
        .name("input-send".into())
        .spawn(move || {
            while let Ok(msg) = input_rx.recv() {
                if ctrl.send(&msg).is_err() {
                    break;
                }
            }
        })?;

    // winit event loop — must run on main thread
    let event_loop = EventLoop::new().context("create event loop")?;
    let mut app = ViewerApp {
        window: None,
        surface: None,
        context: None,
        frame_rx,
        input_tx,
        remote_w,
        remote_h,
        current_w: remote_w,
        current_h: remote_h,
        cursor_in_window: false,
    };
    event_loop.run_app(&mut app).context("event loop")?;
    Ok(())
}

struct ViewerApp {
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    frame_rx: Receiver<DecodedFrame>,
    input_tx: Sender<ControlMsg>,
    remote_w: u32,
    remote_h: u32,
    current_w: u32,
    current_h: u32,
    cursor_in_window: bool,
}

impl ApplicationHandler for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("lan-remote viewer")
            .with_inner_size(LogicalSize::new(self.remote_w, self.remote_h))
            .with_resizable(true);
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let context = softbuffer::Context::new(window.clone()).unwrap();
        let surface = softbuffer::Surface::new(&context, window.clone()).unwrap();
        self.context = Some(context);
        self.surface = Some(surface);
        self.window = Some(window);
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        // Drain any pending decoded frames; request redraw after accepting one
        if let Ok(frame) = self.frame_rx.try_recv() {
            self.current_w = frame.width;
            self.current_h = frame.height;
            if let (Some(surface), Some(window)) = (self.surface.as_mut(), self.window.as_ref()) {
                let w = NonZeroU32::new(frame.width).unwrap();
                let h = NonZeroU32::new(frame.height).unwrap();
                if surface.resize(w, h).is_ok() {
                    if let Ok(mut buf) = surface.buffer_mut() {
                        let len = (frame.width * frame.height) as usize;
                        buf[..len].copy_from_slice(&frame.pixels[..len]);
                        buf.present().ok();
                    }
                }
                window.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        let win = match self.window.as_ref() { Some(w) => w, None => return };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::RedrawRequested => {
                // Rendering happens in about_to_wait; nothing to do here
            }

            WindowEvent::CursorMoved { position, .. } if self.cursor_in_window => {
                let size = win.inner_size();
                let nx = (position.x / size.width as f64).clamp(0.0, 1.0) as f32;
                let ny = (position.y / size.height as f64).clamp(0.0, 1.0) as f32;
                self.input_tx.try_send(ControlMsg::MouseMove { nx, ny }).ok();
            }

            WindowEvent::CursorEntered { .. } => self.cursor_in_window = true,
            WindowEvent::CursorLeft { .. } => self.cursor_in_window = false,

            WindowEvent::MouseInput { state, button, .. } => {
                let btn = match button {
                    winit::event::MouseButton::Left => 0,
                    winit::event::MouseButton::Right => 1,
                    winit::event::MouseButton::Middle => 2,
                    _ => return,
                };
                self.input_tx
                    .try_send(ControlMsg::MouseButton {
                        btn,
                        pressed: state == ElementState::Pressed,
                    })
                    .ok();
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 20.0, p.y as f32 / 20.0),
                };
                self.input_tx
                    .try_send(ControlMsg::MouseScroll { dx, dy })
                    .ok();
            }

            WindowEvent::KeyboardInput { event: key_event, .. } => {
                let pressed = key_event.state == ElementState::Pressed;

                // Send text for printable keys on press
                if pressed {
                    if let Some(text) = &key_event.text {
                        for ch in text.chars() {
                            if !ch.is_control() {
                                self.input_tx
                                    .try_send(ControlMsg::KeyChar { ch: ch as u32 })
                                    .ok();
                            }
                        }
                    }
                }

                // Send raw keycode for special/modifier keys
                if let PhysicalKey::Code(code) = key_event.physical_key {
                    if let Some(kc) = winit_keycode_to_u32(code) {
                        self.input_tx
                            .try_send(ControlMsg::KeyPress { keycode: kc, pressed })
                            .ok();
                    }
                }
            }

            _ => {}
        }
    }
}

/// Receive UDP datagrams, reassemble chunks into complete H.264 frames
fn udp_receiver(nal_tx: Sender<Vec<u8>>) {
    let sock = match UdpSocket::bind(format!("0.0.0.0:{VIDEO_PORT}")) {
        Ok(s) => s,
        Err(e) => { error!("Bind UDP recv: {e}"); return; }
    };
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    info!("UDP video receiver on port {VIDEO_PORT}");

    // frame_id → (expected_chunks, chunks_received: HashMap<chunk_idx, data>)
    let mut pending: HashMap<u32, (u16, HashMap<u16, Vec<u8>>)> = HashMap::new();
    let mut buf = vec![0u8; 65536];
    let mut last_seen_id: u32 = 0;

    loop {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
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

/// Map winit KeyCode variants that have special host-side meaning to our wire codes.
/// Printable characters are handled via KeyChar instead, so only special keys go here.
fn winit_keycode_to_u32(code: winit::keyboard::KeyCode) -> Option<u32> {
    use winit::keyboard::KeyCode::*;
    Some(match code {
        Enter => 0x00,
        Tab => 0x01,
        Space => 0x02,
        Backspace => 0x03,
        Delete => 0x04,
        Escape => 0x05,
        ArrowUp => 0x10,
        ArrowDown => 0x11,
        ArrowLeft => 0x12,
        ArrowRight => 0x13,
        Home => 0x14,
        End => 0x15,
        PageUp => 0x16,
        PageDown => 0x17,
        ShiftLeft | ShiftRight => 0x20,
        ControlLeft | ControlRight => 0x21,
        AltLeft | AltRight => 0x22,
        SuperLeft | SuperRight => 0x23,
        F1 => 0x30,
        F2 => 0x31,
        F3 => 0x32,
        F4 => 0x33,
        F5 => 0x34,
        F6 => 0x35,
        F7 => 0x36,
        F8 => 0x37,
        F9 => 0x38,
        F10 => 0x39,
        F11 => 0x3A,
        F12 => 0x3B,
        CapsLock => 0x40,
        _ => return None,
    })
}

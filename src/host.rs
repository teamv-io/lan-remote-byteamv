use std::net::{TcpListener, UdpSocket};
use std::time::Instant;

use anyhow::{Context, Result};
use crossbeam_channel::bounded;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use scap::capturer::{Capturer, Options, Resolution};
use scap::frame::{Frame, FrameType};
use tracing::{error, info, warn};

use crate::codec::VideoEncoder;
use crate::proto::{ControlMsg, VideoPacket, VIDEO_CHUNK_MAX, VIDEO_PORT};
use crate::transport::ControlChannel;

pub fn run(bind: &str, port: u16, fps: u32, bitrate_mbps: u32) -> Result<()> {
    // macOS requires Screen Recording permission; scap will tell us
    if !scap::is_supported() {
        anyhow::bail!("Screen capture not supported on this platform");
    }
    #[cfg(target_os = "macos")]
    if !scap::has_permission() {
        eprintln!("Screen Recording permission required.");
        eprintln!("Grant it in System Settings → Privacy & Security → Screen Recording");
        scap::request_permission();
        std::process::exit(1);
    }

    let listener = TcpListener::bind(format!("{bind}:{port}")).context("bind TCP")?;
    info!("Listening on {bind}:{port}");
    info!("Run `lan-remote view <this_ip>` on the viewer machine");

    // Accept one viewer at a time (single-session design)
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let peer = stream.peer_addr()?;
                info!("Viewer connected from {peer}");
                if let Err(e) = handle_session(stream, peer.ip().to_string(), fps, bitrate_mbps) {
                    error!("Session error: {e:#}");
                }
                info!("Viewer disconnected, waiting for next connection");
            }
            Err(e) => error!("Accept error: {e}"),
        }
    }
    Ok(())
}

fn handle_session(
    stream: std::net::TcpStream,
    viewer_ip: String,
    fps: u32,
    bitrate_mbps: u32,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut ctrl = ControlChannel::new(stream);

    // Handshake: wait for Hello, then send Welcome
    match ctrl.recv()? {
        ControlMsg::Hello => {}
        other => anyhow::bail!("expected Hello, got {other:?}"),
    }

    // Probe first frame to learn dimensions
    let (width, height) = probe_screen_size(fps)?;
    ctrl.send(&ControlMsg::Welcome { width, height, fps })?;
    info!("Screen size {width}×{height}, streaming at {fps} fps / {bitrate_mbps} Mbps");

    // Channel: raw BGRA frames → encoder thread
    let (frame_tx, frame_rx) = bounded::<Vec<u8>>(2);
    // Channel: encoded NAL data → UDP sender thread
    let (nal_tx, nal_rx) = bounded::<Vec<u8>>(4);

    let viewer_video_addr = format!("{viewer_ip}:{VIDEO_PORT}");
    let (w, h) = (width as usize, height as usize);

    // Encoder thread
    std::thread::Builder::new()
        .name("encoder".into())
        .spawn(move || {
            let mut enc = match VideoEncoder::new(w, h, fps, bitrate_mbps) {
                Ok(e) => e,
                Err(e) => { error!("Encoder init: {e:#}"); return; }
            };
            while let Ok(bgra) = frame_rx.recv() {
                match enc.encode_bgra(&bgra) {
                    Ok(nal) if !nal.is_empty() => { nal_tx.send(nal).ok(); }
                    Ok(_) => {}
                    Err(e) => warn!("Encode error: {e}"),
                }
            }
        })?;

    // UDP sender thread
    std::thread::Builder::new()
        .name("udp-sender".into())
        .spawn(move || {
            let sock = UdpSocket::bind("0.0.0.0:0").expect("bind UDP sender");
            sock.connect(&viewer_video_addr).expect("connect UDP sender");
            let mut frame_id: u32 = 0;
            let t0 = Instant::now();
            let mut pkt_buf = Vec::with_capacity(VIDEO_CHUNK_MAX + crate::proto::VIDEO_HDR_LEN);

            while let Ok(nal) = nal_rx.recv() {
                let chunks: Vec<&[u8]> = nal.chunks(VIDEO_CHUNK_MAX).collect();
                let total = chunks.len() as u16;
                let pts_ms = t0.elapsed().as_millis() as u32;

                for (idx, chunk) in chunks.iter().enumerate() {
                    pkt_buf.clear();
                    VideoPacket {
                        frame_id,
                        chunk_idx: idx as u16,
                        total_chunks: total,
                        pts_ms,
                        data: chunk,
                    }
                    .write_to(&mut pkt_buf);
                    if let Err(e) = sock.send(&pkt_buf) {
                        warn!("UDP send: {e}");
                    }
                }
                frame_id = frame_id.wrapping_add(1);
            }
        })?;

    // Capture thread (owns the Capturer — must be created on the thread it runs on)
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let mut capturer = Capturer::new(Options {
                fps: fps as u64,
                show_cursor: true,
                show_highlight: false,
                excluded_targets: None,
                output_type: FrameType::BGRAFrame,
                output_resolution: Resolution::Captured,
                source_rect: None,
                target: None,
            });
            capturer.start_capture();

            loop {
                match capturer.get_next_frame() {
                    Ok(Frame::BGRA(f)) => {
                        // Only send if encoder is keeping up (drop oldest if full)
                        frame_tx.try_send(f.data).ok();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("Capture error: {e}");
                        break;
                    }
                }
            }
            capturer.stop_capture();
        })?;

    // Input injection (current thread — blocks reading control msgs)
    let mut enigo = Enigo::new(&Settings::default()).context("create Enigo")?;
    let sw = width as f64;
    let sh = height as f64;

    loop {
        match ctrl.recv() {
            Ok(msg) => handle_input(&mut enigo, msg, sw, sh),
            Err(e) => {
                info!("Control channel closed: {e}");
                break;
            }
        }
    }
    Ok(())
}

fn handle_input(enigo: &mut Enigo, msg: ControlMsg, sw: f64, sh: f64) {
    use ControlMsg::*;
    match msg {
        MouseMove { nx, ny } => {
            let x = (nx as f64 * sw) as i32;
            let y = (ny as f64 * sh) as i32;
            enigo.move_mouse(x, y, Coordinate::Abs).ok();
        }
        MouseButton { btn, pressed } => {
            let button = match btn {
                0 => Button::Left,
                1 => Button::Right,
                2 => Button::Middle,
                _ => return,
            };
            let dir = if pressed { Direction::Press } else { Direction::Release };
            enigo.button(button, dir).ok();
        }
        MouseScroll { dx, dy } => {
            if dy.abs() > 0.01 {
                enigo.scroll(dy as i32, Axis::Vertical).ok();
            }
            if dx.abs() > 0.01 {
                enigo.scroll(dx as i32, Axis::Horizontal).ok();
            }
        }
        KeyChar { ch } => {
            if let Some(c) = char::from_u32(ch) {
                enigo.text(&c.to_string()).ok();
            }
        }
        KeyPress { keycode, pressed } => {
            let dir = if pressed { Direction::Press } else { Direction::Release };
            if let Some(key) = winit_keycode_to_enigo(keycode) {
                enigo.key(key, dir).ok();
            }
        }
        Ping => {}
        _ => {}
    }
}

/// One-frame capture just to learn the screen resolution
fn probe_screen_size(fps: u32) -> Result<(u32, u32)> {
    let mut capturer = Capturer::new(Options {
        fps: fps as u64,
        show_cursor: false,
        show_highlight: false,
        excluded_targets: None,
        output_type: FrameType::BGRAFrame,
        output_resolution: Resolution::Captured,
        source_rect: None,
        target: None,
    });
    capturer.start_capture();
    let result = loop {
        match capturer.get_next_frame() {
            Ok(Frame::BGRA(f)) => break Ok((f.width as u32, f.height as u32)),
            Ok(_) => continue,
            Err(e) => break Err(anyhow::anyhow!("probe capture: {e}")),
        }
    };
    capturer.stop_capture();
    result
}

/// Map a subset of winit KeyCode discriminants to enigo Key values.
/// winit KeyCode is repr(u32) — we transmit its u32 value over the wire.
fn winit_keycode_to_enigo(code: u32) -> Option<Key> {
    // winit 0.30 KeyCode values (physical keyboard, US layout independent)
    Some(match code {
        0x00 => Key::Return,       // Enter
        0x01 => Key::Tab,
        0x02 => Key::Space,
        0x03 => Key::Backspace,
        0x04 => Key::Delete,
        0x05 => Key::Escape,
        0x10 => Key::UpArrow,
        0x11 => Key::DownArrow,
        0x12 => Key::LeftArrow,
        0x13 => Key::RightArrow,
        0x14 => Key::Home,
        0x15 => Key::End,
        0x16 => Key::PageUp,
        0x17 => Key::PageDown,
        0x20 => Key::Shift,
        0x21 => Key::Control,
        0x22 => Key::Alt,
        0x23 => Key::Meta,
        0x30 => Key::F1,
        0x31 => Key::F2,
        0x32 => Key::F3,
        0x33 => Key::F4,
        0x34 => Key::F5,
        0x35 => Key::F6,
        0x36 => Key::F7,
        0x37 => Key::F8,
        0x38 => Key::F9,
        0x39 => Key::F10,
        0x3A => Key::F11,
        0x3B => Key::F12,
        0x40 => Key::CapsLock,
        _ => return None,
    })
}

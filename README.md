# lan-remote

Direct LAN remote desktop — no relay server, no cloud, maximum performance.  
Inspired by RustDesk but stripped to the minimum for single-hop LAN use.

## Architecture

```
Host (machine being controlled)        Viewer (your controlling machine)
├─ scap: DXGI / ScreenCaptureKit       ├─ UDP :7274 receive + reassemble
├─ H.264 encode (openh264)             ├─ H.264 decode
├─ UDP :0 → viewer:7274 (video)        ├─ winit window + softbuffer render
└─ TCP :7272 receive input events      └─ TCP → host:7272 send input events
```

- **Video**: UDP, chunked H.264 NAL units, host→viewer
- **Input**: TCP, length-prefixed bincode messages, viewer→host
- **Latency target**: < 16 ms end-to-end on gigabit LAN

## Requirements

| Platform | Dependency |
|----------|-----------|
| macOS (host) | Screen Recording permission in System Settings → Privacy & Security |
| macOS (host) | Accessibility permission for input injection (enigo) |
| Windows (host) | No extra permissions needed for DXGI capture |
| Both | Rust 1.78+ |

openh264 downloads Cisco's prebuilt library at build time — internet required for first build.

## Build

```bash
cargo build --release
```

## Usage

**On the machine you want to control (host):**
```bash
./lan-remote host
# or with options:
./lan-remote host --fps 60 --bitrate 12
```

**On your controlling machine (viewer):**
```bash
./lan-remote view 192.168.1.X
```

The viewer window shows the remote screen. Mouse and keyboard input is forwarded automatically.

## Options

```
lan-remote host [OPTIONS]
  -b, --bind <IP>      Bind address [default: 0.0.0.0]
  -p, --port <PORT>    TCP control port [default: 7272]
      --fps <N>        Capture FPS [default: 60]
      --bitrate <N>    H.264 bitrate in Mbps [default: 8]

lan-remote view <HOST> [OPTIONS]
  -p, --port <PORT>    Host TCP control port [default: 7272]
```

## Troubleshooting

**macOS: "Screen Recording permission required"**  
Go to System Settings → Privacy & Security → Screen Recording → add the terminal app.

**macOS: input not injecting**  
Go to System Settings → Privacy & Security → Accessibility → add the terminal app.

**Blank window / no video**  
Check that UDP port 7274 is not blocked by a firewall on either machine.

**Compilation errors in codec.rs**  
The `openh264` Rust crate API changed across versions. If `EncoderConfig::new()` fails,  
try `EncoderConfig::new(width as u32, height as u32)`. If `dimension_rgb()` or `strides_yuv()`  
don't exist, check the crate docs for the equivalent dimension/stride accessors on `DecodedYUV`.

mod codec;
mod convert;
mod gui;
mod host;
mod proto;
mod transport;
mod viewer;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rust-p2p-viewer", about = "Direct LAN peer-to-peer remote desktop — low latency")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Share this machine's screen and accept input
    Host {
        #[arg(short, long, default_value = "0.0.0.0", help = "Bind address")]
        bind: String,
        #[arg(short, long, default_value = "7272", help = "TCP control port")]
        port: u16,
        #[arg(long, default_value = "60", help = "Target capture FPS")]
        fps: u32,
        #[arg(long, default_value = "8", help = "H.264 bitrate in Mbps")]
        bitrate: u32,
    },
    /// Connect and view/control a remote host
    View {
        #[arg(help = "Host IP address or hostname")]
        host: String,
        #[arg(short, long, default_value = "7272", help = "Host TCP control port")]
        port: u16,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("rust_p2p_viewer=info".parse()?),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        None => gui::run(),
        Some(Cmd::Host { bind, port, fps, bitrate }) => host::run(&bind, port, fps, bitrate),
        Some(Cmd::View { host, port }) => viewer::run(&host, port),
    }
}

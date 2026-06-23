use std::io::{Read, Write};
use std::net::TcpStream;

use anyhow::{Context, Result, bail};

use crate::proto::ControlMsg;

/// Length-prefixed bincode framing over TCP
pub struct ControlChannel {
    stream: TcpStream,
}

impl ControlChannel {
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    pub fn peer_ip(&self) -> std::net::IpAddr {
        self.stream.peer_addr().unwrap().ip()
    }

    pub fn send(&mut self, msg: &ControlMsg) -> Result<()> {
        let data = bincode::serialize(msg).context("serialize ControlMsg")?;
        self.stream
            .write_all(&(data.len() as u32).to_be_bytes())
            .context("write length")?;
        self.stream.write_all(&data).context("write body")?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<ControlMsg> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).context("read length")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 1_000_000 {
            bail!("control message too large: {len} bytes");
        }
        let mut data = vec![0u8; len];
        self.stream.read_exact(&mut data).context("read body")?;
        bincode::deserialize(&data).context("deserialize ControlMsg")
    }

    /// Clone for spawning a reader thread while the original writes
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone().context("clone TcpStream")?,
        })
    }
}

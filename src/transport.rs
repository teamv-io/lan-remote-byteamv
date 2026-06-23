use std::io::{Read, Write};
use std::net::TcpStream;

use anyhow::{bail, Context, Result};

use crate::crypto::{Cipher, SALT_LEN};
use crate::proto::ControlMsg;

/// Send the handshake salt in the clear (fixed length, no framing).
pub fn send_salt(stream: &mut TcpStream, salt: &[u8; SALT_LEN]) -> Result<()> {
    stream.write_all(salt).context("write salt")
}

/// Read the handshake salt in the clear.
pub fn recv_salt(stream: &mut TcpStream) -> Result<[u8; SALT_LEN]> {
    let mut salt = [0u8; SALT_LEN];
    stream.read_exact(&mut salt).context("read salt")?;
    Ok(salt)
}

/// Length-prefixed, AEAD-encrypted bincode framing over TCP.
pub struct ControlChannel {
    stream: TcpStream,
    cipher: Cipher,
}

impl ControlChannel {
    pub fn new(stream: TcpStream, cipher: Cipher) -> Self {
        Self { stream, cipher }
    }

    pub fn peer_ip(&self) -> std::net::IpAddr {
        self.stream.peer_addr().unwrap().ip()
    }

    pub fn send(&mut self, msg: &ControlMsg) -> Result<()> {
        let plain = bincode::serialize(msg).context("serialize ControlMsg")?;
        let sealed = self.cipher.seal(&plain);
        self.stream
            .write_all(&(sealed.len() as u32).to_be_bytes())
            .context("write length")?;
        self.stream.write_all(&sealed).context("write body")?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<ControlMsg> {
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .context("read length")?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 16_000_000 {
            bail!("control message too large: {len} bytes");
        }
        let mut sealed = vec![0u8; len];
        self.stream.read_exact(&mut sealed).context("read body")?;
        let plain = self
            .cipher
            .open(&sealed)
            .context("decrypt failed — wrong password or corrupted stream")?;
        bincode::deserialize(&plain).context("deserialize ControlMsg")
    }

    /// Clone for spawning a reader thread while the original writes.
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone().context("clone TcpStream")?,
            cipher: self.cipher.clone(),
        })
    }

    /// A raw clone of the underlying stream, for `shutdown()` to unblock a reader
    /// thread that is parked in `recv()` when the session is asked to stop.
    pub fn try_clone_stream(&self) -> Result<TcpStream> {
        self.stream.try_clone().context("clone TcpStream")
    }
}

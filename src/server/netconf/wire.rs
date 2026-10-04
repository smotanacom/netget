//! RFC6242 framing. Lengths and aggregate bounds are checked before body copies.
use anyhow::{bail, ensure, Result};
use std::collections::VecDeque;

pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_CHUNKS: usize = 1024;
pub const MAX_BUFFER_BYTES: usize = MAX_MESSAGE_BYTES + 256 * 1024 + 16 * MAX_CHUNKS;
pub const DELIMITER: &[u8] = b"]]>]]>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    Delimiter,
    Chunked,
}

pub struct Decoder {
    framing: Framing,
    input: VecDeque<u8>,
    body: Vec<u8>,
    header: Vec<u8>,
    remaining: usize,
    chunks: usize,
}
impl Decoder {
    pub fn new(framing: Framing) -> Self {
        Self {
            framing,
            input: VecDeque::new(),
            body: Vec::new(),
            header: Vec::new(),
            remaining: 0,
            chunks: 0,
        }
    }
    pub fn set_framing(&mut self, framing: Framing) -> Result<()> {
        ensure!(
            self.body.is_empty()
                && self.header.is_empty()
                && self.remaining == 0
                && self.chunks == 0,
            "NETCONF framing transition during a message"
        );
        self.framing = framing;
        Ok(())
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_BUFFER_BYTES.saturating_sub(self.input.len()),
            "NETCONF input queue bound"
        );
        self.input.extend(bytes);
        Ok(())
    }
    pub fn is_partial(&self) -> bool {
        !self.input.is_empty()
            || !self.body.is_empty()
            || !self.header.is_empty()
            || self.remaining > 0
            || self.chunks > 0
    }
    pub fn next_message(&mut self) -> Result<Option<Vec<u8>>> {
        if self.framing == Framing::Delimiter {
            while let Some(byte) = self.input.pop_front() {
                self.body.push(byte);
                if self.body.ends_with(DELIMITER) {
                    self.body.truncate(self.body.len() - DELIMITER.len());
                    ensure!(
                        self.body.len() <= MAX_MESSAGE_BYTES,
                        "NETCONF message byte bound"
                    );
                    return Ok(Some(std::mem::take(&mut self.body)));
                }
                ensure!(
                    self.body.len() < MAX_MESSAGE_BYTES + DELIMITER.len(),
                    "NETCONF message byte bound"
                );
            }
            return Ok(None);
        }
        loop {
            if self.remaining > 0 {
                let available = self.remaining.min(self.input.len());
                for _ in 0..available {
                    self.body
                        .push(self.input.pop_front().expect("available byte"));
                }
                self.remaining -= available;
                if self.remaining > 0 {
                    return Ok(None);
                }
            }
            let Some(byte) = self.input.pop_front() else {
                return Ok(None);
            };
            ensure!(self.header.len() < 13, "NETCONF chunk header bound");
            self.header.push(byte);
            match self.header.len() {
                1 => ensure!(byte == b'\n', "NETCONF chunk must start with LF"),
                2 => ensure!(byte == b'#', "NETCONF chunk must start with LF HASH"),
                3 => ensure!(
                    byte == b'#' || (b'1'..=b'9').contains(&byte),
                    "NETCONF chunk size must be positive without leading zero"
                ),
                _ => {
                    if self.header[2] == b'#' {
                        ensure!(
                            self.header.len() == 4 && byte == b'\n',
                            "NETCONF invalid end-of-chunks"
                        );
                        ensure!(self.chunks > 0, "NETCONF empty chunked message");
                        self.header.clear();
                        self.chunks = 0;
                        return Ok(Some(std::mem::take(&mut self.body)));
                    }
                    if byte != b'\n' {
                        ensure!(byte.is_ascii_digit(), "NETCONF invalid chunk size digit");
                        continue;
                    }
                    let mut length = 0u64;
                    for digit in &self.header[2..self.header.len() - 1] {
                        length = length * 10 + u64::from(digit - b'0');
                    }
                    ensure!(
                        length <= u64::from(u32::MAX),
                        "NETCONF native chunk size maximum"
                    );
                    ensure!(
                        length <= (MAX_MESSAGE_BYTES - self.body.len()) as u64,
                        "NETCONF aggregate message byte bound"
                    );
                    ensure!(self.chunks < MAX_CHUNKS, "NETCONF chunk count bound");
                    self.chunks += 1;
                    self.remaining = length as usize;
                    self.header.clear();
                }
            }
        }
    }
}

pub fn frame(message: &[u8], framing: Framing) -> Result<Vec<u8>> {
    ensure!(
        !message.is_empty() && message.len() <= MAX_MESSAGE_BYTES,
        "NETCONF outgoing message byte bound"
    );
    match framing {
        Framing::Delimiter => {
            if message.windows(DELIMITER.len()).any(|w| w == DELIMITER) {
                bail!("NETCONF delimiter inside outgoing XML");
            }
            let mut framed = Vec::with_capacity(message.len() + DELIMITER.len());
            framed.extend_from_slice(message);
            framed.extend_from_slice(DELIMITER);
            Ok(framed)
        }
        Framing::Chunked => {
            let mut framed = format!("\n#{}\n", message.len()).into_bytes();
            framed.extend_from_slice(message);
            framed.extend_from_slice(b"\n##\n");
            Ok(framed)
        }
    }
}

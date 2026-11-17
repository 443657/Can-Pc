//! Host protocol and the bounded frame type used by the CAN bridge.
//!
//! The host link is newline-delimited ASCII. A newline is the only frame
//! boundary, so a command may arrive in several USB packets and one packet may
//! carry several commands.
//!
//! Host -> device:
//!   PING
//!   SWD ID
//!   TX <id> <dlc> <data>
//!   STAT
//!
//! Device -> host:
//!   RX <id> <dlc> <data>       (unsolicited, one line per received CAN frame)
//!   OK ... / ERR ...           (one reply per host command)
//!
//! `<id>` is 1-3 hex digits and must fit the 11-bit Classical CAN standard ID.
//! `<dlc>` is a decimal 0-8. `<data>` is exactly `2 * dlc` hex digits, or `-`
//! when `dlc` is zero. The legacy `CAN TX <id> <data>` form remains accepted;
//! `CAN RX` polling is rejected because received frames are now streamed.

use core::fmt::Write;

use embassy_stm32::can::Frame;
use embedded_can::{Frame as _, Id};
use heapless::String;

/// Longest accepted host line, excluding the terminating newline.
pub const MAX_LINE: usize = 96;
/// Every reply fits in a single 64-byte USB full-speed packet.
pub type Reply = String<63>;

/// A Classical CAN standard data frame with a fixed-size backing store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassicFrame {
    pub id: u16,
    pub len: u8,
    pub data: [u8; 8],
}

impl ClassicFrame {
    pub fn from_can(frame: &Frame) -> Option<Self> {
        let Id::Standard(id) = *frame.id() else {
            return None;
        };
        if frame.is_remote_frame() {
            return None;
        }
        let data = frame.data();
        if data.len() > 8 {
            return None;
        }
        let mut bytes = [0; 8];
        bytes[..data.len()].copy_from_slice(data);
        Some(Self {
            id: id.as_raw(),
            len: data.len() as u8,
            data: bytes,
        })
    }

    pub fn to_can(self) -> Option<Frame> {
        Frame::new_standard(self.id, &self.data[..self.len as usize]).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostCommand {
    Ping,
    SwdId,
    Tx(ClassicFrame),
    Stat,
    /// The old one-shot `CAN RX` command. Streaming replaces it.
    StreamRx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Syntax,
    Frame,
}

impl ParseError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "SYNTAX",
            Self::Frame => "FRAME",
        }
    }
}

/// Parse one complete host line. The caller owns the returned command, so the
/// input borrow ends before any `await`.
pub fn parse(line: &str) -> Result<HostCommand, ParseError> {
    let mut words = line.split_ascii_whitespace();
    match words.next() {
        Some("PING") => {
            if words.next().is_some() {
                return Err(ParseError::Syntax);
            }
            Ok(HostCommand::Ping)
        }
        Some("SWD") => {
            if words.next() == Some("ID") && words.next().is_none() {
                Ok(HostCommand::SwdId)
            } else {
                Err(ParseError::Syntax)
            }
        }
        Some("STAT") => {
            if words.next().is_some() {
                return Err(ParseError::Syntax);
            }
            Ok(HostCommand::Stat)
        }
        Some("TX") => {
            let (Some(id), Some(dlc), Some(data)) = (words.next(), words.next(), words.next())
            else {
                return Err(ParseError::Syntax);
            };
            if words.next().is_some() {
                return Err(ParseError::Syntax);
            }
            let id = parse_id(id)?;
            let dlc = parse_dlc(dlc)?;
            let data = parse_data(data, dlc)?;
            Ok(HostCommand::Tx(ClassicFrame { id, len: dlc, data }))
        }
        Some("CAN") => match (words.next(), words.next(), words.next()) {
            (Some("TX"), Some(id), Some(data)) if words.next().is_none() => {
                let id = parse_id(id)?;
                let (len, data) = parse_legacy_data(data)?;
                Ok(HostCommand::Tx(ClassicFrame { id, len, data }))
            }
            (Some("RX"), None, None) => Ok(HostCommand::StreamRx),
            _ => Err(ParseError::Syntax),
        },
        _ => Err(ParseError::Syntax),
    }
}

fn parse_id(text: &str) -> Result<u16, ParseError> {
    if text.is_empty() || text.len() > 3 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ParseError::Frame);
    }
    let id = u16::from_str_radix(text, 16).map_err(|_| ParseError::Frame)?;
    if id > 0x7FF {
        return Err(ParseError::Frame);
    }
    Ok(id)
}

fn parse_dlc(text: &str) -> Result<u8, ParseError> {
    if text.len() != 1 {
        return Err(ParseError::Frame);
    }
    let byte = text.as_bytes()[0];
    if !byte.is_ascii_digit() || byte - b'0' > 8 {
        return Err(ParseError::Frame);
    }
    Ok(byte - b'0')
}

fn parse_data(text: &str, dlc: u8) -> Result<[u8; 8], ParseError> {
    let len = dlc as usize;
    if len == 0 {
        return if text == "-" {
            Ok([0; 8])
        } else {
            Err(ParseError::Frame)
        };
    }
    if text.len() != len * 2 {
        return Err(ParseError::Frame);
    }
    let mut data = [0; 8];
    for (index, byte) in data[..len].iter_mut().enumerate() {
        let high = hex_nibble(text.as_bytes()[index * 2])?;
        let low = hex_nibble(text.as_bytes()[index * 2 + 1])?;
        *byte = (high << 4) | low;
    }
    Ok(data)
}

fn parse_legacy_data(text: &str) -> Result<(u8, [u8; 8]), ParseError> {
    if text == "-" {
        return Ok((0, [0; 8]));
    }
    if text.is_empty() || text.len() > 16 || !text.len().is_multiple_of(2) {
        return Err(ParseError::Frame);
    }
    let len = (text.len() / 2) as u8;
    let data = parse_data(text, len)?;
    Ok((len, data))
}

fn hex_nibble(byte: u8) -> Result<u8, ParseError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ParseError::Frame),
    }
}

/// Encode one received frame as an unsolicited `RX` line.
pub fn rx_line(frame: &ClassicFrame) -> Reply {
    let mut out = Reply::new();
    write!(out, "RX {:03X} {} ", frame.id, frame.len).expect("RX prefix fits one reply");
    if frame.len == 0 {
        out.push('-').expect("empty payload fits");
    } else {
        for byte in &frame.data[..frame.len as usize] {
            write!(out, "{byte:02X}").expect("8-byte payload fits");
        }
    }
    out.push('\n').expect("line ending fits");
    out
}

/// Four saturating counters rendered as one short reply.
pub fn status_line(rx: u32, tx: u32, drx: u32, dtx: u32) -> Reply {
    let mut out = Reply::new();
    write!(
        out,
        "OK rx={} tx={} drx={} dtx={}\n",
        rx.min(999_999),
        tx.min(999_999),
        drx.min(999_999),
        dtx.min(999_999)
    )
    .expect("status reply fits");
    out
}

/// Incremental newline finder. It keeps only one bounded line and discards an
/// overlong line up to its next newline.
pub struct Line {
    bytes: [u8; MAX_LINE],
    len: usize,
    overflowed: bool,
}

impl Line {
    pub const fn new() -> Self {
        Self {
            bytes: [0; MAX_LINE],
            len: 0,
            overflowed: false,
        }
    }

    pub fn push(&mut self, byte: u8) -> Option<&str> {
        if byte == b'\n' {
            let n = core::mem::take(&mut self.len);
            // An overlong line is discarded up to the newline; never execute
            // a truncated command.
            if core::mem::take(&mut self.overflowed) {
                return Some("");
            }
            return Some(core::str::from_utf8(&self.bytes[..n]).unwrap_or_default());
        }
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        } else {
            self.overflowed = true;
        }
        None
    }
}

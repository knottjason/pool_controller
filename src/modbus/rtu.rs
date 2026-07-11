//! Thin Modbus RTU master: build frames, serial transaction with timeout.

use std::io;
use std::time::{Duration, Instant};

use serialport::SerialPort;
use thiserror::Error;
use tracing::debug;

use super::crc::{append_crc, check_crc};

#[derive(Debug, Error)]
pub enum RtuError {
    #[error("serial I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("serial port: {0}")]
    Port(#[from] serialport::Error),
    #[error("response timeout after {0:?}")]
    Timeout(Duration),
    #[error("CRC mismatch")]
    Crc,
    #[error("short / empty response ({0} bytes)")]
    Short(usize),
    #[error("unexpected response: {0}")]
    Unexpected(String),
}

/// Open the RS485 serial port (8N1, no flow control). Caller owns the port exclusively.
pub fn open_port(
    device: &str,
    baud: u32,
    timeout: Duration,
) -> Result<Box<dyn SerialPort>, RtuError> {
    let port = serialport::new(device, baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(timeout)
        .open()?;
    Ok(port)
}

/// Build a Modbus RTU request: `[addr, fc, payload…]` + CRC16 LE.
#[must_use]
pub fn build_request(addr: u8, fc: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(2 + payload.len() + 2);
    frame.push(addr);
    frame.push(fc);
    frame.extend_from_slice(payload);
    append_crc(&mut frame);
    frame
}

/// Flush input, write `request`, then read a response with overall `timeout`.
///
/// Expects at least `min_response_len` bytes (including CRC). Reads until idle
/// gap or timeout once the minimum is met.
pub fn transact(
    port: &mut dyn SerialPort,
    request: &[u8],
    min_response_len: usize,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<Vec<u8>, RtuError> {
    // Discard stale bytes.
    let _ = port.clear(serialport::ClearBuffer::Input);

    debug!(
        req = %hex_preview(request),
        "RTU TX"
    );
    io::Write::write_all(port, request)?;
    io::Write::flush(port)?;

    // Brief gap after TX so the HAT auto-direction settles before RX.
    std::thread::sleep(inter_frame);

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(min_response_len.max(16));
    let mut tmp = [0_u8; 64];
    let mut last_byte_at = Instant::now();

    loop {
        if Instant::now() >= deadline {
            if buf.len() >= min_response_len {
                break;
            }
            return Err(RtuError::Timeout(timeout));
        }

        // Per-read timeout: remaining overall budget, capped.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining.min(Duration::from_millis(50));
        let _ = port.set_timeout(slice);

        match io::Read::read(port, &mut tmp) {
            Ok(0) => {
                if buf.len() >= min_response_len && last_byte_at.elapsed() >= inter_frame {
                    break;
                }
            }
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                last_byte_at = Instant::now();
                // Exception responses are 5 bytes (addr, fc|0x80, ex, crc_lo, crc_hi).
                if buf.len() >= 5 && buf.get(1).is_some_and(|fc| fc & 0x80 != 0) {
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                if buf.len() >= min_response_len {
                    break;
                }
            }
            Err(err) => return Err(RtuError::Io(err)),
        }
    }

    debug!(
        resp = %hex_preview(&buf),
        len = buf.len(),
        "RTU RX"
    );

    if buf.len() < min_response_len {
        return Err(RtuError::Short(buf.len()));
    }
    if !check_crc(&buf) {
        return Err(RtuError::Crc);
    }
    Ok(buf)
}

/// Convenience: build request + transact.
pub fn request(
    port: &mut dyn SerialPort,
    addr: u8,
    fc: u8,
    payload: &[u8],
    min_response_len: usize,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<Vec<u8>, RtuError> {
    let req = build_request(addr, fc, payload);
    transact(port, &req, min_response_len, timeout, inter_frame)
}

#[must_use]
pub fn hex_preview(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modbus::crc::crc16;

    #[test]
    fn build_request_appends_crc_le() {
        let frame = build_request(0x01, 0x01, &[0x00, 0x00, 0x00, 0x08]);
        assert_eq!(&frame[..6], &[0x01, 0x01, 0x00, 0x00, 0x00, 0x08]);
        let crc = crc16(&frame[..6]);
        assert_eq!(frame[6], (crc & 0xFF) as u8);
        assert_eq!(frame[7], (crc >> 8) as u8);
    }
}

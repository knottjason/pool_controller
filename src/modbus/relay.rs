//! Waveshare Modbus RTU Relay — frames that work on the installed board.
//!
//! Hardware probe on the Waveshare board showed:
//! - **Read** all channels: `01 01 00 FF 00 01` (packed status byte) — V3
//!   `01 01 00 00 00 08` times out.
//! - **Write**: FC `0x05` single-coil per channel (addr `0..=7`) — V3
//!   FC `0x0F` multi-coil times out.
//!
//! Wiki still documents V3 multi-coil; this board revision answers the ESP-era
//! read-at-`0x00FF` + FC05 path instead.
//!
//! Bit0 = channel 1 = MQTT r1 … bit7 = r8.

use std::time::Duration;

use serialport::SerialPort;
use tracing::debug;

use super::rtu::{RtuError, build_request, request};

pub const FC_READ_COILS: u8 = 0x01;
pub const FC_WRITE_SINGLE_COIL: u8 = 0x05;

/// Build read-all-relays request (packed status at coil address `0x00FF`).
#[must_use]
#[allow(dead_code)] // unit-tested frame builder
pub fn build_read_coils(addr: u8) -> Vec<u8> {
    build_request(addr, FC_READ_COILS, &[0x00, 0xFF, 0x00, 0x01])
}

/// Build FC05 write for one channel (`channel` 0..=7, `on` → `0xFF00`).
#[must_use]
#[allow(dead_code)] // unit-tested frame builder
pub fn build_write_coil(addr: u8, channel: u8, on: bool) -> Vec<u8> {
    let coil_hi = 0x00;
    let coil_lo = channel & 0x07;
    let (val_hi, val_lo) = if on { (0xFF, 0x00) } else { (0x00, 0x00) };
    build_request(
        addr,
        FC_WRITE_SINGLE_COIL,
        &[coil_hi, coil_lo, val_hi, val_lo],
    )
}

/// Parse FC 0x01 response: `[addr, 0x01, byte_count, status, crc_lo, crc_hi]`.
pub fn parse_coil_status(resp: &[u8], addr: u8) -> Result<u8, RtuError> {
    if resp.len() < 6 {
        return Err(RtuError::Short(resp.len()));
    }
    if resp[0] != addr {
        return Err(RtuError::Unexpected(format!(
            "addr {:02X} != {:02X}",
            resp[0], addr
        )));
    }
    if resp[1] != FC_READ_COILS {
        return Err(RtuError::Unexpected(format!(
            "fc {:02X}, expected 01",
            resp[1]
        )));
    }
    if resp[2] < 1 {
        return Err(RtuError::Unexpected("byte count 0".into()));
    }
    Ok(resp[3])
}

fn write_coil(
    port: &mut dyn SerialPort,
    addr: u8,
    channel: u8,
    on: bool,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<(), RtuError> {
    let coil_lo = channel & 0x07;
    let (val_hi, val_lo) = if on { (0xFF_u8, 0x00_u8) } else { (0x00, 0x00) };
    debug!(addr, channel, on, "relay write single coil");
    let resp = request(
        port,
        addr,
        FC_WRITE_SINGLE_COIL,
        &[0x00, coil_lo, val_hi, val_lo],
        8, // echo: addr fc coil_hi coil_lo val_hi val_lo crc crc
        timeout,
        inter_frame,
    )?;
    if resp.get(1) != Some(&FC_WRITE_SINGLE_COIL) {
        return Err(RtuError::Unexpected(format!(
            "write ack fc {:02X?}",
            resp.get(1)
        )));
    }
    Ok(())
}

/// Write all 8 coil bits via FC05, then read packed status.
pub fn write_then_read(
    port: &mut dyn SerialPort,
    addr: u8,
    mask: u8,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<u8, RtuError> {
    debug!(addr, mask, "relay write coils (FC05 x8)");
    for channel in 0..8_u8 {
        let on = (mask & (1 << channel)) != 0;
        write_coil(port, addr, channel, on, timeout, inter_frame)?;
        std::thread::sleep(inter_frame);
    }
    read_coils(port, addr, timeout, inter_frame)
}

/// Read packed 8-channel status (`01 01 00 FF 00 01`).
pub fn read_coils(
    port: &mut dyn SerialPort,
    addr: u8,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<u8, RtuError> {
    let resp = request(
        port,
        addr,
        FC_READ_COILS,
        &[0x00, 0xFF, 0x00, 0x01],
        6, // addr fc count status crc crc
        timeout,
        inter_frame,
    )?;
    let status = parse_coil_status(&resp, addr)?;
    debug!(addr, status, "relay read coils");
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modbus::crc::check_crc;

    #[test]
    fn read_frame_esp_style() {
        let frame = build_read_coils(0x01);
        assert_eq!(&frame[..6], &[0x01, 0x01, 0x00, 0xFF, 0x00, 0x01]);
        assert!(check_crc(&frame));
    }

    #[test]
    fn write_single_coil_r4_on() {
        // r4 = channel 3
        let frame = build_write_coil(0x01, 3, true);
        assert_eq!(&frame[..6], &[0x01, 0x05, 0x00, 0x03, 0xFF, 0x00]);
        assert!(check_crc(&frame));
    }

    #[test]
    fn write_single_coil_off() {
        let frame = build_write_coil(0x01, 0, false);
        assert_eq!(&frame[..6], &[0x01, 0x05, 0x00, 0x00, 0x00, 0x00]);
        assert!(check_crc(&frame));
    }

    #[test]
    fn parse_coil_status_ok() {
        let mut resp = vec![0x01, 0x01, 0x01, 0x0C];
        crate::modbus::crc::append_crc(&mut resp);
        assert_eq!(parse_coil_status(&resp, 0x01).unwrap(), 0x0C);
    }
}

//! Reverse-engineered VS pump Modbus (slave 0x15).
//!
//! | FC | Purpose | Payload after `[addr][fc]` |
//! |----|---------|----------------------------|
//! | 0x43 | `GET_STATUS` | `20` |
//! | 0x45 | `READ_SENSOR` | `20 00 <sensor_id>` |
//! | 0x44 | `SET_DEMAND` | `20 <mode> <lo> <hi>` — mode 0 demand = **RPM×4** (LE) |
//! | 0x41 | GO | `20` |
//! | 0x42 | STOP | `20` |
//!
//! Response byte2 must be ACK `0x10`. Sensor RPM/watts words are **uint16 LE**;
//! the RPM sensor word is also **RPM×4** (same scale as demand).

use std::time::Duration;

use serialport::SerialPort;
use tracing::debug;

use super::rtu::{RtuError, build_request, request};

pub const PUMP_ACK_SEND: u8 = 0x20;
pub const PUMP_ACK_RESP: u8 = 0x10;

pub const FC_GO: u8 = 0x41;
pub const FC_STOP: u8 = 0x42;
pub const FC_GET_STATUS: u8 = 0x43;
pub const FC_SET_DEMAND: u8 = 0x44;
pub const FC_READ_SENSOR: u8 = 0x45;

pub const SENSOR_RPM: u8 = 0x00;
pub const SENSOR_AMB_TEMP: u8 = 0x08;
pub const SENSOR_SHAFT_WATTS: u8 = 0x0A;

pub const STATUS_OFF: u8 = 0x00;
pub const STATUS_RUNNING: u8 = 0x0B;

/// Demand mode 0 = speed; demand word is **RPM×4** (ESP `main.cpp` comment).
pub const DEMAND_MODE_SPEED: u8 = 0x00;

/// Typical VS pump RPM band mapped from MQTT `spd` 1..=`spd_max`.
const DEMAND_RPM_MIN: u32 = 600;
const DEMAND_RPM_MAX: u32 = 3450;

/// Map MQTT `spd` (0..=`spd_max`) to Modbus demand (mode 0 = **RPM×4**).
///
/// ESP comment: `0 - Speed RPM*4`. Wire tests where demand≈sensor raw were both
/// in RPM×4 units — treating them as plain RPM made MQTT rpm ~4× high and max
/// demand ~4× low (shaft felt like ~10–25% at spd=35).
#[must_use]
pub fn demand_from_spd(spd: u16, spd_max: u16) -> u16 {
    if spd == 0 {
        return 0;
    }
    let max = if spd_max == 0 { 35 } else { spd_max };
    let spd = spd.min(max);
    let rpm = if max <= 1 {
        DEMAND_RPM_MAX
    } else {
        DEMAND_RPM_MIN
            + (u32::from(spd) - 1) * (DEMAND_RPM_MAX - DEMAND_RPM_MIN) / (u32::from(max) - 1)
    };
    u16::try_from(rpm.saturating_mul(4)).unwrap_or(u16::MAX)
}

/// Convert sensor RPM word (RPM×4) to display/MQTT RPM.
#[must_use]
pub const fn rpm_from_sensor(raw: u16) -> u16 {
    raw / 4
}

/// Build `GET_STATUS` request.
#[must_use]
#[allow(dead_code)] // unit-tested frame builder
pub fn build_get_status(addr: u8) -> Vec<u8> {
    build_request(addr, FC_GET_STATUS, &[PUMP_ACK_SEND])
}

/// Build `READ_SENSOR` request.
#[must_use]
#[allow(dead_code)] // unit-tested frame builder
pub fn build_read_sensor(addr: u8, sensor_id: u8) -> Vec<u8> {
    build_request(addr, FC_READ_SENSOR, &[PUMP_ACK_SEND, 0x00, sensor_id])
}

/// Build `SET_DEMAND` request (mode + demand as uint16 LE).
#[must_use]
#[allow(dead_code)] // unit-tested frame builder
pub fn build_set_demand(addr: u8, mode: u8, demand: u16) -> Vec<u8> {
    let lo = (demand & 0xFF) as u8;
    let hi = (demand >> 8) as u8;
    build_request(addr, FC_SET_DEMAND, &[PUMP_ACK_SEND, mode, lo, hi])
}

/// Build GO request.
#[must_use]
#[allow(dead_code)] // unit-tested frame builder
pub fn build_go(addr: u8) -> Vec<u8> {
    build_request(addr, FC_GO, &[PUMP_ACK_SEND])
}

/// Build STOP request.
#[must_use]
#[allow(dead_code)] // unit-tested frame builder
pub fn build_stop(addr: u8) -> Vec<u8> {
    build_request(addr, FC_STOP, &[PUMP_ACK_SEND])
}

fn check_ack(resp: &[u8], addr: u8, fc: u8) -> Result<(), RtuError> {
    if resp.len() < 5 {
        return Err(RtuError::Short(resp.len()));
    }
    if resp[0] != addr {
        return Err(RtuError::Unexpected(format!(
            "addr {:02X} != {:02X}",
            resp[0], addr
        )));
    }
    if resp[1] != fc {
        return Err(RtuError::Unexpected(format!(
            "fc {:02X} != {:02X}",
            resp[1], fc
        )));
    }
    if resp[2] != PUMP_ACK_RESP {
        return Err(RtuError::Unexpected(format!(
            "ack {:02X}, expected 10",
            resp[2]
        )));
    }
    Ok(())
}

/// Parse `GET_STATUS` response → pump status byte.
pub fn parse_status(resp: &[u8], addr: u8) -> Result<u8, RtuError> {
    check_ack(resp, addr, FC_GET_STATUS)?;
    if resp.len() < 6 {
        return Err(RtuError::Short(resp.len()));
    }
    Ok(resp[3])
}

/// Parse `READ_SENSOR` response → uint16 LE value.
///
/// Layout: `[addr, 0x45, 0x10, page, sensor_id, lo, hi, crc, crc]`
pub fn parse_sensor_le(resp: &[u8], addr: u8) -> Result<u16, RtuError> {
    check_ack(resp, addr, FC_READ_SENSOR)?;
    if resp.len() < 9 {
        return Err(RtuError::Short(resp.len()));
    }
    let lo = u16::from(resp[5]);
    let hi = u16::from(resp[6]);
    Ok(lo | (hi << 8))
}

fn pump_request(
    port: &mut dyn SerialPort,
    addr: u8,
    fc: u8,
    payload: &[u8],
    min_len: usize,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<Vec<u8>, RtuError> {
    let resp = request(port, addr, fc, payload, min_len, timeout, inter_frame)?;
    check_ack(&resp, addr, fc)?;
    Ok(resp)
}

pub fn get_status(
    port: &mut dyn SerialPort,
    addr: u8,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<u8, RtuError> {
    let resp = request(
        port,
        addr,
        FC_GET_STATUS,
        &[PUMP_ACK_SEND],
        6,
        timeout,
        inter_frame,
    )?;
    let status = parse_status(&resp, addr)?;
    debug!(addr, status, "pump GET_STATUS");
    Ok(status)
}

pub fn read_sensor(
    port: &mut dyn SerialPort,
    addr: u8,
    sensor_id: u8,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<u16, RtuError> {
    let resp = request(
        port,
        addr,
        FC_READ_SENSOR,
        &[PUMP_ACK_SEND, 0x00, sensor_id],
        9,
        timeout,
        inter_frame,
    )?;
    let value = parse_sensor_le(&resp, addr)?;
    debug!(addr, sensor_id, value, "pump READ_SENSOR");
    Ok(value)
}

pub fn set_demand(
    port: &mut dyn SerialPort,
    addr: u8,
    demand: u16,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<(), RtuError> {
    let lo = (demand & 0xFF) as u8;
    let hi = (demand >> 8) as u8;
    // Full frame also logged by rtu as "RTU TX"; this line is easy to grep.
    debug!(
        addr,
        demand,
        mode = DEMAND_MODE_SPEED,
        payload = format_args!("20 {:02X} {:02X} {:02X}", DEMAND_MODE_SPEED, lo, hi),
        "pump SET_DEMAND"
    );
    let _ = pump_request(
        port,
        addr,
        FC_SET_DEMAND,
        &[PUMP_ACK_SEND, DEMAND_MODE_SPEED, lo, hi],
        5,
        timeout,
        inter_frame,
    )?;
    Ok(())
}

pub fn go(
    port: &mut dyn SerialPort,
    addr: u8,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<(), RtuError> {
    debug!(addr, "pump GO");
    let _ = pump_request(port, addr, FC_GO, &[PUMP_ACK_SEND], 5, timeout, inter_frame)?;
    Ok(())
}

pub fn stop(
    port: &mut dyn SerialPort,
    addr: u8,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<(), RtuError> {
    debug!(addr, "pump STOP");
    let _ = pump_request(
        port,
        addr,
        FC_STOP,
        &[PUMP_ACK_SEND],
        5,
        timeout,
        inter_frame,
    )?;
    Ok(())
}

/// Apply demand: 0 → STOP; non-zero (target RPM) → set-demand + GO.
///
/// Each RTU step is best-effort: a timeout on `SET_DEMAND` still attempts GO/STOP.
/// Returns `true` if at least one actuation frame got a valid ACK.
pub fn apply_spd(
    port: &mut dyn SerialPort,
    addr: u8,
    demand: u16,
    pump_status: u8,
    force: bool,
    timeout: Duration,
    inter_frame: Duration,
) -> bool {
    let mut any_ok = false;
    if demand == 0 {
        if force || pump_status != STATUS_OFF {
            match set_demand(port, addr, 0, timeout, inter_frame) {
                Ok(()) => any_ok = true,
                Err(err) => debug!(error = %err, "pump SET_DEMAND(0) failed"),
            }
            std::thread::sleep(inter_frame);
            match stop(port, addr, timeout, inter_frame) {
                Ok(()) => any_ok = true,
                Err(err) => debug!(error = %err, "pump STOP failed"),
            }
        }
    } else if force || pump_status != STATUS_RUNNING {
        match set_demand(port, addr, demand, timeout, inter_frame) {
            Ok(()) => any_ok = true,
            Err(err) => debug!(error = %err, demand, "pump SET_DEMAND failed"),
        }
        std::thread::sleep(inter_frame);
        match go(port, addr, timeout, inter_frame) {
            Ok(()) => any_ok = true,
            Err(err) => debug!(error = %err, "pump GO failed"),
        }
    }
    any_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modbus::crc::{append_crc, check_crc};

    #[test]
    fn demand_from_spd_maps_rpm_times_four() {
        assert_eq!(demand_from_spd(0, 35), 0);
        // spd=1 → 600 RPM → demand 2400; spd=35 → 3450 RPM → demand 13800
        assert_eq!(demand_from_spd(1, 35), 2400);
        assert_eq!(demand_from_spd(35, 35), 13_800);
        // Mid: spd=25 → 2611 RPM → 10444
        assert_eq!(demand_from_spd(25, 35), 10_444);
        assert_eq!(demand_from_spd(10, 35), 5416);
        assert_eq!(demand_from_spd(20, 35), 8768);
    }

    #[test]
    fn rpm_from_sensor_divides_by_four() {
        assert_eq!(rpm_from_sensor(0), 0);
        assert_eq!(rpm_from_sensor(2400), 600);
        assert_eq!(rpm_from_sensor(5416), 1354);
        assert_eq!(rpm_from_sensor(13_800), 3450);
    }

    #[test]
    fn demand_frame_le_spd_10() {
        let frame = build_set_demand(0x15, DEMAND_MODE_SPEED, demand_from_spd(10, 35));
        // 1354 RPM × 4 = 5416 = 0x1528 LE
        assert_eq!(&frame[..6], &[0x15, 0x44, 0x20, 0x00, 0x28, 0x15]);
        assert!(check_crc(&frame));
    }

    #[test]
    fn demand_frame_spd_35() {
        let frame = build_set_demand(0x15, DEMAND_MODE_SPEED, demand_from_spd(35, 35));
        // 3450 RPM × 4 = 13800 = 0x35E8 LE
        assert_eq!(&frame[4..6], &[0xE8, 0x35]);
    }

    #[test]
    fn status_and_sensor_request_frames() {
        let st = build_get_status(0x15);
        assert_eq!(&st[..3], &[0x15, 0x43, 0x20]);
        assert!(check_crc(&st));
        let sens = build_read_sensor(0x15, SENSOR_RPM);
        assert_eq!(&sens[..5], &[0x15, 0x45, 0x20, 0x00, 0x00]);
        assert!(check_crc(&sens));
    }

    #[test]
    fn stop_and_go_frames() {
        let go = build_go(0x15);
        assert_eq!(&go[..3], &[0x15, 0x41, 0x20]);
        assert!(check_crc(&go));
        let stop = build_stop(0x15);
        assert_eq!(&stop[..3], &[0x15, 0x42, 0x20]);
        assert!(check_crc(&stop));
    }

    #[test]
    fn sensor_le_parse_fixes_esp_bug() {
        // page=0, sensor=RPM, value=0x1234 as LE → lo=0x34 hi=0x12
        let mut resp = vec![0x15, 0x45, 0x10, 0x00, SENSOR_RPM, 0x34, 0x12];
        append_crc(&mut resp);
        assert_eq!(parse_sensor_le(&resp, 0x15).unwrap(), 0x1234);
    }

    #[test]
    fn status_parse() {
        let mut resp = vec![0x15, 0x43, 0x10, STATUS_RUNNING];
        append_crc(&mut resp);
        assert_eq!(parse_status(&resp, 0x15).unwrap(), STATUS_RUNNING);
    }

    #[test]
    fn ack_mismatch_rejected() {
        let mut resp = vec![0x15, 0x43, 0x00, STATUS_OFF];
        append_crc(&mut resp);
        assert!(parse_status(&resp, 0x15).is_err());
    }
}

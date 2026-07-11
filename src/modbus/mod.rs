//! Sole RS485 Modbus RTU master: Waveshare relay (ESP-style FC01/FC05) + VS pump.

mod crc;
mod pump;
mod relay;
mod rtu;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serialport::SerialPort;
use tokio::sync::{Notify, RwLock, watch};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, info, warn};

use crate::config::ModbusConfig;
use crate::health::HealthState;
use crate::state::{PoolState, PublishHint, apply_publish_hint};

use self::rtu::{RtuError, open_port};

const WARN_INTERVAL: Duration = Duration::from_secs(30);
const OPEN_RETRY_SECS: u64 = 5;

/// Arguments for the Modbus background task.
pub struct ModbusTaskArgs {
    pub serial_device: String,
    pub serial_baud: u32,
    pub config: ModbusConfig,
    pub state: Arc<RwLock<PoolState>>,
    pub health: Arc<RwLock<HealthState>>,
    pub status_notify: Arc<Notify>,
    pub bus_notify: Arc<Notify>,
    pub shutdown_rx: watch::Receiver<bool>,
}

/// Fail-soft RS485 owner loop. Never returns `Err`; exits only on shutdown.
#[allow(clippy::too_many_lines)]
pub async fn modbus_task(args: ModbusTaskArgs) {
    let ModbusTaskArgs {
        serial_device,
        serial_baud,
        config,
        state,
        health,
        status_notify,
        bus_notify,
        mut shutdown_rx,
    } = args;

    if !config.enabled {
        info!("modbus disabled ([modbus].enabled = false)");
        return;
    }

    info!(
        device = %serial_device,
        baud = serial_baud,
        relay_addr = format_args!("0x{:02x}", config.relay_addr),
        pump_addr = format_args!("0x{:02x}", config.pump_addr),
        relay_poll_secs = config.relay_poll_secs,
        pump_poll_secs = config.pump_poll_secs,
        spd_max = config.spd_max,
        "modbus task starting (sole owner of RS485)"
    );

    // Mark hardware dirty so boot commanded state is pushed to the bus.
    {
        let mut guard = state.write().await;
        guard.update_relays = true;
        guard.update_pump = true;
    }
    bus_notify.notify_one();

    let timeout = Duration::from_millis(config.response_timeout_ms.max(100));
    let inter_frame = Duration::from_millis(config.inter_frame_ms.max(10));
    let relay_period = Duration::from_secs(config.relay_poll_secs.max(1));
    let pump_period = Duration::from_secs(config.pump_poll_secs.max(1));

    let mut relay_ticker = interval(relay_period);
    relay_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Consume immediate first tick; dirty+notify drives boot write after open.
    relay_ticker.tick().await;

    let mut pump_ticker = interval(pump_period);
    pump_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    pump_ticker.tick().await;

    let mut port: Option<Box<dyn SerialPort>> = None;
    let mut last_warn = Instant::now()
        .checked_sub(WARN_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut open_failures: u32 = 0;
    let mut boot_pending = true;

    loop {
        // Open once (fail-soft retry). Keep the handle across bus timeouts/errors;
        // only re-open if open itself never succeeded or failed.
        if port.is_none() {
            match open_port(&serial_device, serial_baud, timeout) {
                Ok(p) => {
                    info!(device = %serial_device, baud = serial_baud, "modbus serial open");
                    {
                        let mut guard = health.write().await;
                        guard.modbus_ok = true;
                    }
                    port = Some(p);
                    open_failures = 0;
                    if boot_pending {
                        boot_pending = false;
                        if let Some(p) = port.as_mut() {
                            service_bus(
                                p,
                                &config,
                                &state,
                                &status_notify,
                                timeout,
                                inter_frame,
                                true,
                                true,
                                &mut last_warn,
                            )
                            .await;
                        }
                    }
                }
                Err(err) => {
                    open_failures = open_failures.saturating_add(1);
                    {
                        let mut guard = health.write().await;
                        guard.modbus_ok = false;
                    }
                    if last_warn.elapsed() >= WARN_INTERVAL {
                        warn!(
                            device = %serial_device,
                            error = %err,
                            open_failures,
                            "modbus serial open failed; retrying"
                        );
                        last_warn = Instant::now();
                    }
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                debug!("modbus task stopping (open retry)");
                                return;
                            }
                        }
                        () = tokio::time::sleep(Duration::from_secs(OPEN_RETRY_SECS)) => {}
                    }
                    continue;
                }
            }
        }

        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    debug!("modbus task stopping");
                    break;
                }
            }
            () = bus_notify.notified() => {
                // MQTT dirtied hardware flags — service immediately.
                let (ur, up) = {
                    let guard = state.read().await;
                    (guard.update_relays, guard.update_pump)
                };
                if (ur || up)
                    && let Some(p) = port.as_mut()
                {
                    service_bus(
                        p,
                        &config,
                        &state,
                        &status_notify,
                        timeout,
                        inter_frame,
                        ur,
                        up,
                        &mut last_warn,
                    )
                    .await;
                }
            }
            _ = relay_ticker.tick() => {
                if let Some(p) = port.as_mut() {
                    service_bus(
                        p,
                        &config,
                        &state,
                        &status_notify,
                        timeout,
                        inter_frame,
                        true,
                        false,
                        &mut last_warn,
                    )
                    .await;
                }
            }
            _ = pump_ticker.tick() => {
                if let Some(p) = port.as_mut() {
                    service_bus(
                        p,
                        &config,
                        &state,
                        &status_notify,
                        timeout,
                        inter_frame,
                        false,
                        true,
                        &mut last_warn,
                    )
                    .await;
                }
            }
        }
    }
}

/// Poll/actuate relay and/or pump. Bus errors are rate-limited warns; port stays open.
#[allow(
    clippy::too_many_arguments,
    clippy::significant_drop_tightening,
    clippy::too_many_lines
)]
async fn service_bus(
    port: &mut Box<dyn SerialPort>,
    config: &ModbusConfig,
    state: &Arc<RwLock<PoolState>>,
    status_notify: &Notify,
    timeout: Duration,
    inter_frame: Duration,
    do_relay: bool,
    do_pump: bool,
    last_warn: &mut Instant,
) {
    let (update_relays, update_pump, commanded_relays, commanded_spd) = {
        let guard = state.read().await;
        (
            guard.update_relays,
            guard.update_pump,
            guard.commanded.relays,
            guard.commanded.set_speed,
        )
    };

    if do_relay {
        // Idle poll always reads; dirty flag triggers write-then-read.
        let write = update_relays;
        let result = tokio::task::block_in_place(|| {
            run_relay(
                port.as_mut(),
                config,
                commanded_relays,
                write,
                timeout,
                inter_frame,
            )
        });
        match result {
            Ok(measured) => {
                let hint = {
                    let mut guard = state.write().await;
                    let prev = guard.measured.relay_status;
                    guard.measured.relay_status = Some(measured);
                    if write {
                        // Clear dirty only when commanded still matches the snapshot
                        // we wrote and measured feedback agrees — else leave dirty.
                        if guard.commanded.relays == commanded_relays
                            && measured == commanded_relays
                        {
                            guard.update_relays = false;
                        }
                    }
                    if write || prev != Some(measured) {
                        PublishHint::Immediate
                    } else {
                        PublishHint::Silent
                    }
                };
                apply_publish_hint(status_notify, hint);
            }
            Err(err) => {
                rate_limited_bus_warn(last_warn, "relay", &err);
                // Keep update_relays dirty so next cycle retries.
            }
        }
    }

    if do_pump {
        let force = update_pump;
        let spd = commanded_spd.min(if config.spd_max == 0 {
            35
        } else {
            config.spd_max
        });
        let readings = tokio::task::block_in_place(|| {
            run_pump(port.as_mut(), config, spd, force, timeout, inter_frame)
        });
        let mut hint = PublishHint::Silent;
        {
            let mut guard = state.write().await;
            if force {
                // Clear dirty only after demand+GO/STOP got an ACK and commanded
                // still matches the snapshot we applied.
                if readings.actuated && guard.commanded.set_speed == commanded_spd {
                    guard.update_pump = false;
                }
            }
            if let Some(rpm) = readings.rpm {
                let _ = guard.set_rpm(rpm, PublishHint::Silent);
            }
            if let Some(watt) = readings.watt {
                let prev = guard.measured.watt;
                let h = guard.set_watt(watt, PublishHint::Immediate);
                if watt != prev {
                    hint = h;
                }
            }
            if let Some(air) = readings.air_temp {
                #[allow(clippy::cast_precision_loss)]
                let air_f = f64::from(air);
                if (guard.measured.air_temp - air_f).abs() >= 0.5 {
                    guard.measured.air_temp = air_f;
                    hint = PublishHint::Immediate;
                } else {
                    guard.measured.air_temp = air_f;
                }
            }
            if readings.actuated {
                hint = PublishHint::Immediate;
            }
        }
        apply_publish_hint(status_notify, hint);
    }
}

struct PumpReadings {
    rpm: Option<u16>,
    watt: Option<u16>,
    air_temp: Option<u16>,
    actuated: bool,
}

fn run_relay(
    port: &mut dyn SerialPort,
    config: &ModbusConfig,
    commanded: u8,
    write: bool,
    timeout: Duration,
    inter_frame: Duration,
) -> Result<u8, RtuError> {
    let addr = config.relay_addr;
    if write {
        let mut measured = relay::write_then_read(port, addr, commanded, timeout, inter_frame)?;
        if measured != commanded {
            warn!(
                commanded,
                measured, "relay measured ≠ commanded after write+read; retrying write once"
            );
            std::thread::sleep(inter_frame);
            measured = relay::write_then_read(port, addr, commanded, timeout, inter_frame)?;
            if measured != commanded {
                warn!(
                    commanded,
                    measured, "relay still mismatched after retry; status shows measured"
                );
            }
        }
        Ok(measured)
    } else {
        relay::read_coils(port, addr, timeout, inter_frame)
    }
}

/// One pump cycle: demand → status → sensors (best-effort each step).
///
/// While `spd > 0`, `SET_DEMAND`+GO is refreshed every poll (pump expects a
/// continuous demand loop). Dirty (`force`) also covers immediate spd changes
/// and stop (`spd == 0`).
fn run_pump(
    port: &mut dyn SerialPort,
    config: &ModbusConfig,
    spd: u16,
    force: bool,
    timeout: Duration,
    inter_frame: Duration,
) -> PumpReadings {
    let addr = config.pump_addr;
    let mut readings = PumpReadings {
        rpm: None,
        watt: None,
        air_temp: None,
        actuated: false,
    };
    let demand = pump::demand_from_spd(spd, config.spd_max);

    // Continuous demand while running, or immediate dirty actuate (incl. stop).
    if force || spd > 0 {
        if force {
            info!(spd, demand, "pump applying SET_DEMAND+GO|STOP (dirty)");
        } else {
            debug!(spd, demand, "pump refreshing SET_DEMAND+GO (continuous)");
        }
        if pump::apply_spd(port, addr, demand, 0xFF, true, timeout, inter_frame) {
            // Only mark actuated for dirty path so routine refresh does not
            // force an MQTT status publish every poll.
            if force {
                readings.actuated = true;
            }
            debug!(spd, demand, force, "pump demand/GO|STOP applied");
        } else {
            debug!(
                spd,
                demand, "pump actuation got no ACK (will retry next cycle)"
            );
        }
        std::thread::sleep(inter_frame);
    }

    let status = match pump::get_status(port, addr, timeout, inter_frame) {
        Ok(s) => Some(s),
        Err(err) => {
            debug!(error = %err, "pump GET_STATUS failed");
            None
        }
    };
    std::thread::sleep(inter_frame);

    match pump::read_sensor(port, addr, pump::SENSOR_SHAFT_WATTS, timeout, inter_frame) {
        Ok(v) => readings.watt = Some(v),
        Err(err) => debug!(error = %err, "pump watts read failed"),
    }
    std::thread::sleep(inter_frame);

    match pump::read_sensor(port, addr, pump::SENSOR_RPM, timeout, inter_frame) {
        Ok(v) => readings.rpm = Some(pump::rpm_from_sensor(v)),
        Err(err) => debug!(error = %err, "pump rpm read failed"),
    }
    std::thread::sleep(inter_frame);

    match pump::read_sensor(port, addr, pump::SENSOR_AMB_TEMP, timeout, inter_frame) {
        Ok(v) => readings.air_temp = Some(v),
        Err(err) => debug!(error = %err, "pump air temp read failed"),
    }
    std::thread::sleep(inter_frame);

    // Idle + spd==0: ensure STOP if status says still running (demand path above
    // only runs when force || spd>0).
    if !force && spd == 0 {
        let need_stop = status.is_none_or(|s| s != pump::STATUS_OFF);
        if need_stop {
            info!(status = ?status, "pump applying STOP (commanded off)");
            if pump::apply_spd(
                port,
                addr,
                0,
                status.unwrap_or(0xFF),
                true,
                timeout,
                inter_frame,
            ) {
                readings.actuated = true;
            }
        }
    }

    readings
}

fn rate_limited_bus_warn(last_warn: &mut Instant, what: &str, err: &RtuError) {
    if last_warn.elapsed() >= WARN_INTERVAL {
        warn!(error = %err, target = what, "modbus bus error");
        *last_warn = Instant::now();
    } else {
        debug!(error = %err, target = what, "modbus bus error (rate-limited)");
    }
}

/// Divert-valve mask: prefer measured coil feedback when present, else commanded.
#[must_use]
#[allow(dead_code)] // covered by unit test; temp uses PoolState::active_loop
pub fn divert_relays_for_temp(state: &PoolState) -> u8 {
    state
        .measured
        .relay_status
        .unwrap_or(state.commanded.relays)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modbus::crc::check_crc;
    use crate::modbus::pump;
    use crate::modbus::relay;

    #[test]
    fn reexports_frame_builders() {
        assert!(check_crc(&relay::build_read_coils(1)));
        assert!(check_crc(&pump::build_go(0x15)));
    }

    #[test]
    fn divert_prefers_measured() {
        let mut state = PoolState::default();
        state.commanded.relays = 0;
        assert_eq!(divert_relays_for_temp(&state), 0);
        state.measured.relay_status = Some(0b0100_0000); // r7
        assert_eq!(divert_relays_for_temp(&state), 0b0100_0000);
    }
}

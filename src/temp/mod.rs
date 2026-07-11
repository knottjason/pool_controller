//! ADS1115 + NTC water temperature task (shared sensor, r7 divert routing).

mod ads;
mod convert;

pub use ads::{AdsError, AdsErrorKind};
pub use convert::raw_to_fahrenheit;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, RwLock, watch};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, info, warn};

use crate::config::TempConfig;
use crate::health::HealthState;
use crate::state::{ActiveLoop, PoolState, PublishHint, apply_publish_hint};

use self::ads::{open_ads_reader, sample_average};

const WARN_INTERVAL: Duration = Duration::from_secs(30);

/// Arguments for the water-temp background task.
pub struct TempTaskArgs {
    pub config: TempConfig,
    pub state: Arc<RwLock<PoolState>>,
    pub health: Arc<RwLock<HealthState>>,
    pub status_notify: Arc<Notify>,
    pub shutdown_rx: watch::Receiver<bool>,
}

/// Fail-soft ADS1115 poll loop. Never returns `Err`; exits only on shutdown.
pub async fn temp_task(args: TempTaskArgs) {
    let TempTaskArgs {
        config,
        state,
        health,
        status_notify,
        mut shutdown_rx,
    } = args;

    info!(
        bus = %config.i2c_device,
        addr = format_args!("0x{:02x}", config.i2c_address),
        channel = config.channel,
        divert_relay = config.divert_relay,
        settle_secs = config.settle_secs,
        "temp task starting (r7 on=spa, r7 off=pool; settle on divert edges)"
    );

    let mut ticker = interval(Duration::from_secs(config.poll_interval_secs.max(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // First tick completes immediately; skip so we don't double-read at t=0 with settle.
    ticker.tick().await;

    let mut previous_loop: Option<ActiveLoop> = None;
    let mut settle_deadline: Option<Instant> = None;
    let mut faulted = false;
    let mut last_warn = Instant::now()
        .checked_sub(WARN_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_mode_warn = Instant::now()
        .checked_sub(WARN_INTERVAL)
        .unwrap_or_else(Instant::now);

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    debug!("temp task stopping");
                    break;
                }
            }
            _ = ticker.tick() => {
                poll_once(
                    &config,
                    &state,
                    &health,
                    &status_notify,
                    &mut previous_loop,
                    &mut settle_deadline,
                    &mut faulted,
                    &mut last_warn,
                    &mut last_mode_warn,
                ).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::significant_drop_tightening)]
async fn poll_once(
    config: &TempConfig,
    state: &Arc<RwLock<PoolState>>,
    health: &Arc<RwLock<HealthState>>,
    status_notify: &Notify,
    previous_loop: &mut Option<ActiveLoop>,
    settle_deadline: &mut Option<Instant>,
    faulted: &mut bool,
    last_warn: &mut Instant,
    last_mode_warn: &mut Instant,
) {
    // --- divert / settle bookkeeping ---
    let active = {
        let mut guard = state.write().await;
        if guard.measured.relay_status.is_none() {
            debug!(
                commanded = guard.commanded.relays,
                "temp divert using commanded relays (no Modbus feedback yet)"
            );
        }
        let active = guard.active_loop(config.divert_relay);
        let mode_loop = ActiveLoop::from_mode(guard.commanded.mode);

        if mode_loop != active && last_mode_warn.elapsed() >= WARN_INTERVAL {
            warn!(
                mode = guard.commanded.mode,
                ?active,
                divert_relay = config.divert_relay,
                "MQTT mode m disagrees with divert r7 active loop; sensor follows r7"
            );
            *last_mode_warn = Instant::now();
        }

        let loop_changed = previous_loop.is_none_or(|prev| prev != active);
        if loop_changed {
            let hint = guard.on_active_loop_changed(active);
            *settle_deadline =
                Some(Instant::now() + Duration::from_secs(config.settle_secs.max(1)));
            *previous_loop = Some(active);
            apply_publish_hint(status_notify, hint);
            debug!(
                ?active,
                settle_secs = config.settle_secs,
                "active loop changed; entering SETTLING"
            );
        }

        if let Some(deadline) = *settle_deadline
            && Instant::now() >= deadline
        {
            guard.end_settling();
            *settle_deadline = None;
            debug!(?active, "settle complete");
        }
        active
    };

    // --- ADC sample (blocking I/O off the async runtime) ---
    let cfg = config.clone();
    let sample_result = tokio::task::spawn_blocking(move || read_temp_f(&cfg)).await;

    match sample_result {
        Ok(Ok(temp_f)) => {
            if *faulted {
                info!(temp_f, "temp sensor recovered");
                *faulted = false;
            }
            {
                let mut guard = health.write().await;
                guard.temp_ok = true;
            }
            let hint = {
                let mut guard = state.write().await;
                // Re-check settle: deadline may have passed during blocking read.
                if let Some(deadline) = *settle_deadline
                    && Instant::now() >= deadline
                {
                    guard.end_settling();
                    *settle_deadline = None;
                }
                guard.set_water_temp_f(active, temp_f, config.publish_delta_f)
            };
            if hint == PublishHint::Immediate {
                debug!(?active, temp_f, "water temp updated (Immediate)");
            }
            apply_publish_hint(status_notify, hint);
        }
        Ok(Err(err)) => {
            handle_fault(
                state,
                health,
                status_notify,
                active,
                faulted,
                last_warn,
                &err.to_string(),
                err.kind,
            )
            .await;
        }
        Err(join_err) => {
            handle_fault(
                state,
                health,
                status_notify,
                active,
                faulted,
                last_warn,
                &format!("spawn_blocking join: {join_err}"),
                AdsErrorKind::I2c,
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_fault(
    state: &Arc<RwLock<PoolState>>,
    health: &Arc<RwLock<HealthState>>,
    status_notify: &Notify,
    active: ActiveLoop,
    faulted: &mut bool,
    last_warn: &mut Instant,
    message: &str,
    kind: AdsErrorKind,
) {
    let hint = {
        let mut guard = state.write().await;
        guard.clear_water_temp(active)
    };
    apply_publish_hint(status_notify, hint);

    *faulted = true;
    {
        let mut guard = health.write().await;
        guard.temp_ok = false;
    }
    if last_warn.elapsed() >= WARN_INTERVAL {
        warn!(?kind, error = %message, ?active, "temp sensor fault; active temp cleared");
        *last_warn = Instant::now();
    } else {
        debug!(?kind, error = %message, "temp sensor fault (rate-limited)");
    }
}

fn read_temp_f(config: &TempConfig) -> Result<f64, AdsError> {
    let mut reader = open_ads_reader(&config.i2c_device, config.i2c_address)?;
    let raw = sample_average(
        &mut *reader,
        config.channel,
        config.sample_count,
        Duration::from_millis(config.sample_delay_ms),
    )?;
    raw_to_fahrenheit(
        raw,
        config.series_ohms,
        config.thermistor_nominal_ohms,
        config.thermistor_b,
        config.thermistor_nominal_c,
        config.raw_min,
        config.raw_max,
        config.celsius_min,
        config.celsius_max,
    )
    .ok_or_else(|| AdsError {
        kind: AdsErrorKind::OutOfRange,
        message: format!("conversion rejected raw={raw}"),
    })
}

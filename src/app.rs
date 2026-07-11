//! Application runtime: tasks, channels, graceful shutdown.

use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, RwLock, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::health::HealthState;
use crate::http::{HttpTaskArgs, http_task};
use crate::log_buffer::LogBuffer;
use crate::modbus::{ModbusTaskArgs, modbus_task};
use crate::mqtt::{MqttTaskArgs, mqtt_task};
use crate::persist;
use crate::state::{Command, PoolState};
use crate::temp::{TempTaskArgs, temp_task};

pub struct App {
    config: Config,
    state: Arc<RwLock<PoolState>>,
    health: Arc<RwLock<HealthState>>,
    logs: Arc<Mutex<LogBuffer>>,
    status_notify: Arc<Notify>,
    bus_notify: Arc<Notify>,
}

impl App {
    #[must_use]
    pub fn new(config: Config, logs: Arc<Mutex<LogBuffer>>) -> Self {
        let mut pool_state = PoolState::default();
        persist::load_into(&mut pool_state, &config.persist.path);

        let health = HealthState::new(
            config.mqtt_enabled(),
            config.temp.enabled,
            config.modbus.enabled,
        );

        Self {
            config,
            state: Arc::new(RwLock::new(pool_state)),
            health: Arc::new(RwLock::new(health)),
            logs,
            status_notify: Arc::new(Notify::new()),
            bus_notify: Arc::new(Notify::new()),
        }
    }

    /// Run until SIGINT/SIGTERM or a `Command::Shutdown`.
    #[allow(clippy::too_many_lines)]
    pub async fn run(self) -> anyhow::Result<()> {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(32);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut shutdown_rx_supervisor = shutdown_rx.clone();

        let mut tasks = JoinSet::new();

        tasks.spawn(signal_task(shutdown_tx.clone()));

        if self.config.mqtt_enabled() {
            tasks.spawn(mqtt_task(MqttTaskArgs {
                mqtt: self.config.mqtt.clone(),
                status_interval_secs: self.config.timing.status_interval_secs,
                spd_max: self.config.modbus.spd_max,
                state: Arc::clone(&self.state),
                health: Arc::clone(&self.health),
                status_notify: Arc::clone(&self.status_notify),
                bus_notify: Arc::clone(&self.bus_notify),
                state_path: self.config.persist.path.clone(),
                shutdown_rx: shutdown_rx.clone(),
            }));
        } else {
            error!(
                "mqtt host empty; MQTT task not started (set [mqtt].host to enable — dual-controller cutover requires ESP offline first)"
            );
        }

        // Quiet local tick for scaffolding / future diagnostics.
        tasks.spawn(debug_heartbeat_task(
            self.config.timing.heartbeat_secs,
            shutdown_rx.clone(),
        ));

        if self.config.temp.enabled {
            tasks.spawn(temp_task(TempTaskArgs {
                config: self.config.temp.clone(),
                state: Arc::clone(&self.state),
                health: Arc::clone(&self.health),
                status_notify: Arc::clone(&self.status_notify),
                shutdown_rx: shutdown_rx.clone(),
            }));
        } else {
            info!("temp sensing disabled ([temp].enabled = false)");
        }

        if self.config.modbus.enabled {
            tasks.spawn(modbus_task(ModbusTaskArgs {
                serial_device: self.config.serial.device.clone(),
                serial_baud: self.config.serial.baud,
                config: self.config.modbus.clone(),
                state: Arc::clone(&self.state),
                health: Arc::clone(&self.health),
                status_notify: Arc::clone(&self.status_notify),
                bus_notify: Arc::clone(&self.bus_notify),
                shutdown_rx: shutdown_rx.clone(),
            }));
        } else {
            info!("modbus disabled ([modbus].enabled = false)");
        }

        if self.config.http.enabled {
            tasks.spawn(http_task(HttpTaskArgs {
                config: self.config.http.clone(),
                state: Arc::clone(&self.state),
                health: Arc::clone(&self.health),
                logs: Arc::clone(&self.logs),
                shutdown_rx: shutdown_rx.clone(),
            }));
        } else {
            info!("http dashboard disabled ([http].enabled = false)");
        }

        // Keep a command sender alive for future local control tasks.
        let _cmd_tx = cmd_tx;

        info!(
            serial = %self.config.serial.device,
            baud = self.config.serial.baud,
            mqtt_enabled = self.config.mqtt_enabled(),
            mqtt_host = %self.config.mqtt.host,
            status_interval_secs = self.config.timing.status_interval_secs,
            persist = %self.config.persist.path.display(),
            temp_enabled = self.config.temp.enabled,
            modbus_enabled = self.config.modbus.enabled,
            http_enabled = self.config.http.enabled,
            "runtime ready"
        );

        loop {
            tokio::select! {
                biased;

                changed = shutdown_rx_supervisor.changed() => {
                    if changed.is_err() || *shutdown_rx_supervisor.borrow() {
                        break;
                    }
                }

                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(Command::Ping) => {
                            debug!("received ping");
                        }
                        Some(Command::Shutdown) => {
                            info!("shutdown command received");
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                        None => {
                            warn!("command channel closed");
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                    }
                }

                Some(joined) = tasks.join_next() => {
                    match joined {
                        Ok(()) => {}
                        Err(err) if err.is_cancelled() => {}
                        Err(err) => {
                            error!(error = %err, "background task failed");
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                    }
                }
            }
        }

        let _ = shutdown_tx.send(true);

        while let Some(joined) = tasks.join_next().await {
            if let Err(err) = joined
                && !err.is_cancelled()
            {
                warn!(error = %err, "task join error during shutdown");
            }
        }

        info!("shutdown complete");
        Ok(())
    }
}

async fn signal_task(shutdown_tx: watch::Sender<bool>) {
    match wait_for_shutdown_signal().await {
        Ok(()) => {
            info!("OS shutdown signal received");
            let _ = shutdown_tx.send(true);
        }
        Err(err) => {
            error!(error = %err, "failed to install signal handlers");
            let _ = shutdown_tx.send(true);
        }
    }
}

async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Quiet local tick (debug-only heartbeat).
async fn debug_heartbeat_task(period_secs: u64, mut shutdown_rx: watch::Receiver<bool>) {
    let mut ticker = interval(Duration::from_secs(period_secs.max(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tick: u64 = 0;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    debug!("debug heartbeat stopping");
                    break;
                }
            }
            _ = ticker.tick() => {
                tick = tick.saturating_add(1);
                debug!(tick, "heartbeat");
            }
        }
    }
}

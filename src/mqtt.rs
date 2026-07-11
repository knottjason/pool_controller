//! MQTT task: `pool/command` / `pool/status` / `pool/connected`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS, Transport};
use tokio::sync::{Notify, RwLock, watch};
use tracing::{debug, error, info, warn};

use crate::config::MqttConfig;
use crate::health::HealthState;
use crate::persist;
use crate::state::PoolState;

const COALESCE_MS: u64 = 75;
const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 30_000;
/// Reject oversized `pool/command` payloads before JSON parse (~4 KiB).
const MAX_COMMAND_PAYLOAD_BYTES: usize = 4096;

/// Best-effort local IPv4 for status `ip` (empty if unknown).
#[must_use]
pub fn local_ipv4() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            // Destination need not be reachable; OS picks the egress interface.
            socket.connect("1.1.1.1:80")?;
            socket.local_addr().map(|addr| addr.ip().to_string())
        })
        .unwrap_or_default()
}

/// Parse a `pool/command` payload into a JSON value.
pub fn parse_command_payload(payload: &[u8]) -> Result<serde_json::Value, String> {
    if payload.len() > MAX_COMMAND_PAYLOAD_BYTES {
        return Err(format!(
            "command payload too large ({} > {MAX_COMMAND_PAYLOAD_BYTES} bytes)",
            payload.len()
        ));
    }
    serde_json::from_slice(payload).map_err(|err| err.to_string())
}

pub struct MqttTaskArgs {
    pub mqtt: MqttConfig,
    pub status_interval_secs: u64,
    /// Max MQTT `spd` demand (from `[modbus].spd_max`).
    pub spd_max: u16,
    pub state: Arc<RwLock<PoolState>>,
    pub health: Arc<RwLock<HealthState>>,
    pub status_notify: Arc<Notify>,
    /// Wake the Modbus task when `update_pump` / `update_relays` are set.
    pub bus_notify: Arc<Notify>,
    pub state_path: PathBuf,
    pub shutdown_rx: watch::Receiver<bool>,
}

/// Run the MQTT client until shutdown. Reconnects with exponential backoff.
pub async fn mqtt_task(args: MqttTaskArgs) {
    let MqttTaskArgs {
        mqtt,
        status_interval_secs,
        spd_max,
        state,
        health,
        status_notify,
        bus_notify,
        state_path,
        mut shutdown_rx,
    } = args;

    {
        let mut guard = state.write().await;
        if guard.measured.ip.is_empty() {
            guard.measured.ip = local_ipv4();
        }
    }

    let mut backoff_ms = RECONNECT_BASE_MS;

    loop {
        if *shutdown_rx.borrow() {
            debug!("mqtt task stopping before connect");
            break;
        }

        info!(
            host = %mqtt.host,
            port = mqtt.port,
            client_id = %mqtt.client_id,
            "mqtt connecting"
        );

        match run_session(
            &mqtt,
            status_interval_secs,
            spd_max,
            Arc::clone(&state),
            Arc::clone(&health),
            Arc::clone(&status_notify),
            Arc::clone(&bus_notify),
            state_path.clone(),
            shutdown_rx.clone(),
        )
        .await
        {
            SessionOutcome::Shutdown => break,
            SessionOutcome::Disconnected { ever_connected } => {
                {
                    let mut guard = health.write().await;
                    guard.mqtt_connected = false;
                }
                warn!(
                    backoff_ms,
                    ever_connected, "mqtt disconnected; reconnecting"
                );
                // Successful ConnAck resets backoff so a later drop starts from base again.
                if ever_connected {
                    backoff_ms = RECONNECT_BASE_MS;
                }
            }
        }

        if *shutdown_rx.borrow() {
            break;
        }

        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            () = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
        }

        backoff_ms = (backoff_ms.saturating_mul(2)).min(RECONNECT_MAX_MS);
    }

    info!("mqtt task stopped");
}

enum SessionOutcome {
    Shutdown,
    Disconnected { ever_connected: bool },
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // select! event loop is intentionally flat
async fn run_session(
    mqtt: &MqttConfig,
    status_interval_secs: u64,
    spd_max: u16,
    state: Arc<RwLock<PoolState>>,
    health: Arc<RwLock<HealthState>>,
    status_notify: Arc<Notify>,
    bus_notify: Arc<Notify>,
    state_path: PathBuf,
    mut shutdown_rx: watch::Receiver<bool>,
) -> SessionOutcome {
    let mut options = MqttOptions::new(&mqtt.client_id, &mqtt.host, mqtt.port);
    options.set_keep_alive(Duration::from_secs(10));
    options.set_clean_session(true);
    options.set_transport(Transport::Tcp);
    options.set_last_will(LastWill::new(
        mqtt.connected_topic.clone(),
        "0",
        QoS::AtLeastOnce,
        true,
    ));

    let (client, mut eventloop) = AsyncClient::new(options, 64);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(status_interval_secs.max(1)));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick; ConnAck path publishes status on connect.
    heartbeat.tick().await;

    let mut connected = false;
    let mut last_status_payload = String::new();

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    if connected {
                        let _ = client
                            .publish(&mqtt.connected_topic, QoS::AtLeastOnce, true, "0")
                            .await;
                    }
                    return SessionOutcome::Shutdown;
                }
            }

            event = eventloop.poll() => {
                if let Some(outcome) = handle_mqtt_event(
                    event,
                    &client,
                    mqtt,
                    spd_max,
                    &state,
                    &health,
                    &state_path,
                    &bus_notify,
                    &mut connected,
                    &mut last_status_payload,
                )
                .await
                {
                    return outcome;
                }
            }

            () = status_notify.notified() => {
                // Coalesce bursty multi-field updates into one publish.
                tokio::time::sleep(Duration::from_millis(COALESCE_MS)).await;
                if connected
                    && let Err(err) = publish_connected_and_status(
                        &client,
                        mqtt,
                        &state,
                        &mut last_status_payload,
                        false,
                    )
                    .await
                {
                    warn!(error = %err, "coalesced status publish failed");
                }
            }

            _ = heartbeat.tick() => {
                if connected
                    && let Err(err) = publish_connected_and_status(
                        &client,
                        mqtt,
                        &state,
                        &mut last_status_payload,
                        true,
                    )
                    .await
                {
                    warn!(error = %err, "heartbeat status publish failed");
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_mqtt_event(
    event: Result<Event, rumqttc::ConnectionError>,
    client: &AsyncClient,
    mqtt: &MqttConfig,
    spd_max: u16,
    state: &Arc<RwLock<PoolState>>,
    health: &Arc<RwLock<HealthState>>,
    state_path: &Path,
    bus_notify: &Notify,
    connected: &mut bool,
    last_status_payload: &mut String,
) -> Option<SessionOutcome> {
    match event {
        Ok(Event::Incoming(Incoming::ConnAck(_))) => {
            info!("mqtt connected");
            *connected = true;
            {
                let mut guard = health.write().await;
                guard.mqtt_connected = true;
            }
            if let Err(err) = client
                .subscribe(&mqtt.command_topic, QoS::AtLeastOnce)
                .await
            {
                error!(error = %err, "mqtt subscribe failed");
                {
                    let mut guard = health.write().await;
                    guard.mqtt_connected = false;
                }
                return Some(SessionOutcome::Disconnected {
                    ever_connected: true,
                });
            }
            if let Err(err) =
                publish_connected_and_status(client, mqtt, state, last_status_payload, true).await
            {
                warn!(error = %err, "initial status publish failed");
            }
            None
        }
        Ok(Event::Incoming(Incoming::Publish(publish))) => {
            if publish.topic != mqtt.command_topic {
                return None;
            }
            match handle_command(&publish.payload, state, state_path, bus_notify, spd_max).await {
                // Echo status only when commanded state actually changed.
                // HA often re-publishes identical spd/relays; forcing publish floods the bus.
                Ok(true) => {
                    if let Err(err) =
                        publish_connected_and_status(client, mqtt, state, last_status_payload, true)
                            .await
                    {
                        warn!(error = %err, "status publish after command failed");
                    }
                }
                Ok(false) => {
                    debug!("ignored noop pool/command (no commanded change)");
                }
                Err(err) => {
                    warn!(error = %err, "rejected pool/command");
                }
            }
            None
        }
        Ok(Event::Incoming(Incoming::Disconnect)) => {
            warn!("mqtt broker disconnect");
            Some(SessionOutcome::Disconnected {
                ever_connected: *connected,
            })
        }
        Ok(_) => None,
        Err(err) => {
            warn!(error = %err, "mqtt event loop error");
            Some(SessionOutcome::Disconnected {
                ever_connected: *connected,
            })
        }
    }
}

/// Apply command to a draft, persist when changed, then commit to shared state.
///
/// On persist failure the shared state is left unchanged and `Err` is returned
/// (caller must not publish status as success).
async fn handle_command(
    payload: &[u8],
    state: &Arc<RwLock<PoolState>>,
    state_path: &Path,
    bus_notify: &Notify,
    spd_max: u16,
) -> Result<bool, String> {
    let value = parse_command_payload(payload)?;

    if let Some(spd) = value.get("spd") {
        info!(raw = %spd, "pool/command includes spd");
    }

    let mut draft = {
        let guard = state.read().await;
        guard.clone()
    };
    let changed = draft.apply_command_json(&value, spd_max)?;

    if changed {
        if let Err(err) = persist::save(&draft, state_path) {
            error!(
                error = %err,
                path = %state_path.display(),
                "failed to persist commanded state; leaving in-memory state unchanged"
            );
            return Err(format!("persist failed: {err}"));
        }
        debug!(path = %state_path.display(), "persisted commanded state");
    }

    let update_pump = draft.update_pump;
    let update_relays = draft.update_relays;
    let set_speed = draft.commanded.set_speed;

    {
        let mut guard = state.write().await;
        // Commit commanded fields; only SET dirty flags (never clear — Modbus owns clear).
        guard.commanded = draft.commanded;
        guard.update_pump |= draft.update_pump;
        guard.update_relays |= draft.update_relays;
    }

    if update_pump || update_relays {
        bus_notify.notify_one();
    }

    if update_pump && changed {
        debug!(
            spd = set_speed,
            update_pump, changed, "pump command committed to shared state"
        );
    }

    Ok(changed)
}

async fn publish_connected_and_status(
    client: &AsyncClient,
    mqtt: &MqttConfig,
    state: &Arc<RwLock<PoolState>>,
    last_status_payload: &mut String,
    force: bool,
) -> anyhow::Result<()> {
    let (payload, pump_dbg) = {
        let guard = state.read().await;
        let payload = guard.status_payload()?;
        let pump_dbg = (
            guard.commanded.set_speed,
            guard.measured.rpm,
            guard.measured.watt,
            guard.measured.air_temp,
            guard.update_pump,
        );
        drop(guard);
        (payload, pump_dbg)
    };

    // Skip duplicate snapshots unless forced (heartbeat always publishes for HA freshness / RPM).
    if force || payload != *last_status_payload {
        client
            .publish(
                &mqtt.status_topic,
                QoS::AtLeastOnce,
                false,
                payload.as_bytes(),
            )
            .await?;
        *last_status_payload = payload;
        let (spd, rpm, watt, at, update_pump) = pump_dbg;
        debug!(
            topic = %mqtt.status_topic,
            spd,
            rpm,
            watt,
            at,
            update_pump,
            "published pool/status (pump fields)"
        );
    }

    client
        .publish(&mqtt.connected_topic, QoS::AtLeastOnce, true, "1")
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DEFAULT_SPD_MAX, PoolState, encode_spd};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::RwLock;

    fn temp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rs_pool_mqtt_{label}_{nanos}.json"))
    }

    #[test]
    fn parse_command_partial_json() {
        let raw = br#"{"m":0,"spt":72,"r1":1}"#;
        let value = parse_command_payload(raw).unwrap();
        let mut state = PoolState::default();
        assert!(state.apply_command_json(&value, DEFAULT_SPD_MAX).unwrap());
        assert_eq!(state.commanded.set_pool_temp, 72);
        assert_eq!(state.commanded.mode, 0);
        assert_eq!(state.commanded.relays & 1, 1);
    }

    #[test]
    fn parse_command_rejects_non_object() {
        let value = parse_command_payload(b"[1,2]").unwrap();
        let mut state = PoolState::default();
        assert!(state.apply_command_json(&value, DEFAULT_SPD_MAX).is_err());
    }

    #[test]
    fn parse_command_rejects_oversized_payload() {
        let mut raw = br#"{"m":0,"pad":""#.to_vec();
        raw.extend(std::iter::repeat_n(b'x', MAX_COMMAND_PAYLOAD_BYTES));
        raw.extend_from_slice(br#""}"#);
        assert!(raw.len() > MAX_COMMAND_PAYLOAD_BYTES);
        let err = parse_command_payload(&raw).unwrap_err();
        assert!(err.contains("too large"), "unexpected error: {err}");
    }

    #[test]
    fn status_payload_includes_encoded_spd() {
        let mut state = PoolState::default();
        state.commanded.set_speed = encode_spd(25, DEFAULT_SPD_MAX);
        let payload = state.status_payload().unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["spd"], json!(encode_spd(25, DEFAULT_SPD_MAX)));
    }

    #[tokio::test]
    async fn handle_command_noop_still_succeeds() {
        let path = temp_path("noop");
        let _ = std::fs::remove_file(&path);
        let state = Arc::new(RwLock::new(PoolState::default()));
        let bus_notify = Arc::new(tokio::sync::Notify::new());
        let changed = handle_command(
            br#"{"spt":70}"#,
            &state,
            &path,
            &bus_notify,
            DEFAULT_SPD_MAX,
        )
        .await
        .expect("noop command must succeed");
        assert!(!changed);
        assert!(!path.exists(), "noop must not write persist file");
    }

    #[tokio::test]
    async fn handle_command_persist_failure_leaves_state_unchanged() {
        let path = temp_path("bad_persist_dir");
        // Point at a non-writable nested path under a file so create_dir_all fails.
        let blocker = temp_path("blocker_file");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let bad_path = blocker.join("state.json");

        let state = Arc::new(RwLock::new(PoolState::default()));
        let bus_notify = Arc::new(tokio::sync::Notify::new());
        assert_eq!(state.read().await.commanded.mode, 0);

        let err = handle_command(
            br#"{"m":1}"#,
            &state,
            &bad_path,
            &bus_notify,
            DEFAULT_SPD_MAX,
        )
        .await
        .expect_err("persist failure must return Err");
        assert!(err.contains("persist failed"), "unexpected error: {err}");

        assert_eq!(
            state.read().await.commanded.mode,
            0,
            "in-memory state must not commit when persist fails"
        );

        let _ = std::fs::remove_file(&blocker);
        let _ = std::fs::remove_file(&path);
    }
}

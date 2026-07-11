//! Shared subsystem health for the HTTP status dashboard.

use std::time::Instant;

use serde_json::{Value, json};

/// Live health flags for MQTT / temp / Modbus (not published on `pool/status`).
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // intentional flat health snapshot for the dashboard API
pub struct HealthState {
    pub mqtt_enabled: bool,
    pub mqtt_connected: bool,
    pub temp_enabled: bool,
    pub temp_ok: bool,
    pub modbus_enabled: bool,
    pub modbus_ok: bool,
    pub started_at: Instant,
}

impl HealthState {
    #[must_use]
    pub fn new(mqtt_enabled: bool, temp_enabled: bool, modbus_enabled: bool) -> Self {
        Self {
            mqtt_enabled,
            mqtt_connected: false,
            temp_enabled,
            temp_ok: false,
            modbus_enabled,
            modbus_ok: false,
            started_at: Instant::now(),
        }
    }

    /// JSON for `GET /api/health` (includes uptime).
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "mqtt_enabled": self.mqtt_enabled,
            "mqtt_connected": self.mqtt_connected,
            "temp_enabled": self.temp_enabled,
            "temp_ok": self.temp_ok,
            "modbus_enabled": self.modbus_enabled,
            "modbus_ok": self.modbus_ok,
            "uptime_secs": self.started_at.elapsed().as_secs(),
        })
    }
}

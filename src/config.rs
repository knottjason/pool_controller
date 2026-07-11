//! Runtime configuration loaded from TOML.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

const DEFAULT_CONFIG_PATH: &str = "/etc/rs_pool/config.toml";
const DEFAULT_STATE_PATH: &str = "/var/lib/rs_pool/state.json";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub serial: SerialConfig,
    pub mqtt: MqttConfig,
    pub timing: TimingConfig,
    #[serde(default)]
    pub persist: PersistConfig,
    /// ADS1115 + NTC water temp. Missing section uses defaults (enabled).
    #[serde(default)]
    pub temp: TempConfig,
    /// RS485 Modbus master (relay + pump). Missing section uses defaults.
    #[serde(default)]
    pub modbus: ModbusConfig,
    /// HTTPS status dashboard. Missing section uses defaults (enabled).
    #[serde(default)]
    pub http: HttpConfig,
}

/// Read-only HTTPS status dashboard (Basic auth + self-signed TLS).
#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_http_enabled")]
    pub enabled: bool,
    /// Plain HTTP listener (redirect-only).
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
    /// TLS listener for the dashboard.
    #[serde(default = "default_https_bind")]
    pub https_bind: String,
    #[serde(default = "default_cert_path")]
    pub cert_path: PathBuf,
    #[serde(default = "default_key_path")]
    pub key_path: PathBuf,
    /// `web:<bcrypt-hash>` file (mode 600 on device).
    #[serde(default = "default_auth_path")]
    pub auth_path: PathBuf,
    /// In-process tracing ring buffer size for `GET /api/logs`.
    #[serde(default = "default_log_buffer_lines")]
    pub log_buffer_lines: usize,
}

const fn default_http_enabled() -> bool {
    true
}
fn default_http_bind() -> String {
    "0.0.0.0:80".into()
}
fn default_https_bind() -> String {
    "0.0.0.0:443".into()
}
fn default_cert_path() -> PathBuf {
    PathBuf::from("/etc/rs_pool/tls/cert.pem")
}
fn default_key_path() -> PathBuf {
    PathBuf::from("/etc/rs_pool/tls/key.pem")
}
fn default_auth_path() -> PathBuf {
    PathBuf::from("/etc/rs_pool/http_auth")
}
const fn default_log_buffer_lines() -> usize {
    500
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: default_http_enabled(),
            http_bind: default_http_bind(),
            https_bind: default_https_bind(),
            cert_path: default_cert_path(),
            key_path: default_key_path(),
            auth_path: default_auth_path(),
            log_buffer_lines: default_log_buffer_lines(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SerialConfig {
    pub device: String,
    pub baud: u32,
}

/// RS485 Modbus RTU master settings (Waveshare relay + VS pump).
#[derive(Debug, Clone, Deserialize)]
pub struct ModbusConfig {
    #[serde(default = "default_modbus_enabled")]
    pub enabled: bool,
    /// Waveshare relay slave address (default 0x01).
    #[serde(default = "default_relay_addr")]
    pub relay_addr: u8,
    /// Pump slave address (default 0x15).
    #[serde(default = "default_pump_addr")]
    pub pump_addr: u8,
    /// Idle coil-read interval (seconds).
    #[serde(default = "default_relay_poll_secs")]
    pub relay_poll_secs: u64,
    /// Pump status/sensor poll interval (seconds).
    #[serde(default = "default_pump_poll_secs")]
    pub pump_poll_secs: u64,
    /// Per-transaction response timeout (milliseconds).
    #[serde(default = "default_response_timeout_ms")]
    pub response_timeout_ms: u64,
    /// Inter-frame gap between requests (milliseconds).
    #[serde(default = "default_inter_frame_ms")]
    pub inter_frame_ms: u64,
    /// Max MQTT/persist `spd` demand (default 35); used by encode/clamp.
    #[serde(default = "default_spd_max")]
    pub spd_max: u16,
}

const fn default_modbus_enabled() -> bool {
    true
}
const fn default_relay_addr() -> u8 {
    0x01
}
const fn default_pump_addr() -> u8 {
    0x15
}
const fn default_relay_poll_secs() -> u64 {
    30
}
const fn default_pump_poll_secs() -> u64 {
    8
}
const fn default_response_timeout_ms() -> u64 {
    // RS485 slaves should answer quickly; 1s is already generous.
    1000
}
const fn default_inter_frame_ms() -> u64 {
    50
}
const fn default_spd_max() -> u16 {
    35
}

impl Default for ModbusConfig {
    fn default() -> Self {
        Self {
            enabled: default_modbus_enabled(),
            relay_addr: default_relay_addr(),
            pump_addr: default_pump_addr(),
            relay_poll_secs: default_relay_poll_secs(),
            pump_poll_secs: default_pump_poll_secs(),
            response_timeout_ms: default_response_timeout_ms(),
            inter_frame_ms: default_inter_frame_ms(),
            spd_max: default_spd_max(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub client_id: String,
    pub command_topic: String,
    pub status_topic: String,
    pub connected_topic: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimingConfig {
    /// Legacy scaffold tick (debug-only heartbeat when MQTT is enabled).
    pub heartbeat_secs: u64,
    /// MQTT `pool/status` heartbeat interval (seconds).
    #[serde(default = "default_status_interval_secs")]
    pub status_interval_secs: u64,
}

const fn default_status_interval_secs() -> u64 {
    4
}

#[derive(Debug, Clone, Deserialize)]
pub struct PersistConfig {
    /// Path for commanded-settings JSON (atomic write on each command apply).
    #[serde(default = "default_persist_path")]
    pub path: PathBuf,
}

fn default_persist_path() -> PathBuf {
    PathBuf::from(DEFAULT_STATE_PATH)
}

impl Default for PersistConfig {
    fn default() -> Self {
        Self {
            path: default_persist_path(),
        }
    }
}

/// ADS1115 + NTC water-temperature sensing.
#[derive(Debug, Clone, Deserialize)]
pub struct TempConfig {
    #[serde(default = "default_temp_enabled")]
    pub enabled: bool,
    #[serde(default = "default_i2c_device")]
    pub i2c_device: String,
    #[serde(default = "default_i2c_address")]
    pub i2c_address: u8,
    /// Single-ended channel: 0=A0 … 3=A3.
    #[serde(default)]
    pub channel: u8,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_sample_count")]
    pub sample_count: u8,
    #[serde(default = "default_sample_delay_ms")]
    pub sample_delay_ms: u64,
    #[serde(default = "default_series_ohms")]
    pub series_ohms: f64,
    #[serde(default = "default_thermistor_nominal_ohms")]
    pub thermistor_nominal_ohms: f64,
    #[serde(default = "default_thermistor_b")]
    pub thermistor_b: f64,
    #[serde(default = "default_thermistor_nominal_c")]
    pub thermistor_nominal_c: f64,
    #[serde(default = "default_publish_delta_f")]
    pub publish_delta_f: f64,
    /// Seconds to wait after divert-valve (r7) change before trusting the reading.
    #[serde(default = "default_settle_secs")]
    pub settle_secs: u64,
    /// Divert valve relay number (1-based). On → spa; off → pool. Locked default: 7.
    #[serde(default = "default_divert_relay")]
    pub divert_relay: u8,
    #[serde(default = "default_raw_min")]
    pub raw_min: i16,
    #[serde(default = "default_raw_max")]
    pub raw_max: i16,
    #[serde(default = "default_celsius_min")]
    pub celsius_min: f64,
    #[serde(default = "default_celsius_max")]
    pub celsius_max: f64,
}

const fn default_temp_enabled() -> bool {
    true
}
fn default_i2c_device() -> String {
    "/dev/i2c-1".into()
}
const fn default_i2c_address() -> u8 {
    0x48
}
const fn default_poll_interval_secs() -> u64 {
    4
}
const fn default_sample_count() -> u8 {
    5
}
const fn default_sample_delay_ms() -> u64 {
    10
}
const fn default_series_ohms() -> f64 {
    10_000.0
}
const fn default_thermistor_nominal_ohms() -> f64 {
    10_500.0
}
const fn default_thermistor_b() -> f64 {
    3950.0
}
const fn default_thermistor_nominal_c() -> f64 {
    25.0
}
const fn default_publish_delta_f() -> f64 {
    0.1
}
const fn default_settle_secs() -> u64 {
    90
}
const fn default_divert_relay() -> u8 {
    7
}
const fn default_raw_min() -> i16 {
    80
}
const fn default_raw_max() -> i16 {
    32_600
}
const fn default_celsius_min() -> f64 {
    -20.0
}
const fn default_celsius_max() -> f64 {
    60.0
}

impl Default for TempConfig {
    fn default() -> Self {
        Self {
            enabled: default_temp_enabled(),
            i2c_device: default_i2c_device(),
            i2c_address: default_i2c_address(),
            channel: 0,
            poll_interval_secs: default_poll_interval_secs(),
            sample_count: default_sample_count(),
            sample_delay_ms: default_sample_delay_ms(),
            series_ohms: default_series_ohms(),
            thermistor_nominal_ohms: default_thermistor_nominal_ohms(),
            thermistor_b: default_thermistor_b(),
            thermistor_nominal_c: default_thermistor_nominal_c(),
            publish_delta_f: default_publish_delta_f(),
            settle_secs: default_settle_secs(),
            divert_relay: default_divert_relay(),
            raw_min: default_raw_min(),
            raw_max: default_raw_max(),
            celsius_min: default_celsius_min(),
            celsius_max: default_celsius_max(),
        }
    }
}

impl Config {
    /// Resolve path: `RS_POOL_CONFIG` env, else `/etc/rs_pool/config.toml`.
    pub fn path() -> PathBuf {
        std::env::var_os("RS_POOL_CONFIG")
            .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from)
    }

    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(Self::path())
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// `true` when the error is a missing config file (dev-friendly defaults OK).
    #[must_use]
    pub fn is_not_found(err: &ConfigError) -> bool {
        matches!(
            err,
            ConfigError::Read { source, .. } if source.kind() == std::io::ErrorKind::NotFound
        )
    }

    #[must_use]
    pub fn mqtt_enabled(&self) -> bool {
        !self.mqtt.host.trim().is_empty()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            serial: SerialConfig {
                device: "/dev/serial0".into(),
                baud: 9600,
            },
            mqtt: MqttConfig {
                host: String::new(),
                port: 1883,
                client_id: "rs_pool".into(),
                command_topic: "pool/command".into(),
                status_topic: "pool/status".into(),
                connected_topic: "pool/connected".into(),
            },
            timing: TimingConfig {
                heartbeat_secs: 5,
                status_interval_secs: default_status_interval_secs(),
            },
            persist: PersistConfig::default(),
            temp: TempConfig::default(),
            modbus: ModbusConfig::default(),
            http: HttpConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_temp_section_uses_defaults() {
        let raw = r#"
[serial]
device = "/dev/serial0"
baud = 9600
[mqtt]
host = ""
port = 1883
client_id = "rs_pool"
command_topic = "pool/command"
status_topic = "pool/status"
connected_topic = "pool/connected"
[timing]
heartbeat_secs = 5
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.temp.enabled);
        assert_eq!(cfg.temp.i2c_address, 0x48);
        assert_eq!(cfg.temp.divert_relay, 7);
        assert_eq!(cfg.temp.settle_secs, 90);
        assert!((cfg.temp.thermistor_nominal_ohms - 10_500.0).abs() < f64::EPSILON);
        assert!(cfg.modbus.enabled);
        assert_eq!(cfg.modbus.relay_addr, 0x01);
        assert_eq!(cfg.modbus.pump_addr, 0x15);
        assert_eq!(cfg.modbus.relay_poll_secs, 30);
        assert_eq!(cfg.modbus.pump_poll_secs, 8);
        assert_eq!(cfg.modbus.spd_max, 35);
        assert!(cfg.http.enabled);
        assert_eq!(cfg.http.http_bind, "0.0.0.0:80");
        assert_eq!(cfg.http.https_bind, "0.0.0.0:443");
        assert_eq!(cfg.http.log_buffer_lines, 500);
    }
}

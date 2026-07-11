//! Shared runtime state and cross-task messages.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Notify;
use tracing::debug;

/// Software status schema version published as `v`.
pub const STATUS_VERSION: i32 = 1;

/// Whether a state mutation should wake the MQTT status publisher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // used by future Modbus/temp writers + unit tests
pub enum PublishHint {
    /// Wake MQTT to publish soon (commanded / measured changes other than RPM).
    Immediate,
    /// Mutate only; next heartbeat (or other Immediate publish) carries the value.
    Silent,
}

/// Notify the MQTT task when `hint` is [`PublishHint::Immediate`].
#[allow(dead_code)] // used by future Modbus/temp writers + unit tests
pub fn apply_publish_hint(notify: &Notify, hint: PublishHint) {
    if hint == PublishHint::Immediate {
        notify.notify_one();
    }
}

/// Commands that will eventually arrive from MQTT / local control.
#[derive(Debug, Clone)]
#[expect(dead_code)] // constructed by MQTT/control tasks once wired
pub enum Command {
    /// Placeholder until real command schema is ported.
    Ping,
    Shutdown,
}

/// Default max MQTT/persist `spd` demand (matches `[modbus].spd_max` default).
pub const DEFAULT_SPD_MAX: u16 = 35;

/// Encode MQTT `spd` (0..=`spd_max` demand scale) into the commanded speed word.
///
/// - `< 1` → `0` (off)
/// - `1..=spd_max` → value
/// - `> spd_max` → `spd_max`
#[must_use]
pub fn encode_spd(spd: i32, spd_max: u16) -> u16 {
    let max = if spd_max == 0 {
        DEFAULT_SPD_MAX
    } else {
        spd_max
    };
    if spd < 1 {
        0
    } else {
        let spd_u = u16::try_from(spd).unwrap_or(u16::MAX);
        spd_u.min(max)
    }
}

/// Identity reverse of [`encode_spd`] for diagnostics (already 0..=`spd_max`).
#[must_use]
#[allow(dead_code)]
pub fn decode_spd(word: u16, spd_max: u16) -> i32 {
    let max = if spd_max == 0 {
        DEFAULT_SPD_MAX
    } else {
        spd_max
    };
    i32::from(word.min(max))
}

/// Migrate a persisted speed word from the old ESP `×655` encoding to 0..=35.
///
/// Values already in range (`<= 35`) are kept; legacy words (`> 35`) are
/// divided by 655 and clamped to 35.
#[must_use]
pub const fn migrate_spd(word: u16) -> u16 {
    if word > 35 {
        let scaled = word / 655;
        if scaled > 35 { 35 } else { scaled }
    } else {
        word
    }
}

/// Pack commanded relay bits `r1` (LSB) … `r8` (MSB) into a byte.
#[must_use]
pub fn pack_relays(bits: [bool; 8]) -> u8 {
    bits.iter()
        .enumerate()
        .fold(0_u8, |acc, (i, on)| if *on { acc | (1 << i) } else { acc })
}

/// Unpack a relay command byte into `r1`…`r8`.
#[must_use]
pub fn unpack_relays(byte: u8) -> [bool; 8] {
    std::array::from_fn(|i| (byte & (1 << i)) != 0)
}

/// Commanded settings that survive reboot (persisted to disk).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandedState {
    /// Mode: `0` = pool, non-zero = spa (ESP `m`).
    pub mode: i32,
    /// Internal speed demand 0..=35 (MQTT `spd`).
    pub set_speed: u16,
    pub set_spa_temp: i32,
    pub set_pool_temp: i32,
    /// Commanded relay bits packed as `r1` LSB … `r8` MSB.
    pub relays: u8,
}

impl Default for CommandedState {
    fn default() -> Self {
        Self {
            mode: 0,
            set_speed: 0,
            set_spa_temp: 104,
            set_pool_temp: 70,
            relays: 0,
        }
    }
}

/// Live measurements (not persisted).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeasuredState {
    pub rpm: u16,
    pub watt: u16,
    pub air_temp: f64,
    /// Spa water temp (°F). `None` = invalid / settling / no sensor.
    pub spa_temp: Option<f64>,
    /// Pool water temp (°F). `None` = invalid / settling / no sensor.
    pub pool_temp: Option<f64>,
    /// Feedback relay bits from bus (`None` until first successful Modbus read).
    /// Status `r1`–`r8` come from this only — never from commanded.
    pub relay_status: Option<u8>,
    pub ip: String,
}

/// Which hydraulic loop the shared NTC currently samples (from divert valve r7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveLoop {
    Pool,
    Spa,
}

impl ActiveLoop {
    /// Derive active loop from commanded relays: divert on → spa, off → pool.
    ///
    /// `divert_relay` is 1-based (`7` = r7).
    #[must_use]
    pub fn from_relays(relays: u8, divert_relay: u8) -> Self {
        let bits = unpack_relays(relays);
        let idx = usize::from(divert_relay.saturating_sub(1)).min(7);
        if bits[idx] { Self::Spa } else { Self::Pool }
    }

    /// MQTT mode `m`: 0 = pool, non-zero = spa.
    #[must_use]
    pub const fn from_mode(mode: i32) -> Self {
        if mode == 0 { Self::Pool } else { Self::Spa }
    }
}

/// In-process pool controller state (replaces the ESP global `Mq` hub).
#[derive(Debug, Clone, Default)]
pub struct PoolState {
    pub commanded: CommandedState,
    pub measured: MeasuredState,
    /// Dirty flag for Modbus pump write.
    pub update_pump: bool,
    /// Dirty flag for Modbus relay write.
    pub update_relays: bool,
    /// True while waiting for water to circulate after a divert-valve (r7) change.
    /// Future heat (`r6`) must not turn on while settling or when active temp is `None`.
    pub water_temp_settling: bool,
}

impl PoolState {
    /// Apply a partial MQTT command JSON object. Only present keys are updated.
    ///
    /// Validates the full object on a draft clone and commits only on success, so a
    /// mid-object parse error leaves the live state unchanged.
    ///
    /// Returns `true` if any commanded field value changed. Dirty flags
    /// (`update_pump` / `update_relays`) are set (`|=`) on real value changes
    /// (and on relay key presence for write-retry). Never cleared here.
    pub fn apply_command_json(&mut self, value: &Value, spd_max: u16) -> Result<bool, String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "command payload must be a JSON object".to_string())?;

        let mut draft = self.clone();
        let mut changed = false;

        if let Some(v) = obj.get("m") {
            let mode = json_as_i32(v, "m")?;
            if draft.commanded.mode != mode {
                draft.commanded.mode = mode;
                changed = true;
            }
        }

        if let Some(v) = obj.get("spd") {
            let pct = json_as_i32(v, "spd")?;
            let encoded = encode_spd(pct, spd_max);
            let prev_encoded = draft.commanded.set_speed;
            let value_changed = prev_encoded != encoded;
            // Only dirty on real change — idle pump poll refreshes demand continuously.
            if value_changed {
                draft.commanded.set_speed = encoded;
                draft.update_pump = true;
                changed = true;
            }
            debug!(
                raw = %v,
                spd = pct,
                encoded,
                prev_encoded,
                value_changed,
                update_pump = value_changed,
                "pump command: spd applied"
            );
        }

        if let Some(v) = obj.get("sst") {
            let sst = json_as_i32(v, "sst")?;
            if draft.commanded.set_spa_temp != sst {
                draft.commanded.set_spa_temp = sst;
                changed = true;
            }
        }

        if let Some(v) = obj.get("spt") {
            let spt = json_as_i32(v, "spt")?;
            if draft.commanded.set_pool_temp != spt {
                draft.commanded.set_pool_temp = spt;
                changed = true;
            }
        }

        // `v` is accepted for ESP parity (remote version) but not stored in PoolState.
        let _ = obj.get("v");

        let mut relays = unpack_relays(draft.commanded.relays);
        let mut relays_key_present = false;
        let mut relays_value_changed = false;
        for (idx, key) in ["r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8"]
            .into_iter()
            .enumerate()
        {
            if let Some(v) = obj.get(key) {
                relays_key_present = true;
                let on = json_as_boolish(v, key)?;
                if relays[idx] != on {
                    relays[idx] = on;
                    relays_value_changed = true;
                }
            }
        }
        if relays_key_present {
            // Key presence marks relays dirty for Modbus even on a noop value.
            draft.update_relays = true;
            if relays_value_changed {
                draft.commanded.relays = pack_relays(relays);
                changed = true;
            }
        }

        self.commanded = draft.commanded;
        // Only SET dirty flags — never clear (Modbus clears after a matching write).
        self.update_pump |= draft.update_pump;
        self.update_relays |= draft.update_relays;
        Ok(changed)
    }

    /// Update measured RPM. Always silent (no immediate MQTT publish).
    #[allow(dead_code)] // used by future Modbus task + unit tests
    pub const fn set_rpm(&mut self, rpm: u16, hint: PublishHint) -> PublishHint {
        self.measured.rpm = rpm;
        // RPM is noisy; callers should pass Silent. Force Silent regardless.
        let _ = hint;
        PublishHint::Silent
    }

    /// Update a publish-worthy measured field (watts, temps, etc.).
    #[allow(dead_code)] // used by future Modbus task + unit tests
    pub const fn set_watt(&mut self, watt: u16, hint: PublishHint) -> PublishHint {
        self.measured.watt = watt;
        hint
    }

    /// Active hydraulic loop from divert valve (`divert_relay`, 1-based).
    ///
    /// Prefers **measured** relay feedback when available; falls back to commanded.
    #[must_use]
    pub fn active_loop(&self, divert_relay: u8) -> ActiveLoop {
        let relays = self.measured.relay_status.unwrap_or(self.commanded.relays);
        ActiveLoop::from_relays(relays, divert_relay)
    }

    /// Active-loop water temp (°F), or `None` if invalid/settling-cleared.
    #[must_use]
    #[allow(dead_code)] // future heat (r6) gate + unit tests
    pub const fn active_water_temp(&self, loop_: ActiveLoop) -> Option<f64> {
        match loop_ {
            ActiveLoop::Spa => self.measured.spa_temp,
            ActiveLoop::Pool => self.measured.pool_temp,
        }
    }

    /// Future heat gate: `r6` must not turn on when this is false.
    ///
    /// Requires a valid active-loop temp **and** not settling after a divert change.
    #[must_use]
    #[allow(dead_code)] // future heat (r6) must call this before enabling
    pub const fn water_temp_ok(&self, loop_: ActiveLoop) -> bool {
        !self.water_temp_settling && self.active_water_temp(loop_).is_some()
    }

    /// Enter SETTLING after an r7 / active-loop change: clear the **new** active field.
    ///
    /// Inactive loop retains its last good reading. Returns [`PublishHint::Immediate`].
    pub const fn on_active_loop_changed(&mut self, new_loop: ActiveLoop) -> PublishHint {
        self.water_temp_settling = true;
        match new_loop {
            ActiveLoop::Spa => self.measured.spa_temp = None,
            ActiveLoop::Pool => self.measured.pool_temp = None,
        }
        PublishHint::Immediate
    }

    /// Apply a good °F sample to the active loop (no-op write while settling).
    ///
    /// Returns [`PublishHint::Immediate`] when validity flips to Some or `|Δ| >= publish_delta_f`.
    pub fn set_water_temp_f(
        &mut self,
        loop_: ActiveLoop,
        temp_f: f64,
        publish_delta_f: f64,
    ) -> PublishHint {
        if self.water_temp_settling {
            return PublishHint::Silent;
        }
        let slot = match loop_ {
            ActiveLoop::Spa => &mut self.measured.spa_temp,
            ActiveLoop::Pool => &mut self.measured.pool_temp,
        };
        match *slot {
            None => {
                *slot = Some(temp_f);
                PublishHint::Immediate
            }
            Some(prev) if (prev - temp_f).abs() >= publish_delta_f => {
                *slot = Some(temp_f);
                PublishHint::Immediate
            }
            Some(_) => {
                *slot = Some(temp_f);
                PublishHint::Silent
            }
        }
    }

    /// Clear the active-loop temp (I2C / conversion fault). Immediate if it was `Some`.
    pub const fn clear_water_temp(&mut self, loop_: ActiveLoop) -> PublishHint {
        let slot = match loop_ {
            ActiveLoop::Spa => &mut self.measured.spa_temp,
            ActiveLoop::Pool => &mut self.measured.pool_temp,
        };
        if slot.is_some() {
            *slot = None;
            PublishHint::Immediate
        } else {
            PublishHint::Silent
        }
    }

    /// Mark settle complete so the next good sample may populate the active field.
    pub const fn end_settling(&mut self) {
        self.water_temp_settling = false;
    }

    /// Build the MQTT `pool/status` JSON object (ESP key set).
    ///
    /// `r1`–`r8` come **only** from [`MeasuredState::relay_status`]; JSON `null`
    /// until the first successful Modbus coil read.
    #[must_use]
    pub fn to_status_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("ip".into(), Value::String(self.measured.ip.clone()));
        map.insert("rpm".into(), Value::from(self.measured.rpm));
        map.insert("spd".into(), Value::from(self.commanded.set_speed));
        map.insert("watt".into(), Value::from(self.measured.watt));
        map.insert("m".into(), Value::from(self.commanded.mode));
        map.insert("st".into(), json_opt_number(self.measured.spa_temp));
        map.insert("pt".into(), json_opt_number(self.measured.pool_temp));
        map.insert("sst".into(), Value::from(self.commanded.set_spa_temp));
        map.insert("spt".into(), Value::from(self.commanded.set_pool_temp));
        map.insert("v".into(), Value::from(STATUS_VERSION));
        map.insert("at".into(), json_number(self.measured.air_temp));
        match self.measured.relay_status {
            Some(byte) => {
                let relays = unpack_relays(byte);
                for (i, on) in relays.iter().enumerate() {
                    map.insert(format!("r{}", i + 1), Value::from(u8::from(*on)));
                }
            }
            None => {
                for i in 1..=8 {
                    map.insert(format!("r{i}"), Value::Null);
                }
            }
        }
        Value::Object(map)
    }

    /// Serialize status to a compact JSON string.
    pub fn status_payload(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.to_status_json())
    }
}

fn json_number(v: f64) -> Value {
    serde_json::Number::from_f64(v).map_or_else(|| Value::from(0), Value::Number)
}

fn json_opt_number(v: Option<f64>) -> Value {
    v.map_or(Value::Null, json_number)
}

fn json_as_i32(value: &Value, key: &str) -> Result<i32, String> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .or_else(|| {
                n.as_f64().map(|f| {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        f as i32
                    }
                })
            })
            .ok_or_else(|| format!("{key} out of range")),
        Value::Bool(b) => Ok(i32::from(*b)),
        Value::String(s) => s
            .parse::<i32>()
            .map_err(|_| format!("{key} must be an integer")),
        _ => Err(format!("{key} must be an integer")),
    }
}

fn json_as_boolish(value: &Value, key: &str) -> Result<bool, String> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => {
            let v = n
                .as_i64()
                .or_else(|| {
                    n.as_f64().map(|f| {
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            f as i64
                        }
                    })
                })
                .ok_or_else(|| format!("{key} out of range"))?;
            Ok(v != 0)
        }
        Value::String(s) => match s.as_str() {
            "1" | "true" | "True" | "TRUE" => Ok(true),
            "0" | "false" | "False" | "FALSE" => Ok(false),
            _ => Err(format!("{key} must be 0/1")),
        },
        _ => Err(format!("{key} must be 0/1")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spd_encode_boundaries() {
        assert_eq!(encode_spd(0, DEFAULT_SPD_MAX), 0);
        assert_eq!(encode_spd(1, DEFAULT_SPD_MAX), 1);
        assert_eq!(encode_spd(10, DEFAULT_SPD_MAX), 10);
        assert_eq!(encode_spd(35, DEFAULT_SPD_MAX), 35);
        assert_eq!(encode_spd(36, DEFAULT_SPD_MAX), 35);
        assert_eq!(encode_spd(100, DEFAULT_SPD_MAX), 35);
        assert_eq!(encode_spd(-1, DEFAULT_SPD_MAX), 0);
        assert_eq!(encode_spd(20, 10), 10);
    }

    #[test]
    fn spd_decode_roundtrip() {
        for spd in [0, 1, 10, 35] {
            assert_eq!(
                decode_spd(encode_spd(spd, DEFAULT_SPD_MAX), DEFAULT_SPD_MAX),
                spd
            );
        }
    }

    #[test]
    fn migrate_spd_divides_legacy_words() {
        assert_eq!(migrate_spd(0), 0);
        assert_eq!(migrate_spd(10), 10);
        assert_eq!(migrate_spd(35), 35);
        // Barely over range: not a valid ×655 word → maps to 0, not clamp-to-35.
        assert_eq!(migrate_spd(36), 0);
        assert_eq!(migrate_spd(655), 1);
        assert_eq!(migrate_spd(6_550), 10);
        assert_eq!(migrate_spd(32_750), 35); // old ×655 midrange → 49.min(35)
        assert_eq!(migrate_spd(0xFFFF), 35);
    }

    #[test]
    fn relay_pack_unpack() {
        let bits = [true, false, true, false, false, true, false, true];
        let packed = pack_relays(bits);
        assert_eq!(packed, 0b1010_0101);
        assert_eq!(unpack_relays(packed), bits);
    }

    #[test]
    fn partial_command_only_present_keys() {
        let mut state = PoolState::default();
        assert_eq!(state.commanded.set_pool_temp, 70);
        assert_eq!(state.commanded.set_spa_temp, 104);
        assert_eq!(state.commanded.mode, 0);

        let changed = state
            .apply_command_json(&json!({"spt": 72, "r1": 1}), DEFAULT_SPD_MAX)
            .unwrap();
        assert!(changed);
        assert_eq!(state.commanded.set_pool_temp, 72);
        assert_eq!(state.commanded.set_spa_temp, 104);
        assert_eq!(state.commanded.mode, 0);
        assert!(unpack_relays(state.commanded.relays)[0]);
        assert!(!unpack_relays(state.commanded.relays)[1]);
        assert!(state.update_relays);
        assert!(!state.update_pump);
    }

    #[test]
    fn partial_command_spd_sets_update_pump() {
        let mut state = PoolState::default();
        let changed = state
            .apply_command_json(&json!({"spd": 10, "m": 1}), DEFAULT_SPD_MAX)
            .unwrap();
        assert!(changed);
        assert_eq!(state.commanded.set_speed, encode_spd(10, DEFAULT_SPD_MAX));
        assert_eq!(state.commanded.mode, 1);
        assert!(state.update_pump);
    }

    #[test]
    fn mid_object_failure_leaves_state_unchanged() {
        let mut state = PoolState::default();
        state.commanded.mode = 0;
        state.commanded.set_pool_temp = 70;
        let err = state
            .apply_command_json(&json!({"m": 1, "r1": "nope"}), DEFAULT_SPD_MAX)
            .unwrap_err();
        assert!(err.contains("r1"), "unexpected error: {err}");
        assert_eq!(
            state.commanded.mode, 0,
            "mode must not commit on mid-object failure"
        );
        assert_eq!(state.commanded.set_pool_temp, 70);
        assert!(!state.update_relays);
        assert!(!state.update_pump);
    }

    #[test]
    fn spd_noop_does_not_set_dirty() {
        let mut state = PoolState::default();
        state.commanded.set_speed = encode_spd(10, DEFAULT_SPD_MAX);
        let changed = state
            .apply_command_json(&json!({"spd": 10}), DEFAULT_SPD_MAX)
            .unwrap();
        assert!(!changed);
        assert!(
            !state.update_pump,
            "identical spd must not dirty; continuous poll refreshes demand"
        );
        assert_eq!(state.commanded.set_speed, encode_spd(10, DEFAULT_SPD_MAX));
    }

    #[test]
    fn relay_key_sets_dirty_even_if_unchanged() {
        let mut state = PoolState::default();
        state.commanded.relays =
            pack_relays([true, false, false, false, false, false, false, false]);
        let changed = state
            .apply_command_json(&json!({"r1": 1}), DEFAULT_SPD_MAX)
            .unwrap();
        assert!(!changed);
        assert!(state.update_relays);
        assert!(unpack_relays(state.commanded.relays)[0]);
    }

    #[test]
    fn apply_command_never_clears_dirty_flags() {
        let mut state = PoolState {
            update_pump: true,
            update_relays: true,
            ..Default::default()
        };
        let changed = state
            .apply_command_json(&json!({"spt": 71}), DEFAULT_SPD_MAX)
            .unwrap();
        assert!(changed);
        assert!(
            state.update_pump,
            "relay/temp-only command must not clear update_pump"
        );
        assert!(
            state.update_relays,
            "temp-only command must not clear update_relays"
        );
    }

    #[test]
    fn publish_hint_rpm_is_silent() {
        let mut state = PoolState::default();
        let hint = state.set_rpm(2400, PublishHint::Immediate);
        assert_eq!(hint, PublishHint::Silent);
        assert_eq!(state.measured.rpm, 2400);

        let hint = state.set_watt(900, PublishHint::Immediate);
        assert_eq!(hint, PublishHint::Immediate);
    }

    #[tokio::test]
    async fn apply_publish_hint_immediate_vs_silent() {
        let notify = Notify::new();

        apply_publish_hint(&notify, PublishHint::Silent);
        let silent =
            tokio::time::timeout(std::time::Duration::from_millis(30), notify.notified()).await;
        assert!(silent.is_err(), "Silent must not wake waiters");

        apply_publish_hint(&notify, PublishHint::Immediate);
        tokio::time::timeout(std::time::Duration::from_millis(30), notify.notified())
            .await
            .expect("Immediate must wake waiters");
    }

    #[test]
    fn status_json_shape_and_keys() {
        let mut state = PoolState::default();
        state.commanded.set_speed = encode_spd(25, DEFAULT_SPD_MAX);
        state.commanded.relays =
            pack_relays([true, false, false, false, false, false, false, false]);
        // Commanded relays must NOT appear in status until measured is set.
        state.measured.ip = "192.168.1.10".into();
        state.measured.rpm = 1234;

        let status = state.to_status_json();
        let obj = status.as_object().unwrap();
        for key in [
            "ip", "rpm", "spd", "watt", "m", "st", "pt", "sst", "spt", "v", "at", "r1", "r2", "r3",
            "r4", "r5", "r6", "r7", "r8",
        ] {
            assert!(obj.contains_key(key), "missing key {key}");
        }
        assert_eq!(obj["spd"], json!(25));
        assert!(obj["r1"].is_null(), "r1 null before first Modbus read");
        assert!(obj["r2"].is_null());
        assert_eq!(obj["v"], json!(STATUS_VERSION));
        assert_eq!(obj["ip"], json!("192.168.1.10"));
        assert_eq!(obj["rpm"], json!(1234));
        assert_eq!(obj["spt"], json!(70));
        assert_eq!(obj["sst"], json!(104));
        assert!(obj["st"].is_null(), "default spa_temp must be JSON null");
        assert!(obj["pt"].is_null(), "default pool_temp must be JSON null");
    }

    #[test]
    fn status_json_relays_from_measured_only() {
        let mut state = PoolState::default();
        state.commanded.relays =
            pack_relays([true, false, false, false, false, false, false, false]);
        // Before read: null
        let status = state.to_status_json();
        assert!(status["r1"].is_null());
        // After measured read with different mask than commanded:
        state.measured.relay_status = Some(pack_relays([
            false, true, false, false, false, false, true, false,
        ]));
        let status = state.to_status_json();
        assert_eq!(status["r1"], json!(0), "must not echo commanded r1");
        assert_eq!(status["r2"], json!(1));
        assert_eq!(status["r7"], json!(1));
        assert_eq!(status["r8"], json!(0));
    }

    #[test]
    fn active_loop_follows_measured_r7_when_present() {
        let mut state = PoolState::default();
        state.commanded.mode = 1; // spa mode commanded
        // No measured yet → commanded r7 off → pool
        assert_eq!(state.active_loop(7), ActiveLoop::Pool);
        state.commanded.relays =
            pack_relays([false, false, false, false, false, false, true, false]);
        assert_eq!(state.active_loop(7), ActiveLoop::Spa);
        // Measured overrides commanded: r7 off while commanded on
        state.measured.relay_status = Some(0);
        assert_eq!(state.active_loop(7), ActiveLoop::Pool);
        state.measured.relay_status = Some(pack_relays([
            false, false, false, false, false, false, true, false,
        ]));
        assert_eq!(state.active_loop(7), ActiveLoop::Spa);
    }

    #[test]
    fn r7_switch_clears_active_retains_inactive() {
        let mut state = PoolState::default();
        state.measured.pool_temp = Some(78.0);
        state.measured.spa_temp = Some(102.0);

        let hint = state.on_active_loop_changed(ActiveLoop::Spa);
        assert_eq!(hint, PublishHint::Immediate);
        assert!(state.water_temp_settling);
        assert!(state.measured.spa_temp.is_none());
        assert_eq!(state.measured.spa_temp, None);
        assert_eq!(state.measured.pool_temp, Some(78.0));

        let status = state.to_status_json();
        assert!(status["st"].is_null());
        assert_eq!(status["pt"], json!(78.0));
        assert!(!state.water_temp_ok(ActiveLoop::Spa));
    }

    #[test]
    fn settle_blocks_writes_then_allows() {
        let mut state = PoolState::default();
        state.on_active_loop_changed(ActiveLoop::Pool);
        let hint = state.set_water_temp_f(ActiveLoop::Pool, 75.0, 0.1);
        assert_eq!(hint, PublishHint::Silent);
        assert!(state.measured.pool_temp.is_none());

        state.end_settling();
        let hint = state.set_water_temp_f(ActiveLoop::Pool, 75.0, 0.1);
        assert_eq!(hint, PublishHint::Immediate);
        assert_eq!(state.measured.pool_temp, Some(75.0));
        assert!(state.water_temp_ok(ActiveLoop::Pool));
    }

    #[test]
    fn publish_hysteresis_and_clear() {
        let mut state = PoolState::default();
        state.end_settling();
        assert_eq!(
            state.set_water_temp_f(ActiveLoop::Pool, 70.0, 0.1),
            PublishHint::Immediate
        );
        assert_eq!(
            state.set_water_temp_f(ActiveLoop::Pool, 70.05, 0.1),
            PublishHint::Silent
        );
        assert_eq!(
            state.set_water_temp_f(ActiveLoop::Pool, 70.2, 0.1),
            PublishHint::Immediate
        );
        assert_eq!(
            state.clear_water_temp(ActiveLoop::Pool),
            PublishHint::Immediate
        );
        assert!(state.measured.pool_temp.is_none());
        assert_eq!(
            state.clear_water_temp(ActiveLoop::Pool),
            PublishHint::Silent
        );
        let status = state.to_status_json();
        assert!(status["pt"].is_null());
    }

    #[test]
    fn apply_command_noop_when_unchanged() {
        let mut state = PoolState::default();
        let changed = state
            .apply_command_json(&json!({"spt": 70}), DEFAULT_SPD_MAX)
            .unwrap();
        assert!(!changed);
    }
}

# Agent notes — rs_pool

Site-specific host / SSH user / MQTT broker live in **`AGENTS.local.md`** (gitignored). Read that file when present; do not invent IPs or hostnames.

## Deploy + verify loop

After code changes that should run on the Pi:

```bash
./deploy/deploy.sh <pi-host>
# or
make deploy HOST=<pi-host>
# user@host form also works; default SSH user is `pi` unless deploy/local.env sets RS_POOL_DEPLOY_USER
```

Then confirm the service is healthy:

```bash
ssh <user>@<pi-host> 'sudo systemctl --no-pager --full status rs-pool'
ssh <user>@<pi-host> 'sudo journalctl -u rs-pool -n 50 --no-pager'
```

Follow logs while iterating:

```bash
ssh <user>@<pi-host> 'sudo journalctl -u rs-pool -f'
```

Skip rebuild when only redeploying an existing binary:

```bash
./deploy/deploy.sh <pi-host> --skip-build
```

## Logging

- App uses `tracing` + `RUST_LOG` (default `info`).
- Level is controlled by `/etc/rs_pool/env` on the Pi (from `deploy/rs-pool.env`).
- Journal retention is capped by `deploy/journald-rs-pool.conf` → `/etc/systemd/journald.conf.d/99-rs-pool.conf`.
- Recent in-process lines are also available on the HTTPS dashboard (`GET /api/logs`).

### Modbus / pump / relay debug

Targeted filter:

```bash
ssh <user>@<pi-host> 'sudo sed -i "s/^RUST_LOG=.*/RUST_LOG=info,rs_pool::modbus=debug/" /etc/rs_pool/env && sudo systemctl restart rs-pool'
```

Look for journal lines like:

- `modbus serial open`
- `modbus task starting`
- `relay write coils (FC05 x8)` / `relay write single coil` / `relay read coils`
- `pump GET_STATUS` / `SET_DEMAND` / `GO` / `STOP`
- `modbus bus error` (rate-limited; fail-soft)

Dial back to `RUST_LOG=info` when done debugging.

Change level without a full redeploy:

```bash
ssh <user>@<pi-host> 'sudo sed -i "s/^RUST_LOG=.*/RUST_LOG=debug/" /etc/rs_pool/env && sudo systemctl restart rs-pool'
```

Or edit `deploy/rs-pool.env` locally and redeploy.

## Config

- Runtime TOML: `/etc/rs_pool/config.toml` (seeded once from `deploy/rs-pool.toml`; not overwritten on later deploys).
- Example always updated: `/etc/rs_pool/config.toml.example`
- Override path with `RS_POOL_CONFIG`.
- MQTT enabled when `[mqtt].host` is non-empty (template placeholder: `mqtt.local` — set your broker). Empty host disables MQTT (logged at error); only intentional for local/dev.
- If the config file exists but fails to parse, the process exits nonzero (no silent defaults). Defaults apply only when the file is missing.
- Status heartbeat: `[timing].status_interval_secs` (default `4`); commands publish `pool/status` immediately (including noops).
- Modbus: `[modbus]` (defaults if missing). `enabled`, relay/pump addrs, poll intervals, `spd_max = 35`.
- HTTP dashboard: `[http]` (defaults if missing). `enabled`, `http_bind` / `https_bind`, cert/key/auth paths, `log_buffer_lines`.

## HTTPS status dashboard

Read-only LAN UI (no MQTT commands over HTTP):

| Listener | Behavior |
|----------|----------|
| `:80` | Permanent redirect to HTTPS |
| `:443` | TLS (self-signed) + HTTP Basic auth |

- URL: `https://<pi-host>/` (accept the self-signed warning)
- User: **`web`**
- Password: local gitignored file `deploy/http_web.password` (deploy hashes it to `/etc/rs_pool/http_auth`; plaintext never copied to the Pi)
- Certs: `/etc/rs_pool/tls/{cert,key}.pem` — deploy seeds once if missing (`CN=<deploy host>`); later deploys leave them alone
- Routes: `GET /`, `/api/status`, `/api/health`, `/api/logs`
- Fail-soft: bind/cert/auth load failure logs and exits the HTTP task only

```bash
# after deploy (PASSWORD from deploy/http_web.password)
curl -k -u web:PASSWORD https://<pi-host>/api/health
curl -s -o /dev/null -w '%{http_code} %{redirect_url}\n' http://<pi-host>/api/health
```

Existing on-device `config.toml` is not overwritten; merge `[http]` from `config.toml.example` if you want explicit keys.

## Persistence

- Commanded settings JSON: `/var/lib/rs_pool/state.json` (override with `[persist].path`).
- Loaded on boot; atomic write on each successful `pool/command` apply.
- Persists only commanded fields (`mode`, `set_speed`, setpoints, relays) — not RPM/temps/watts.
- `spd` is **0..=35**. Legacy ESP `×655` words (`>35`) are clamped to 35 on load and rewritten.

## MQTT (ESP-compatible)

### Hard prerequisite: ESP must be offline

**Before enabling rs_pool MQTT** (`[mqtt].host` non-empty), the ESP pool controller must be powered off / disconnected from the broker. Both clients share `pool/command`, `pool/status`, and `pool/connected` — dual publishers cause retained-status fights, command races, and confusing Home Assistant state. Cutover is one-at-a-time: stop ESP → deploy/start rs_pool with a real MQTT host → verify. Runtime does not enforce single-controller ownership yet; this is an operational requirement.

| Topic | Role |
|-------|------|
| `pool/command` | Subscribe; partial JSON updates |
| `pool/status` | Publish full snapshot (immediate on command, including noops; ~4s heartbeat) |
| `pool/connected` | Retained LWT `"0"`; publish `"1"` on connect / with status |

**`r1`–`r8` in status** come **only** from measured Modbus coil feedback (`null` until first successful read). Never echo commanded as truth.

**`spd`** is integer **0..=35** (demand scale; `<1` → off).

Test a command (from a host with broker access):

```bash
mosquitto_pub -h <mqtt-broker> -t pool/command -m '{"m":0,"spt":72,"r1":1}'
mosquitto_pub -h <mqtt-broker> -t pool/command -m '{"spd":10}'
mosquitto_sub -h <mqtt-broker> -t 'pool/#' -v
```

On the Pi, if `mosquitto_pub` is installed, same commands work. Confirm persist with:

```bash
ssh <user>@<pi-host> 'sudo cat /var/lib/rs_pool/state.json'
```

## Runtime

- Tokio multi-thread runtime, SIGINT/SIGTERM shutdown
- `mpsc` command channel + `watch` shutdown + `Arc<RwLock<PoolState>>` + status `Notify` + bus `Notify`
- MQTT task when enabled; quiet debug heartbeat; ADS1115 temp task when `[temp].enabled`
- **Modbus task** when `[modbus].enabled`: sole owner of `/dev/serial0` @ 9600 8N1

### RS485 Modbus (relay + pump)

Sole master on the bus (ESP offline). HAT auto TX/RX — no RE/DE GPIO.

| Slave | Addr | Role |
|-------|------|------|
| Waveshare Modbus RTU Relay | `0x01` | 8 coils → MQTT r1–r8 |
| VS pump | `0x15` | Custom FCs 0x41–0x45 |

**Relay (hardware-verified — ESP-compatible frames):**

| Op | Frame (before CRC) |
|----|--------------------|
| Read 8 coils | `01 01 00 FF 00 01` (packed status byte) |
| Write one coil | FC `0x05` per channel `0..=7` (`01 05 00 <ch> FF 00` on / `00 00` off) |

Bit0 = r1 … bit7 = r8. **r6** = heat, **r7** = spa divert. Prefer **r4/r5** for bench tests (avoid heat/divert). On command: FC05 ×8 → immediate read → status. Idle poll ~30s. Optional one retry if measured≠commanded.

Wiki [Protocol V3](https://www.waveshare.com/wiki/Modbus_RTU_Relay#Development_Protocol_V3) frames (`01 01 00 00 00 08` read, FC `0x0F` multi-coil write) **timed out** on this board; the ESP-era read-at-`0x00FF` + FC05 path works.

**Pump:**

| FC | Purpose |
|----|---------|
| `0x43` | GET_STATUS |
| `0x45` | READ_SENSOR (RPM `0x00`, amb `0x08`, watts `0x0A`) — value **uint16 LE**, RPM is **raw/4** |
| `0x44` | SET_DEMAND (mode 0 = Speed RPM×4; demand = target_RPM×4 LE) |
| `0x41` | GO |
| `0x42` | STOP |

ACK response byte `0x10`. Off: demand 0 + STOP; on: demand + GO. Poll ~8s or on `update_pump`.

Fail-soft: individual RTU timeouts are best-effort (pump cycle continues: demand → status → sensors); service stays up and keeps the serial port open (no drop/re-open on timeout). Default response timeout is **1s**.

Debug: `RUST_LOG=info,rs_pool::modbus=debug`.

### Water temp (ADS1115)

Shared NTC on ADS1115 A0 (`/dev/i2c-1`, `0x48`). Divert valve **r7** selects the active loop: **on → spa (`st`)**, **off → pool (`pt`)**. Prefers **measured** r7 when Modbus feedback is available; falls back to commanded until first read. After an r7 edge (and on first boot observation), the active field is `null` for `settle_secs` (default 90) while water circulates; the inactive field keeps its last good value. Sensor routing follows **r7**, not MQTT mode `m` (rate-limited warn if they disagree).

Fail-soft: missing/disconnected ADS1115 or bad reads keep the service up; active temp is `None` / MQTT JSON `null` (never `0`). Rate-limited warn (~30s).

Debug filter: `RUST_LOG=info,rs_pool::temp=debug`.

**I2C enable** is in `deploy/config.txt` (`dtparam=i2c_arm=on` under `[all]`). Deploy does **not** push `config.txt` — copy to `/boot/firmware/config.txt` and reboot once. On Raspberry Pi OS, `dtparam=i2c_arm=on` alone may not create `/dev/i2c-1` — also load `i2c-dev` (`modprobe i2c-dev` and `/etc/modules-load.d/i2c-dev.conf`). Confirm the ADS1115 with `i2cdetect` and journals before treating temp as live. Existing `/etc/rs_pool/config.toml` is not overwritten; missing `[temp]` / `[modbus]` use serde defaults. Merge from `config.toml.example` if you want explicit keys on device.

After hardware:

```bash
ssh <user>@<pi-host> 'sudo i2cdetect -y 1'   # want 48 (or UU) at 0x48 — follow up if missing
ssh <user>@<pi-host> 'sudo journalctl -u rs-pool -n 80 --no-pager'
# look for temp recovered / published st|pt; fault path: rate-limited warn + null temps
# look for modbus serial open / relay|pump activity
```

## Local checks (before deploy)

```bash
make check          # fmt check + clippy -D warnings + host build
make build-pi       # aarch64-linux release via cargo zigbuild
```

## Layout reminders

| Path | Role |
|------|------|
| `src/main.rs` | Entrypoint: logging, config, run |
| `src/app.rs` | Task supervisor, channels, shutdown |
| `src/config.rs` | TOML config load |
| `src/state.rs` | `PoolState` / commanded + measured / spd 0–35 |
| `src/health.rs` | Subsystem health flags for the HTTP dashboard |
| `src/log_buffer.rs` | Tracing ring buffer for `/api/logs` |
| `src/http/` | HTTPS status dashboard (axum + Basic auth) |
| `src/persist.rs` | Atomic load/save of commanded settings |
| `src/mqtt.rs` | rumqttc task: command/status/connected |
| `src/modbus/` | RS485 RTU master: CRC, relay (ESP-style FC01/FC05), pump FCs, bus task |
| `src/temp/` | ADS1115 + NTC: convert, AdsReader, poll task (r7 divert / settle) |
| `deploy/deploy.sh` | Build + push binary/unit/env/journald/config/TLS/auth over SSH |
| `deploy/rs-pool.service` | systemd unit (`rs-pool`) |
| `deploy/rs-pool.env` | `RUST_LOG` and other runtime env |
| `deploy/rs-pool.toml` | Config template (set `[mqtt].host` to your broker) |
| `deploy/local.env.example` | Copy to gitignored `deploy/local.env` for SSH user default |
| `deploy/http_web.password` | Gitignored plaintext for Basic user `web` |
| `deploy/journald-rs-pool.conf` | journald size/retention limits |
| `deploy/README.md` | Pi UART / RS485 HAT setup |
| `AGENTS.local.md` | Gitignored site-specific host / broker notes |

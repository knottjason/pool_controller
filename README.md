# pool_controller (`rs_pool`)

Rust service that drives a variable-speed pool pump and Waveshare Modbus RTU relay board over RS485, with MQTT topics compatible with a prior ESP32 controller / Home Assistant setup.

Runs as a systemd service on a Raspberry Pi (tested on Pi Zero 2 W) with a Waveshare [RS485 CAN HAT](https://www.waveshare.com/wiki/RS485_CAN_HAT).

## Bill of materials

| Part | Notes | Link |
|------|-------|------|
| Raspberry Pi Zero 2 W kit | Board + headers / case / adapters | [Amazon](https://www.amazon.com/dp/B09LTDQY2Z) |
| Waveshare RS485 CAN HAT | RS485 bus for Modbus (relay + pump) | [Amazon](https://www.amazon.com/dp/B07VMB1ZKH) |
| ADS1115 16-bit ADC (I2C) | Water-temp NTC (A0); 3.3 V friendly | [Amazon](https://www.amazon.com/dp/B0GJCSJVH9) |
| Waveshare PoE Ethernet USB HUB HAT | Optional power + Ethernet for Zero | [Amazon](https://www.amazon.com/dp/B09PZY3HGV) |

Also required on the RS485 bus (not linked above): Waveshare Modbus RTU 8-ch relay board (slave `0x01`) and the VS pump (slave `0x15`), plus an NTC thermistor / divider for the ADS1115.

## Home Assistant

Designed to sit on the same MQTT broker as [Home Assistant](https://www.home-assistant.io/) (MQTT integration / Mosquitto add-on). Short JSON keys match the earlier ESP controller so existing automations, MQTT sensors, and switches can keep working with little or no change.

Point HA at your broker, then use the topics below (defaults in `deploy/rs-pool.toml`). Status publishes immediately on real commanded changes and on a short heartbeat (~4s). `pool/connected` is retained with LWT `"0"` so HA can show availability.

**Cutover:** only one publisher should own `pool/#` — stop the ESP before enabling this service with a non-empty `[mqtt].host`.

## MQTT

| Topic | Direction | Notes |
|-------|-----------|--------|
| `pool/command` | subscribe | Partial JSON object; only present keys are applied |
| `pool/status` | publish | Full snapshot (commanded + measured) |
| `pool/connected` | publish (retained) | `"1"` when up; LWT `"0"` on disconnect |

### `pool/command` (partial)

| Key | Type | Meaning |
|-----|------|---------|
| `m` | int | Mode: `0` = pool, non-zero = spa |
| `spd` | int | Pump demand **0..=35** (`0` / `<1` = off) |
| `spt` | int | Pool setpoint (°F) |
| `sst` | int | Spa setpoint (°F) |
| `r1`…`r8` | boolish | Relays (`0`/`1`, `true`/`false`); **r6** heat, **r7** spa divert |
| `v` | any | Accepted for ESP parity; ignored |

```bash
mosquitto_pub -h <mqtt-broker> -t pool/command -m '{"m":0,"spt":72,"r1":1}'
mosquitto_pub -h <mqtt-broker> -t pool/command -m '{"spd":10}'
```

### `pool/status`

| Key | Meaning |
|-----|---------|
| `ip` | Controller IP string |
| `rpm` | Pump shaft RPM (sensor raw ÷ 4) |
| `spd` | Commanded speed 0..=35 |
| `watt` | Pump shaft watts |
| `m` | Mode (pool/spa) |
| `st` / `pt` | Spa / pool water °F, or JSON `null` if unknown / settling |
| `sst` / `spt` | Spa / pool setpoints |
| `at` | Ambient °F (pump sensor) |
| `v` | Status schema version |
| `r1`…`r8` | Measured coil feedback (`0`/`1`), or `null` until first Modbus read |

`r1`–`r8` in status are **bus feedback only** — never an echo of the last command.

## Features

- **MQTT** — `pool/command`, `pool/status`, `pool/connected` (LWT); partial JSON commands; `spd` scale **0..=35**
- **Modbus RTU** — sole bus master on `/dev/serial0` @ 9600: relay coils (ESP-compatible FC01/FC05) + VS pump custom FCs (`0x41`–`0x45`)
- **Water temp** — ADS1115 + NTC; divert relay **r7** selects spa vs pool; fail-soft `null` temps when the chip is missing
- **HTTPS dashboard** — read-only status / health / recent logs (Basic auth); HTTP → HTTPS redirect
- **Persistence** — commanded settings in `/var/lib/rs_pool/state.json`

## Quick start

**Requirements (build host):** Rust (see `rust-toolchain.toml`), [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) + [Zig](https://ziglang.org/) for aarch64 cross builds, `htpasswd`, `openssl`, SSH key access to the Pi.

```bash
git clone https://github.com/knottjason/pool_controller.git
cd pool_controller

# optional: set default SSH user for deploy
cp deploy/local.env.example deploy/local.env

# edit MQTT broker before first deploy (seeded once to the Pi)
# deploy/rs-pool.toml → [mqtt].host = "your-broker.local"

make check
make deploy HOST=pi@192.168.1.50
```

See **[deploy/README.md](deploy/README.md)** for UART / HAT setup, Modbus notes, ADS1115 wiring, and logging.

## Configuration

| Path | Role |
|------|------|
| `deploy/rs-pool.toml` | Template → `/etc/rs_pool/config.toml` (seeded once; not overwritten later) |
| `deploy/rs-pool.env` | `RUST_LOG` → `/etc/rs_pool/env` |
| `deploy/http_web.password` | Local-only Basic-auth password (gitignored; hash installed on the Pi) |
| `deploy/local.env` | Optional gitignored deploy defaults (`RS_POOL_DEPLOY_USER`) |

Override config path with `RS_POOL_CONFIG`. Empty `[mqtt].host` disables MQTT.

## Development

```bash
make fmt
make clippy
cargo test
make build-pi    # aarch64-unknown-linux-gnu release via zig
```

Agent-oriented notes: [`AGENTS.md`](AGENTS.md). Site-specific hostnames belong in gitignored `AGENTS.local.md`.

## License

[MIT](LICENSE)

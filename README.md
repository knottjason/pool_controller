# pool_controller (`rs_pool`)

Rust service that drives a variable-speed pool pump and Waveshare Modbus RTU relay board over RS485, with MQTT topics compatible with a prior ESP32 controller / Home Assistant setup.

Runs as a systemd service on a Raspberry Pi (tested on Pi Zero 2 W) with a Waveshare [RS485 CAN HAT](https://www.waveshare.com/wiki/RS485_CAN_HAT).

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

**Cutover:** If replacing an ESP controller, power it off (or disconnect it from MQTT) before enabling `rs_pool` with a non-empty `[mqtt].host`. Do not run both publishers on `pool/#` at once.

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

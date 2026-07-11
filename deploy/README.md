# Deploy

Deploy `rs_pool` to a Raspberry Pi Zero 2 W over SSH, and enable the Waveshare [RS485 CAN HAT](https://www.waveshare.com/wiki/RS485_CAN_HAT).

## Deploy the service

Assumes SSH key access to the Pi.

**Cutover:** If replacing the ESP controller, power the ESP off (or disconnect it from MQTT) **before** starting rs_pool with a non-empty `[mqtt].host`. Both must not publish `pool/#` at the same time.

```bash
# from repo root
./deploy/deploy.sh 192.168.1.50
./deploy/deploy.sh pi@pool-pi.local
./deploy/deploy.sh 192.168.1.50 --skip-build
make deploy HOST=192.168.1.50
```

Default SSH user is `pi`. Override with `user@host`, `--user`, or gitignored `deploy/local.env` (`RS_POOL_DEPLOY_USER`; see `deploy/local.env.example`).

What it does:

1. Builds the Pi release binary (`cargo zigbuild`, unless `--skip-build`)
2. Compares remote vs local SHA-256 for unit / env / journald conf
3. Ensures `deploy/http_web.password` exists (generates one if missing) and installs bcrypt hash to `/etc/rs_pool/http_auth`
4. Seeds self-signed TLS cert/key under `/etc/rs_pool/tls/` once if missing
5. Installs:
   - binary → `/usr/local/bin/rs_pool`
   - unit → `/etc/systemd/system/rs-pool.service`
   - env → `/etc/rs_pool/env` (`RUST_LOG`, etc.)
   - journald drop-in → `/etc/systemd/journald.conf.d/99-rs-pool.conf`
   - state dir → `/var/lib/rs_pool`
6. If journald conf changed → `systemctl restart systemd-journald`
7. If the unit changed → `daemon-reload` + `enable` + `restart`
8. If only env or binary changed → `restart`
9. Fails if remote `/etc/rs_pool/config.toml` has an empty `[mqtt].host` (MQTT would be disabled)
10. Prints `systemctl status` and HTTPS verify hints

Requires local `htpasswd` (`apache2-utils` / `httpd`) and `openssl`.

## HTTPS status dashboard

Read-only UI on the Pi:

- `https://<host>/` — Basic auth user **`web`**, password in gitignored `deploy/http_web.password`
- Port **80** redirects to HTTPS; port **443** serves TLS with a self-signed cert
- Certs live in `/etc/rs_pool/tls/` (seeded once by deploy; not overwritten later)
- Auth hash: `/etc/rs_pool/http_auth` (`web:<bcrypt>`) — rewritten every deploy from the local password file

```bash
curl -k -u web:PASSWORD https://<pi-host>/api/health
```

Accept the browser self-signed warning (or use `curl -k`). Existing `/etc/rs_pool/config.toml` is not overwritten; merge `[http]` from `config.toml.example` if needed.

## Logging

The service uses `tracing`. Level is controlled by `RUST_LOG` in `/etc/rs_pool/env` (source: `deploy/rs-pool.env`).

```bash
# follow
journalctl -u rs-pool -f

# bump to debug, then restart
sudo sed -i 's/^RUST_LOG=.*/RUST_LOG=debug/' /etc/rs_pool/env
sudo systemctl restart rs-pool
```

Journal rotation / retention (Pi SD-friendly) comes from `deploy/journald-rs-pool.conf`:

- `SystemMaxUse=64M`
- `MaxRetentionSec=14day`
- `MaxFileSec=1day`
- compression on

Manual vacuum if needed:

```bash
sudo journalctl --vacuum-size=64M
sudo journalctl --vacuum-time=14d
```

## Raspberry Pi: enable the RS485 CAN HAT

Hardware: Waveshare RS485 CAN HAT on a Pi Zero 2 W.

RS485 uses the Pi UART (GPIO TX/RX). The HAT defaults to **hardware auto TX/RX**, so you do **not** need to drive the RSE pin (BCM 4) unless you rework the board for manual direction control.

### 1. Enable UART, disable serial console

```bash
sudo raspi-config
```

**Interface Options → Serial Port**

- Login shell over serial: **No**
- Serial port hardware: **Yes**

### 2. Boot config

A ready-to-use `config.txt` lives in this folder (copied from a Pi Zero 2 W image with the HAT lines under `[all]`):

```ini
# RS485 — free PL011 UART on GPIO 14/15
enable_uart=1
dtoverlay=disable-bt
# CAN — MCP2515 (Waveshare overlay; 12 MHz crystal on current boards)
dtparam=spi=on
dtoverlay=mcp2515-can0,oscillator=12000000,interrupt=25,spimaxfrequency=2000000
# ADS1115 water-temp ADC on I2C1
dtparam=i2c_arm=on
```

There is **no RS485-specific dtoverlay** for this HAT: RS485 is plain UART plus the onboard SP3485 with hardware auto TX/RX. The board-specific overlay is **`mcp2515-can0`** for the CAN side.

**Note:** `deploy.sh` does **not** push `config.txt`. Apply it manually to the SD boot partition or `/boot/firmware/config.txt`, then reboot.

On a mounted SD card boot partition:

```bash
cp deploy/config.txt /Volumes/bootfs/config.txt
```

Or on a running Pi (Bookworm):

```bash
sudo cp /path/to/deploy/config.txt /boot/firmware/config.txt
```

`disable-bt` frees the full PL011 UART on the Zero 2 W GPIO TX/RX pins for the HAT (Bluetooth otherwise often owns it).

On older Raspberry Pi OS images the path is `/boot/config.txt`.

### 3. Remove serial console from cmdline (if present)

```bash
sudo nano /boot/firmware/cmdline.txt
```

Delete any `console=serial0,115200` / `console=ttyAMA0,...` fragment so the kernel is not using the port.

### 4. Reboot and confirm the device

```bash
sudo reboot
ls -l /dev/serial*
```

On Zero 2 W you typically want:

- `/dev/serial0` → `/dev/ttyAMA0`

Prefer **`/dev/serial0`** in software (stable symlink).

### 5. Wiring

| HAT | Device |
|-----|--------|
| A   | A      |
| B   | B      |
| GND | GND (recommended) |

### 6. Permissions (optional)

The systemd unit runs as `root`, so this is only needed for interactive testing as a normal user:

```bash
sudo usermod -aG dialout "$USER"
# log out / in
```

### Sanity check

Use a USB–RS485 adapter ↔ HAT A/B. Open `/dev/serial0` at your bus baud (often 9600/19200 for pool gear), no flow control.

## RS485 Modbus devices (rs_pool)

`rs_pool` is the **sole** Modbus RTU master on `/dev/serial0` @ 9600 8N1. Keep the ESP powered off.

### Waveshare 8-ch relay (hardware-verified — ESP-compatible frames)

This board answers **ESP-compatible** frames, not wiki Protocol V3 (V3 read `01 01 00 00 00 08` and FC `0x0F` multi-coil write **timed out** on this hardware):

| Op | Frame (before CRC) |
|----|--------------------|
| Read 8 coils | `01 01 00 FF 00 01` (packed status byte) |
| Write one coil | FC `0x05` per channel `0..=7` (`01 05 00 <ch> FF 00` on / `00 00` off) |

Bit0 = MQTT **r1** … bit7 = **r8**. Status `r1`–`r8` are bus feedback only (`null` until first successful read). Prefer **r4/r5** for bench tests (avoid heat **r6** / divert **r7**). Wiki still documents [V3](https://www.waveshare.com/wiki/Modbus_RTU_Relay#Development_Protocol_V3); `src/modbus/relay.rs` implements what this unit actually speaks.

### VS pump (slave `0x15`)

Custom FCs: `0x43` status, `0x45` sensor (uint16 LE), `0x44` demand, `0x41` GO, `0x42` STOP. MQTT `spd` is **0..=35**.

### Config

`[modbus]` in `rs-pool.toml` / `config.toml.example`. Existing device configs without `[modbus]` still boot (serde defaults). Merge from the example if you want explicit keys.

Debug:

```bash
ssh <user>@<pi-host> 'sudo journalctl -u rs-pool -n 80 --no-pager'
# RUST_LOG=info,rs_pool::modbus=debug
```

## Optional: bring up CAN

After boot, confirm the MCP2515 overlay loaded, then bring up the interface:

```bash
dmesg | grep -iE 'can|spi|mcp251'
sudo ip link set can0 up type can bitrate 1000000
```

Older pre‑Aug 2019 boards used an **8 MHz** crystal — change the overlay to `oscillator=8000000` and `spimaxfrequency=1000000`. See the [Waveshare wiki](https://www.waveshare.com/wiki/RS485_CAN_HAT).

Not required for RS485 / Modbus.

## ADS1115 water temperature

Shared NTC on ADS1115 channel A0. Divert valve **r7** selects which loop the reading applies to:

| r7 | Active MQTT field |
|----|-------------------|
| on / true | spa (`st`) |
| off / false | pool (`pt`) |

After an r7 change, the active field stays JSON `null` for `[temp].settle_secs` (default 90) while water circulates; the other field keeps its last good value. Routing follows **r7**, not MQTT mode `m`.

### Wiring

| Signal | Pi | ADS1115 / divider |
|--------|----|-------------------|
| SDA | GPIO2 pin 3 | SDA |
| SCL | GPIO3 pin 5 | SCL |
| 3V3 | pin 1 | VDD |
| GND | pin 9 (or any GND) | GND + ADDR |
| A0 | — | divider mid (10k series to 3V3, NTC to GND) |

Thermistor constants match the ESP: series 10k, nominal 10500 Ω @ 25 °C, B=3950. Output is °F.

### Enable I2C + verify

1. Ensure `dtparam=i2c_arm=on` in `/boot/firmware/config.txt` (see `deploy/config.txt`), then reboot. On Raspberry Pi OS that alone may not create `/dev/i2c-1` — also load `i2c-dev` (`modprobe i2c-dev` and `/etc/modules-load.d/i2c-dev.conf`). Wiring/permissions may still need follow-up if the chip does not appear.
2. Detect the chip:

```bash
sudo i2cdetect -y 1   # want 48 (or UU) at 0x48 — investigate if missing
```

3. Deploy and check journals / MQTT:

```bash
make deploy HOST=<pi-host>
ssh <user>@<pi-host> 'sudo systemctl --no-pager --full status rs-pool'
ssh <user>@<pi-host> 'sudo journalctl -u rs-pool -n 80 --no-pager'
# Debug: RUST_LOG=info,rs_pool::temp=debug
mosquitto_sub -h <mqtt-broker> -t pool/status -C 3 -v
```

Without hardware the service stays up and rate-limits ADS open/read warnings; `st`/`pt` publish as JSON `null`.

**Config note:** Deploy seeds `/etc/rs_pool/config.toml` once and does not overwrite it later. Older configs without `[temp]` still boot (serde defaults). The example at `/etc/rs_pool/config.toml.example` is always refreshed — merge `[temp]` manually if you want explicit keys on device.

## Files

| File | Purpose |
|------|---------|
| `deploy.sh` | Build + push binary/unit/env/journald over SSH |
| `rs-pool.service` | systemd unit installed on the Pi |
| `rs-pool.env` | Runtime env (`RUST_LOG`) → `/etc/rs_pool/env` |
| `local.env.example` | Copy to gitignored `local.env` for `RS_POOL_DEPLOY_USER` |
| `journald-rs-pool.conf` | Journal size/retention drop-in |
| `config.txt` | Pi boot config with UART / RS485 HAT / I2C enabled |
| `rs-pool.toml` | Runtime config template (set `[mqtt].host`; includes `[temp]`) |
| `http_web.password` | Gitignored Basic-auth plaintext for user `web` |
| `README.md` | This doc |

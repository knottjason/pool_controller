#!/usr/bin/env bash
# Deploy rs_pool binary + systemd unit to a Raspberry Pi over SSH (key auth).
#
# Usage:
#   ./deploy/deploy.sh <host|user@host> [--skip-build] [--user USER]
#
# Examples:
#   ./deploy/deploy.sh 192.168.1.50
#   ./deploy/deploy.sh pi@pool-pi.local
#   ./deploy/deploy.sh 192.168.1.50 --skip-build

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_PI="aarch64-unknown-linux-gnu"
BIN_NAME="rs_pool"
LOCAL_BIN="${ROOT}/target/${TARGET_PI}/release/${BIN_NAME}"
LOCAL_UNIT="${ROOT}/deploy/rs-pool.service"
LOCAL_ENV="${ROOT}/deploy/rs-pool.env"
LOCAL_JOURNALD="${ROOT}/deploy/journald-rs-pool.conf"
LOCAL_CONFIG="${ROOT}/deploy/rs-pool.toml"
LOCAL_PASSWORD="${ROOT}/deploy/http_web.password"
REMOTE_BIN="/usr/local/bin/${BIN_NAME}"
REMOTE_UNIT="/etc/systemd/system/rs-pool.service"
REMOTE_ENV="/etc/rs_pool/env"
REMOTE_JOURNALD="/etc/systemd/journald.conf.d/99-rs-pool.conf"
REMOTE_CONFIG="/etc/rs_pool/config.toml"
REMOTE_CONFIG_EXAMPLE="/etc/rs_pool/config.toml.example"
REMOTE_STATE_DIR="/var/lib/rs_pool"
REMOTE_TLS_DIR="/etc/rs_pool/tls"
REMOTE_AUTH="/etc/rs_pool/http_auth"
SERVICE_NAME="rs-pool"
SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10)

SKIP_BUILD=0
# Default SSH user when host has no user@ prefix. Override via deploy/local.env
# (gitignored) or RS_POOL_DEPLOY_USER in the environment.
if [[ -f "${ROOT}/deploy/local.env" ]]; then
  # shellcheck disable=SC1091
  source "${ROOT}/deploy/local.env"
fi
DEFAULT_USER="${RS_POOL_DEPLOY_USER:-pi}"
HOST=""
SSH_USER=""

usage() {
  cat <<'EOF'
Deploy rs_pool binary + systemd unit over SSH (key auth).

Usage:
  ./deploy/deploy.sh <host|user@host> [--skip-build] [--user USER]

Examples:
  ./deploy/deploy.sh 192.168.1.50
  ./deploy/deploy.sh pi@pool-pi.local
  ./deploy/deploy.sh 192.168.1.50 --skip-build
  make deploy HOST=192.168.1.50
EOF
  exit "${1:-0}"
}

die() {
  echo "error: $*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h | --help)
      usage 0
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --user)
      [[ $# -ge 2 ]] || die "--user requires a value"
      SSH_USER="$2"
      shift 2
      ;;
    -*)
      die "unknown option: $1"
      ;;
    *)
      [[ -z "${HOST}" ]] || die "unexpected argument: $1"
      HOST="$1"
      shift
      ;;
  esac
done

[[ -n "${HOST}" ]] || usage 1

# Allow user@host, or --user, or default.
if [[ "${HOST}" == *@* ]]; then
  SSH_USER="${HOST%%@*}"
  HOST="${HOST#*@}"
elif [[ -z "${SSH_USER}" ]]; then
  SSH_USER="${DEFAULT_USER}"
fi

REMOTE="${SSH_USER}@${HOST}"

need_cmd ssh
need_cmd scp
need_cmd make
need_cmd openssl
need_cmd htpasswd
if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  die "missing required command: sha256sum or shasum"
fi

remote() {
  # shellcheck disable=SC2029
  ssh "${SSH_OPTS[@]}" "${REMOTE}" "$@"
}

file_sha() {
  local path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${path}" | awk '{print $1}'
  else
    shasum -a 256 "${path}" | awk '{print $1}'
  fi
}

remote_file_sha() {
  local remote_path="$1"
  remote "if [ -f '${remote_path}' ]; then sha256sum '${remote_path}' | awk '{print \$1}'; else echo missing; fi"
}

echo "==> target: ${REMOTE}"

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
  echo "==> building Pi release binary"
  make -C "${ROOT}" build-pi
else
  echo "==> skipping build (--skip-build)"
fi

[[ -f "${LOCAL_BIN}" ]] || die "missing binary: ${LOCAL_BIN} (run without --skip-build)"
[[ -f "${LOCAL_UNIT}" ]] || die "missing unit file: ${LOCAL_UNIT}"
[[ -f "${LOCAL_ENV}" ]] || die "missing env file: ${LOCAL_ENV}"
[[ -f "${LOCAL_JOURNALD}" ]] || die "missing journald conf: ${LOCAL_JOURNALD}"
[[ -f "${LOCAL_CONFIG}" ]] || die "missing config file: ${LOCAL_CONFIG}"

if [[ ! -f "${LOCAL_PASSWORD}" ]]; then
  echo "==> generating ${LOCAL_PASSWORD}"
  openssl rand -base64 18 >"${LOCAL_PASSWORD}"
  chmod 600 "${LOCAL_PASSWORD}"
  echo "==> HTTP Basic password for user 'web' (save this): $(tr -d '\n' <"${LOCAL_PASSWORD}")"
else
  echo "==> using existing ${LOCAL_PASSWORD}"
fi

AUTH_LINE="$(htpasswd -nbB web "$(tr -d '\n' <"${LOCAL_PASSWORD}")")"
[[ "${AUTH_LINE}" == web:* ]] || die "htpasswd failed to produce web:hash line"

echo "==> checking SSH"
remote 'echo ok' >/dev/null

LOCAL_UNIT_SHA="$(file_sha "${LOCAL_UNIT}")"
LOCAL_ENV_SHA="$(file_sha "${LOCAL_ENV}")"
LOCAL_JOURNALD_SHA="$(file_sha "${LOCAL_JOURNALD}")"
REMOTE_UNIT_SHA="$(remote_file_sha "${REMOTE_UNIT}")"
REMOTE_ENV_SHA="$(remote_file_sha "${REMOTE_ENV}")"
REMOTE_JOURNALD_SHA="$(remote_file_sha "${REMOTE_JOURNALD}")"
REMOTE_CONFIG_EXISTS="$(remote "if [ -f '${REMOTE_CONFIG}' ]; then echo yes; else echo no; fi")"

UNIT_CHANGED=0
ENV_CHANGED=0
JOURNALD_CHANGED=0
if [[ "${REMOTE_UNIT_SHA}" != "${LOCAL_UNIT_SHA}" ]]; then
  UNIT_CHANGED=1
  echo "==> unit file changed (remote=${REMOTE_UNIT_SHA}, local=${LOCAL_UNIT_SHA})"
else
  echo "==> unit file unchanged"
fi
if [[ "${REMOTE_ENV_SHA}" != "${LOCAL_ENV_SHA}" ]]; then
  ENV_CHANGED=1
  echo "==> env file changed (remote=${REMOTE_ENV_SHA}, local=${LOCAL_ENV_SHA})"
else
  echo "==> env file unchanged"
fi
if [[ "${REMOTE_JOURNALD_SHA}" != "${LOCAL_JOURNALD_SHA}" ]]; then
  JOURNALD_CHANGED=1
  echo "==> journald conf changed (remote=${REMOTE_JOURNALD_SHA}, local=${LOCAL_JOURNALD_SHA})"
else
  echo "==> journald conf unchanged"
fi
if [[ "${REMOTE_CONFIG_EXISTS}" == "yes" ]]; then
  echo "==> config present (leaving ${REMOTE_CONFIG} untouched)"
else
  echo "==> config missing (will seed ${REMOTE_CONFIG} from example)"
fi

TMP_DIR="$(remote 'mktemp -d /tmp/rs_pool_deploy.XXXXXX')"
cleanup_remote() {
  remote "rm -rf '${TMP_DIR}'" >/dev/null 2>&1 || true
}
trap cleanup_remote EXIT

echo "==> uploading artifacts to ${TMP_DIR}"
scp "${SSH_OPTS[@]}" \
  "${LOCAL_BIN}" \
  "${LOCAL_UNIT}" \
  "${LOCAL_ENV}" \
  "${LOCAL_JOURNALD}" \
  "${LOCAL_CONFIG}" \
  "${REMOTE}:${TMP_DIR}/"

# Auth hash only (never the plaintext password file).
printf '%s\n' "${AUTH_LINE}" | remote "cat > '${TMP_DIR}/http_auth'"

echo "==> installing"
remote "sudo install -d '${REMOTE_STATE_DIR}' /etc/rs_pool '${REMOTE_TLS_DIR}' /etc/systemd/journald.conf.d \
  && sudo install -m 755 '${TMP_DIR}/${BIN_NAME}' '${REMOTE_BIN}' \
  && sudo install -m 644 '${TMP_DIR}/rs-pool.service' '${REMOTE_UNIT}' \
  && sudo install -m 644 '${TMP_DIR}/rs-pool.env' '${REMOTE_ENV}' \
  && sudo install -m 644 '${TMP_DIR}/journald-rs-pool.conf' '${REMOTE_JOURNALD}' \
  && sudo install -m 644 '${TMP_DIR}/rs-pool.toml' '${REMOTE_CONFIG_EXAMPLE}' \
  && if [ ! -f '${REMOTE_CONFIG}' ]; then sudo install -m 644 '${TMP_DIR}/rs-pool.toml' '${REMOTE_CONFIG}'; fi \
  && sudo install -m 600 '${TMP_DIR}/http_auth' '${REMOTE_AUTH}' \
  && if [ ! -f '${REMOTE_TLS_DIR}/cert.pem' ] || [ ! -f '${REMOTE_TLS_DIR}/key.pem' ]; then \
       echo 'seeding self-signed TLS cert for ${HOST}'; \
       sudo openssl req -x509 -nodes -newkey rsa:2048 -days 825 \
         -keyout '${REMOTE_TLS_DIR}/key.pem' \
         -out '${REMOTE_TLS_DIR}/cert.pem' \
         -subj '/CN=${HOST}'; \
       sudo chmod 644 '${REMOTE_TLS_DIR}/cert.pem'; \
       sudo chmod 600 '${REMOTE_TLS_DIR}/key.pem'; \
     else \
       echo 'TLS cert present (leaving ${REMOTE_TLS_DIR} untouched)'; \
     fi"

echo "==> checking mqtt host in ${REMOTE_CONFIG}"
# Extract host= from the [mqtt] section only (simple TOML; no nested tables).
MQTT_HOST_VAL="$(remote "awk '
  /^[[:space:]]*\\[mqtt\\]/ { in_mqtt=1; next }
  /^[[:space:]]*\\[/ { in_mqtt=0 }
  in_mqtt && \$1 == \"host\" {
    sub(/^[^=]*=[[:space:]]*/, \"\");
    gsub(/[\\\"'\\'']/, \"\");
    sub(/[[:space:]]*#.*/, \"\");
    gsub(/[[:space:]]/, \"\");
    print;
    exit
  }
' '${REMOTE_CONFIG}'")"

if [[ -z "${MQTT_HOST_VAL}" ]]; then
  die "mqtt host is empty in ${REMOTE_CONFIG} — MQTT would be disabled. Set [mqtt].host to your broker before deploy. Ensure the ESP is offline before enabling rs_pool MQTT."
fi
echo "==> mqtt host ok (${MQTT_HOST_VAL})"

if [[ "${JOURNALD_CHANGED}" -eq 1 ]]; then
  echo "==> journald conf changed: restarting systemd-journald"
  remote "sudo systemctl restart systemd-journald"
fi

if [[ "${UNIT_CHANGED}" -eq 1 ]]; then
  echo "==> unit changed: daemon-reload + enable + restart"
  remote "sudo systemctl daemon-reload \
    && sudo systemctl enable '${SERVICE_NAME}' \
    && sudo systemctl restart '${SERVICE_NAME}'"
elif [[ "${ENV_CHANGED}" -eq 1 ]]; then
  echo "==> env changed: restarting service"
  remote "sudo systemctl enable '${SERVICE_NAME}' >/dev/null 2>&1 || true; \
    sudo systemctl restart '${SERVICE_NAME}'"
else
  echo "==> restarting service (binary updated)"
  remote "sudo systemctl enable '${SERVICE_NAME}' >/dev/null 2>&1 || true; \
    sudo systemctl restart '${SERVICE_NAME}'"
fi

echo "==> status"
remote "sudo systemctl --no-pager --full status '${SERVICE_NAME}' || true"
echo "==> deploy complete"
echo "    logs:   journalctl -u ${SERVICE_NAME} -f"
echo "    level:  edit ${REMOTE_ENV} (RUST_LOG=...) then systemctl restart ${SERVICE_NAME}"
echo "    config: ${REMOTE_CONFIG} (example: ${REMOTE_CONFIG_EXAMPLE})"
echo "    https:  https://${HOST}/  (self-signed — accept browser warning; user web)"
echo "    verify: curl -k -u web:PASSWORD https://${HOST}/api/health"
echo "            (PASSWORD from ${LOCAL_PASSWORD})"

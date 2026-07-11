.PHONY: fmt clippy check build build-pi run deploy install-service help

TARGET_PI := aarch64-unknown-linux-gnu
BIN := rs_pool
SERVICE := deploy/rs-pool.service
# Usage: make deploy HOST=192.168.1.50
#        make deploy HOST=pi@pool-pi.local
HOST ?=

help:
	@echo "Targets:"
	@echo "  fmt         - rustfmt"
	@echo "  clippy      - clippy with -D warnings"
	@echo "  check       - fmt check + clippy + host build"
	@echo "  build       - release build for host (macOS)"
	@echo "  build-pi    - release build for Pi Zero 2 W (aarch64-linux via zig)"
	@echo "  run         - run on host"
	@echo "  deploy      - build + push binary/unit over SSH (HOST=ip|user@host)"
	@echo "  install-service - print install steps for the systemd unit"

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings

check:
	cargo fmt -- --check
	cargo clippy --all-targets -- -D warnings
	cargo build

build:
	cargo build --release

# Apple Silicon -> Linux aarch64: cargo-zigbuild + zig (cross's Docker images are amd64-only).
build-pi:
	cargo zigbuild --release --target $(TARGET_PI)
	@echo "Binary: target/$(TARGET_PI)/release/$(BIN)"
	@file target/$(TARGET_PI)/release/$(BIN)

run:
	cargo run

deploy:
	@[[ -n "$(HOST)" ]] || (echo "usage: make deploy HOST=192.168.1.50" >&2; exit 1)
	./deploy/deploy.sh "$(HOST)"

install-service:
	@echo "On the Pi:"
	@echo "  sudo install -d /var/lib/rs_pool"
	@echo "  sudo install -m 755 target/$(TARGET_PI)/release/$(BIN) /usr/local/bin/$(BIN)"
	@echo "  sudo install -m 644 $(SERVICE) /etc/systemd/system/rs-pool.service"
	@echo "  sudo systemctl daemon-reload"
	@echo "  sudo systemctl enable --now rs-pool"
	@echo "  journalctl -u rs-pool -f"

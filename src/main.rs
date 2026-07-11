mod app;
mod config;
mod health;
mod http;
mod log_buffer;
mod modbus;
mod mqtt;
mod persist;
mod state;
mod temp;

use std::sync::Arc;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::app::App;
use crate::config::Config;
use crate::log_buffer::{LogBuffer, LogBufferLayer};

fn init_logging(logs: Arc<std::sync::Mutex<LogBuffer>>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_level(true)
                .with_ansi(false)
                .with_writer(std::io::stdout),
        )
        .with(LogBufferLayer::new(logs))
        .init();
}

#[tokio::main]
async fn main() {
    let logs = LogBuffer::new(500);
    init_logging(Arc::clone(&logs));
    info!(service = "rs_pool", "starting");

    let config_path = Config::path();
    let config = match Config::load() {
        Ok(config) => config,
        Err(err) if Config::is_not_found(&err) => {
            // Local/dev fallback so `cargo run` works without /etc/rs_pool.
            info!(
                path = %config_path.display(),
                "config file missing; using built-in defaults (dev)"
            );
            Config::default()
        }
        Err(err) => {
            error!(
                error = %err,
                path = %config_path.display(),
                "config file exists but failed to load; refusing to start with defaults"
            );
            std::process::exit(1);
        }
    };

    if let Ok(mut guard) = logs.lock() {
        guard.set_capacity(config.http.log_buffer_lines);
    }

    if config.mqtt_enabled() {
        info!(host = %config.mqtt.host, "mqtt enabled");
    } else {
        error!(
            path = %config_path.display(),
            "mqtt host is empty — MQTT is DISABLED. Set [mqtt].host in config to your broker hostname/IP to enable. \
             Empty host is only intentional for local/dev without a broker."
        );
    }

    if let Err(err) = App::new(config, logs).run().await {
        error!(error = %err, "runtime exited with error");
        std::process::exit(1);
    }
}

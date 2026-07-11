//! HTTPS status dashboard (read-only) with :80 → HTTPS redirect and Basic auth.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{RwLock, watch};
use tracing::{debug, error, info};

use crate::config::HttpConfig;
use crate::health::HealthState;
use crate::log_buffer::LogBuffer;
use crate::state::{PoolState, unpack_relays};

const AUTH_USER: &str = "web";
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Arguments for the HTTP status dashboard task.
pub struct HttpTaskArgs {
    pub config: HttpConfig,
    pub state: Arc<RwLock<PoolState>>,
    pub health: Arc<RwLock<HealthState>>,
    pub logs: Arc<Mutex<LogBuffer>>,
    pub shutdown_rx: watch::Receiver<bool>,
}

#[derive(Clone)]
struct AppState {
    state: Arc<RwLock<PoolState>>,
    health: Arc<RwLock<HealthState>>,
    logs: Arc<Mutex<LogBuffer>>,
    password_hash: String,
}

/// Fail-soft HTTPS dashboard. Exits cleanly on bind/cert/auth failure or shutdown.
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub async fn http_task(args: HttpTaskArgs) {
    let HttpTaskArgs {
        config,
        state,
        health,
        logs,
        mut shutdown_rx,
    } = args;

    if !config.enabled {
        info!("http dashboard disabled ([http].enabled = false)");
        return;
    }

    // rustls 0.23 requires an explicit process-default crypto provider.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        debug!("rustls crypto provider already installed");
    }

    let password_hash = match load_password_hash(&config.auth_path) {
        Ok(hash) => hash,
        Err(err) => {
            error!(
                error = %err,
                path = %config.auth_path.display(),
                "http auth load failed; dashboard not started"
            );
            return;
        }
    };

    let tls = match RustlsConfig::from_pem_file(&config.cert_path, &config.key_path).await {
        Ok(cfg) => cfg,
        Err(err) => {
            error!(
                error = %err,
                cert = %config.cert_path.display(),
                key = %config.key_path.display(),
                "http TLS cert/key load failed; dashboard not started"
            );
            return;
        }
    };

    let redirect_addr: SocketAddr = match config.http_bind.parse() {
        Ok(addr) => addr,
        Err(err) => {
            error!(error = %err, bind = %config.http_bind, "invalid [http].http_bind");
            return;
        }
    };
    let tls_addr: SocketAddr = match config.https_bind.parse() {
        Ok(addr) => addr,
        Err(err) => {
            error!(error = %err, bind = %config.https_bind, "invalid [http].https_bind");
            return;
        }
    };

    let app_state = AppState {
        state,
        health,
        logs,
        password_hash,
    };

    let https_app = Router::new()
        .route("/", get(dashboard))
        .route("/api/status", get(api_status))
        .route("/api/health", get(api_health))
        .route("/api/logs", get(api_logs))
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            basic_auth,
        ))
        .with_state(app_state);

    let redirect_app = Router::new().fallback(redirect_to_https);

    let redirect_handle = Handle::new();
    let tls_handle = Handle::new();
    let redirect_shutdown = redirect_handle.clone();
    let tls_shutdown = tls_handle.clone();

    let mut shutdown_watch = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            if *shutdown_watch.borrow() {
                break;
            }
            if shutdown_watch.changed().await.is_err() {
                break;
            }
        }
        redirect_shutdown.graceful_shutdown(Some(Duration::from_secs(1)));
        tls_shutdown.graceful_shutdown(Some(Duration::from_secs(1)));
    });

    info!(%redirect_addr, %tls_addr, "http dashboard starting");

    let redirect_fut = axum_server::bind(redirect_addr)
        .handle(redirect_handle)
        .serve(redirect_app.into_make_service());

    let tls_fut = axum_server::bind_rustls(tls_addr, tls)
        .handle(tls_handle)
        .serve(https_app.into_make_service());

    tokio::select! {
        result = redirect_fut => {
            if let Err(err) = result {
                error!(error = %err, bind = %redirect_addr, "http redirect listener failed");
            }
        }
        result = tls_fut => {
            if let Err(err) = result {
                error!(error = %err, bind = %tls_addr, "https listener failed");
            }
        }
        _ = shutdown_rx.changed() => {
            debug!("http task shutdown signaled");
        }
    }

    info!("http dashboard stopped");
}

fn load_password_hash(path: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or_else(|| anyhow::anyhow!("auth file is empty"))?;
    let (user, hash) = line
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("auth line must be user:hash"))?;
    if user != AUTH_USER {
        anyhow::bail!("expected username {AUTH_USER}, found {user}");
    }
    if hash.is_empty() {
        anyhow::bail!("empty password hash");
    }
    Ok(hash.to_string())
}

async fn basic_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b64| B64.decode(b64.trim()).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|decoded| {
            let Some((user, pass)) = decoded.split_once(':') else {
                return false;
            };
            user == AUTH_USER && bcrypt::verify(pass, &state.password_hash).unwrap_or(false)
        });

    if authorized {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"rs_pool\""),
            )],
            "unauthorized",
        )
            .into_response()
    }
}

async fn redirect_to_https(req: Request) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let host = host.strip_suffix(":80").unwrap_or(host);
    let path_and_query = req.uri().path_and_query().map_or("/", |pq| pq.as_str());
    let location = format!("https://{host}{path_and_query}");
    Redirect::permanent(&location).into_response()
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn api_status(State(app): State<AppState>) -> impl IntoResponse {
    let guard = app.state.read().await;
    axum::Json(status_dashboard_json(&guard))
}

async fn api_health(State(app): State<AppState>) -> impl IntoResponse {
    let guard = app.health.read().await;
    axum::Json(guard.to_json())
}

#[derive(Debug, Deserialize)]
struct LogsQuery {
    limit: Option<usize>,
}

async fn api_logs(
    State(app): State<AppState>,
    Query(query): Query<LogsQuery>,
) -> impl IntoResponse {
    let lines = app
        .logs
        .lock()
        .map(|buf| buf.snapshot(query.limit))
        .unwrap_or_default();
    axum::Json(json!({ "lines": lines }))
}

/// Human-friendly pool snapshot for the dashboard (not ESP `pool/status` shape).
fn status_dashboard_json(state: &PoolState) -> Value {
    let commanded = unpack_relays(state.commanded.relays);
    let measured = state.measured.relay_status.map(unpack_relays);

    let relay_obj = |bits: [bool; 8]| -> Value {
        let mut map = serde_json::Map::new();
        for (i, on) in bits.iter().enumerate() {
            map.insert(format!("r{}", i + 1), Value::from(*on));
        }
        Value::Object(map)
    };

    json!({
        "ip": state.measured.ip,
        "mode": state.commanded.mode,
        "set_speed": state.commanded.set_speed,
        "set_spa_temp": state.commanded.set_spa_temp,
        "set_pool_temp": state.commanded.set_pool_temp,
        "rpm": state.measured.rpm,
        "watt": state.measured.watt,
        "air_temp": state.measured.air_temp,
        "spa_temp": state.measured.spa_temp,
        "pool_temp": state.measured.pool_temp,
        "commanded_relays": relay_obj(commanded),
        "measured_relays": measured.map(relay_obj),
        "update_pump": state.update_pump,
        "update_relays": state.update_relays,
        "water_temp_settling": state.water_temp_settling,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PoolState, pack_relays};

    #[test]
    fn status_json_includes_relays_and_temps() {
        let mut state = PoolState::default();
        state.commanded.relays =
            pack_relays([true, false, false, false, false, false, true, false]);
        state.measured.relay_status = Some(pack_relays([
            false, true, false, false, false, false, false, false,
        ]));
        state.measured.pool_temp = Some(72.5);
        let v = status_dashboard_json(&state);
        assert_eq!(v["commanded_relays"]["r1"], true);
        assert_eq!(v["commanded_relays"]["r7"], true);
        assert_eq!(v["measured_relays"]["r2"], true);
        assert_eq!(v["pool_temp"], 72.5);
        assert!(v.get("mqtt_connected").is_none());
    }

    #[test]
    fn load_password_hash_parses_web_line() {
        let dir = std::env::temp_dir().join(format!(
            "rs_pool_auth_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("http_auth");
        // bcrypt of "test" with cost 4 (fast for unit test)
        let hash = bcrypt::hash("test", 4).unwrap();
        std::fs::write(&path, format!("web:{hash}\n")).unwrap();
        let loaded = load_password_hash(&path).unwrap();
        assert!(bcrypt::verify("test", &loaded).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Offline persistence of commanded pool settings.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::state::{CommandedState, PoolState, migrate_spd};

#[derive(Debug, Error)]
pub enum PersistError {
    #[error("failed to create state directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write state {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to serialize state: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// On-disk document (commanded settings only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedState {
    pub mode: i32,
    pub set_speed: u16,
    pub set_spa_temp: i32,
    pub set_pool_temp: i32,
    pub relays: u8,
}

impl From<&CommandedState> for PersistedState {
    fn from(cmd: &CommandedState) -> Self {
        Self {
            mode: cmd.mode,
            set_speed: cmd.set_speed,
            set_spa_temp: cmd.set_spa_temp,
            set_pool_temp: cmd.set_pool_temp,
            relays: cmd.relays,
        }
    }
}

impl From<PersistedState> for CommandedState {
    fn from(p: PersistedState) -> Self {
        Self {
            mode: p.mode,
            set_speed: p.set_speed,
            set_spa_temp: p.set_spa_temp,
            set_pool_temp: p.set_pool_temp,
            relays: p.relays,
        }
    }
}

/// Load commanded settings from disk into `state`, or leave defaults if missing/corrupt.
pub fn load_into(state: &mut PoolState, path: &Path) {
    match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<PersistedState>(&raw) {
            Ok(persisted) => {
                let mut commanded = CommandedState::from(persisted);
                let before = commanded.set_speed;
                commanded.set_speed = migrate_spd(before);
                state.commanded = commanded;
                if before == state.commanded.set_speed {
                    info!(path = %path.display(), "loaded persisted commanded state");
                } else {
                    info!(
                        path = %path.display(),
                        before,
                        after = state.commanded.set_speed,
                        "migrated persisted spd to 0..=35 scale"
                    );
                    if let Err(err) = save(state, path) {
                        warn!(
                            path = %path.display(),
                            error = %err,
                            "failed to rewrite migrated spd"
                        );
                    }
                }
            }
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "persisted state corrupt; using defaults"
                );
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            info!(
                path = %path.display(),
                "no persisted state; using defaults"
            );
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "failed to read persisted state; using defaults"
            );
        }
    }
}

/// Atomically write commanded settings (`tmp` + rename).
pub fn save(state: &PoolState, path: &Path) -> Result<(), PersistError> {
    let persisted = PersistedState::from(&state.commanded);
    let json = serde_json::to_vec_pretty(&persisted)?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| PersistError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let tmp = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&tmp).map_err(|source| PersistError::Write {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(&json)
            .map_err(|source| PersistError::Write {
                path: tmp.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| PersistError::Write {
            path: tmp.clone(),
            source,
        })?;
    }

    fs::rename(&tmp, path).map_err(|source| PersistError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{encode_spd, migrate_spd, pack_relays};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rs_pool_{label}_{nanos}.json"))
    }

    #[test]
    fn persistence_roundtrip() {
        let path = temp_path("roundtrip");
        let _ = fs::remove_file(&path);

        let mut state = PoolState::default();
        state.commanded.mode = 1;
        state.commanded.set_speed = encode_spd(25, 35);
        state.commanded.set_pool_temp = 72;
        state.commanded.set_spa_temp = 100;
        state.commanded.relays =
            pack_relays([true, false, true, false, false, false, false, false]);
        // Measured must not be persisted.
        state.measured.rpm = 9999;
        state.measured.ip = "should-not-persist".into();

        save(&state, &path).expect("save");

        let mut loaded = PoolState::default();
        load_into(&mut loaded, &path);

        assert_eq!(loaded.commanded, state.commanded);
        assert_eq!(loaded.measured.rpm, 0);
        assert!(loaded.measured.ip.is_empty());

        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("rpm"));
        assert!(!raw.contains("should-not-persist"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_missing_keeps_defaults() {
        let path = temp_path("missing");
        let _ = fs::remove_file(&path);
        let mut state = PoolState::default();
        load_into(&mut state, &path);
        assert_eq!(state.commanded, CommandedState::default());
    }

    #[test]
    fn load_migrates_legacy_spd_and_rewrites() {
        let path = temp_path("migrate_spd");
        let _ = fs::remove_file(&path);
        // Old ESP ×655 encoding for ~50% → 32750
        let legacy = PersistedState {
            mode: 0,
            set_speed: 32_750,
            set_spa_temp: 104,
            set_pool_temp: 70,
            relays: 0,
        };
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let mut state = PoolState::default();
        load_into(&mut state, &path);
        assert_eq!(state.commanded.set_speed, migrate_spd(32_750));
        assert_eq!(state.commanded.set_speed, 35);

        let reloaded: PersistedState =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.set_speed, 35);

        let _ = fs::remove_file(&path);
    }
}

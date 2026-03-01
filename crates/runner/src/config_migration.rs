use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use toml_edit::{DocumentMut, value};
use tracing::info;
use types::DEFAULT_RUNNER_CONFIG_VERSION;

use crate::RunnerError;

const LEGACY_RUNNER_CONFIG_VERSION: &str = "1.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigType {
    Global,
    User,
}

struct ConfigMigration {
    from_version: &'static str,
    to_version: &'static str,
    transform: fn(&mut DocumentMut) -> Result<(), RunnerError>,
}

const GLOBAL_MIGRATIONS: &[ConfigMigration] = &[ConfigMigration {
    from_version: LEGACY_RUNNER_CONFIG_VERSION,
    to_version: DEFAULT_RUNNER_CONFIG_VERSION,
    transform: migrate_global_1_0_0_to_1_0_1,
}];

const USER_MIGRATIONS: &[ConfigMigration] = &[ConfigMigration {
    from_version: LEGACY_RUNNER_CONFIG_VERSION,
    to_version: DEFAULT_RUNNER_CONFIG_VERSION,
    transform: migrate_user_1_0_0_to_1_0_1,
}];

pub(crate) fn migrate_config_file_if_needed(
    path: &Path,
    config_type: ConfigType,
) -> Result<(), RunnerError> {
    let mut document = parse_document(path)?;
    let mut current_version = document
        .get("config_version")
        .and_then(|item| item.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| LEGACY_RUNNER_CONFIG_VERSION.to_owned());

    if current_version == DEFAULT_RUNNER_CONFIG_VERSION {
        return Ok(());
    }

    let migrations = match config_type {
        ConfigType::Global => GLOBAL_MIGRATIONS,
        ConfigType::User => USER_MIGRATIONS,
    };

    if !is_older_version(&current_version, DEFAULT_RUNNER_CONFIG_VERSION) {
        return Ok(());
    }

    let from_version = current_version.clone();
    while current_version != DEFAULT_RUNNER_CONFIG_VERSION {
        let migration = migrations
            .iter()
            .find(|migration| migration.from_version == current_version)
            .ok_or_else(|| RunnerError::ConfigMigration {
                path: path.to_path_buf(),
                message: format!(
                    "no migration registered for {} config version `{}`",
                    config_type.as_label(),
                    current_version
                ),
            })?;

        (migration.transform)(&mut document)?;
        document["config_version"] = value(migration.to_version);
        current_version = migration.to_version.to_owned();
    }

    let backup_path = backup_path_for(path);
    fs::copy(path, &backup_path).map_err(|source| RunnerError::ConfigMigrationIo {
        path: backup_path.clone(),
        operation: "backup",
        source,
    })?;
    fs::write(path, document.to_string()).map_err(|source| RunnerError::ConfigMigrationIo {
        path: path.to_path_buf(),
        operation: "write",
        source,
    })?;

    info!(
        config_type = config_type.as_label(),
        path = %path.display(),
        from_version,
        to_version = DEFAULT_RUNNER_CONFIG_VERSION,
        backup_path = %backup_path.display(),
        "auto-migrated runner config"
    );

    Ok(())
}

fn parse_document(path: &Path) -> Result<DocumentMut, RunnerError> {
    let raw = fs::read_to_string(path).map_err(|source| RunnerError::ReadConfig {
        path: path.to_path_buf(),
        source,
    })?;
    raw.parse::<DocumentMut>()
        .map_err(|source| RunnerError::ParseConfigDocument {
            path: path.to_path_buf(),
            source,
        })
}

fn backup_path_for(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    PathBuf::from(format!("{}.bak.{timestamp}", path.display()))
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts
        .next()
        .map_or(Some(0), |value| value.parse::<u64>().ok())?;
    let patch = parts
        .next()
        .map_or(Some(0), |value| value.parse::<u64>().ok())?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn is_older_version(version: &str, target: &str) -> bool {
    match (parse_version(version), parse_version(target)) {
        (Some(version), Some(target)) => version < target,
        _ => false,
    }
}

fn migrate_global_1_0_0_to_1_0_1(_: &mut DocumentMut) -> Result<(), RunnerError> {
    Ok(())
}

fn migrate_user_1_0_0_to_1_0_1(_: &mut DocumentMut) -> Result<(), RunnerError> {
    Ok(())
}

impl ConfigType {
    fn as_label(self) -> &'static str {
        match self {
            ConfigType::Global => "global",
            ConfigType::User => "user",
        }
    }
}

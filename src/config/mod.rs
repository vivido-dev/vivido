use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::result::Result as StdResult;
use std::{env, fs, io};

use log::{debug, error, info};
use serde::Deserialize;
use toml::de::Error as TomlError;
use toml::{Table, Value};

pub mod bell;
pub mod color;
pub mod cursor;
pub mod debug;
pub mod font;
pub mod general;
pub mod monitor;
pub mod scrolling;
pub mod selection;
pub mod serde_utils;
pub mod terminal;
pub mod ui_config;
pub mod window;

mod bindings;
mod mouse;

use crate::cli::Options;
#[cfg(test)]
pub use crate::config::bindings::Binding;
pub use crate::config::bindings::{
    Action, BindingKey, BindingMode, KeyBinding, MouseEvent, SearchAction,
};
pub use crate::config::ui_config::UiConfig;
use crate::logging::LOG_TARGET_CONFIG;

/// Maximum number of depth for the configuration file imports.
pub const IMPORT_RECURSION_LIMIT: usize = 5;

/// Result from config loading.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors occurring during config loading.
#[derive(Debug)]
pub enum Error {
    /// Couldn't read $HOME environment variable.
    ReadingEnvHome(env::VarError),

    /// io error reading file.
    Io(io::Error),

    /// Invalid toml.
    Toml(TomlError),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::ReadingEnvHome(err) => err.source(),
            Error::Io(err) => err.source(),
            Error::Toml(err) => err.source(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::ReadingEnvHome(err) => {
                write!(f, "Unable to read $HOME environment variable: {err}")
            },
            Error::Io(err) => write!(f, "Error reading config file: {err}"),
            Error::Toml(err) => write!(f, "Config error: {err}"),
        }
    }
}

impl From<env::VarError> for Error {
    fn from(val: env::VarError) -> Self {
        Error::ReadingEnvHome(val)
    }
}

impl From<io::Error> for Error {
    fn from(val: io::Error) -> Self {
        Error::Io(val)
    }
}

impl From<TomlError> for Error {
    fn from(val: TomlError) -> Self {
        Error::Toml(val)
    }
}

/// Load the configuration file.
pub fn load(options: &mut Options) -> UiConfig {
    let config_path = select_config_path(options.config_file.clone(), installed_config);

    // Load the config using the following fallback behavior:
    //  - Config path + CLI overrides
    //  - CLI overrides
    //  - Default
    let mut config = config_path
        .as_ref()
        .and_then(|config_path| load_from(config_path).ok())
        .unwrap_or_else(|| {
            let mut config = UiConfig::default();
            match config_path {
                Some(config_path) => config.config_paths.push(config_path),
                None => info!(target: LOG_TARGET_CONFIG, "No config file found; using default"),
            }
            config
        });

    after_loading(&mut config, options);

    config
}

fn select_config_path<F>(explicit: Option<PathBuf>, discover_installed: F) -> Option<PathBuf>
where
    F: FnOnce() -> Option<PathBuf>,
{
    explicit.or_else(discover_installed)
}

/// Attempt to reload the configuration file.
pub fn reload(config_path: &Path, options: &mut Options) -> Result<UiConfig> {
    debug!("Reloading configuration file: {config_path:?}");

    // Load config, propagating errors.
    let mut config = load_from(config_path)?;

    after_loading(&mut config, options);

    Ok(config)
}

/// Modifications after the `UiConfig` object is created.
fn after_loading(config: &mut UiConfig, options: &mut Options) {
    // Override config with CLI options.
    options.override_config(config);
}

/// Load configuration file and log errors.
fn load_from(path: &Path) -> Result<UiConfig> {
    match read_config(path) {
        Ok(config) => Ok(config),
        Err(Error::Io(io)) if io.kind() == io::ErrorKind::NotFound => {
            error!(target: LOG_TARGET_CONFIG, "Unable to load config {path:?}: File not found");
            Err(Error::Io(io))
        },
        Err(err) => {
            error!(target: LOG_TARGET_CONFIG, "Unable to load config {path:?}: {err}");
            Err(err)
        },
    }
}

/// Deserialize configuration file from path.
fn read_config(path: &Path) -> Result<UiConfig> {
    let mut config_paths = Vec::new();
    let config_value = parse_config(path, &mut config_paths, IMPORT_RECURSION_LIMIT)?;

    // Deserialize to concrete type.
    let mut config = UiConfig::deserialize(config_value)?;
    config.config_paths = config_paths;

    Ok(config)
}

/// Deserialize all configuration files as generic Value.
fn parse_config(
    path: &Path,
    config_paths: &mut Vec<PathBuf>,
    recursion_limit: usize,
) -> Result<Value> {
    config_paths.push(path.to_owned());

    // Deserialize the configuration file.
    let config = deserialize_config(path)?;

    // Merge config with imports.
    let imports = load_imports(&config, path, config_paths, recursion_limit);
    Ok(serde_utils::merge(imports, config))
}

/// Deserialize a configuration file.
pub fn deserialize_config(path: &Path) -> Result<Value> {
    let mut contents = fs::read_to_string(path)?;

    // Remove UTF-8 BOM.
    if contents.starts_with('\u{FEFF}') {
        contents = contents.split_off(3);
    }

    // Load configuration file as Value.
    let config: Value = toml::from_str(&contents)?;

    Ok(config)
}

/// Load all referenced configuration files.
fn load_imports(
    config: &Value,
    base_path: &Path,
    config_paths: &mut Vec<PathBuf>,
    recursion_limit: usize,
) -> Value {
    // Get paths for all imports.
    let import_paths = match imports(config, base_path, recursion_limit) {
        Ok(import_paths) => import_paths,
        Err(err) => {
            error!(target: LOG_TARGET_CONFIG, "{err}");
            return Value::Table(Table::new());
        },
    };

    // Parse configs for all imports recursively.
    let mut merged = Value::Table(Table::new());
    for import_path in import_paths {
        let path = match import_path {
            Ok(path) => path,
            Err(err) => {
                error!(target: LOG_TARGET_CONFIG, "{err}");
                continue;
            },
        };

        match parse_config(&path, config_paths, recursion_limit - 1) {
            Ok(config) => merged = serde_utils::merge(merged, config),
            Err(Error::Io(io)) if io.kind() == io::ErrorKind::NotFound => {
                info!(target: LOG_TARGET_CONFIG, "Config import not found:\n  {:?}", path.display());
                continue;
            },
            Err(err) => {
                error!(target: LOG_TARGET_CONFIG, "Unable to import config {path:?}: {err}")
            },
        }
    }

    merged
}

/// Get all import paths for a configuration.
pub fn imports(
    config: &Value,
    base_path: &Path,
    recursion_limit: usize,
) -> StdResult<Vec<StdResult<PathBuf, String>>, String> {
    let imports =
        config.get("import").or_else(|| config.get("general").and_then(|g| g.get("import")));
    let imports = match imports {
        Some(Value::Array(imports)) => imports,
        Some(_) => return Err("Invalid import type: expected a sequence".into()),
        None => return Ok(Vec::new()),
    };

    // Limit recursion to prevent infinite loops.
    if !imports.is_empty() && recursion_limit == 0 {
        return Err("Exceeded maximum configuration import depth".into());
    }

    let mut import_paths = Vec::new();

    for import in imports {
        let path = match import {
            Value::String(path) => PathBuf::from(path),
            _ => {
                import_paths.push(Err("Invalid import element type: expected path string".into()));
                continue;
            },
        };

        let normalized = normalize_import(base_path, path);

        import_paths.push(Ok(normalized));
    }

    Ok(import_paths)
}

/// Normalize import paths.
pub fn normalize_import(base_config_path: &Path, import_path: impl Into<PathBuf>) -> PathBuf {
    let mut import_path = import_path.into();

    // Resolve paths relative to user's home directory.
    if let (Ok(stripped), Some(home_dir)) = (import_path.strip_prefix("~/"), home::home_dir()) {
        import_path = home_dir.join(stripped);
    }

    if import_path.is_relative()
        && let Some(base_config_dir) = base_config_path.parent()
    {
        import_path = base_config_dir.join(import_path)
    }

    import_path
}

/// Get the location of the first found default config file paths
/// according to the following order:
///
/// 1. $XDG_CONFIG_HOME/vivido/vivido.toml
/// 2. $XDG_CONFIG_HOME/vivido.toml
/// 3. $HOME/.config/vivido/vivido.toml
/// 4. $HOME/.vivido.toml
/// 5. /etc/vivido/vivido.toml
#[cfg(not(windows))]
pub fn installed_config() -> Option<PathBuf> {
    let file_name = String::from("vivido.toml");

    // Try using XDG location by default.
    xdg::BaseDirectories::with_prefix("vivido")
        .find_config_file(&file_name)
        .or_else(|| xdg::BaseDirectories::new().find_config_file(&file_name))
        .or_else(|| {
            if let Ok(home) = env::var("HOME") {
                // Fallback path: $HOME/.config/vivido/vivido.toml.
                let fallback = PathBuf::from(&home).join(".config/vivido").join(&file_name);
                if fallback.exists() {
                    return Some(fallback);
                }
                // Fallback path: $HOME/.vivido.toml.
                let hidden_name = format!(".{file_name}");
                let fallback = PathBuf::from(&home).join(hidden_name);
                if fallback.exists() {
                    return Some(fallback);
                }
            }

            let fallback = PathBuf::from("/etc/vivido").join(&file_name);
            fallback.exists().then_some(fallback)
        })
}

#[cfg(windows)]
pub fn installed_config() -> Option<PathBuf> {
    let user_profile = env::var_os("USERPROFILE").map(PathBuf::from).or_else(home::home_dir);
    first_existing_windows_config(windows_config_candidates(user_profile, dirs::config_dir()))
}

#[cfg(any(windows, test))]
fn first_existing_windows_config(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| path.exists())
}

/// Return Windows configuration candidates in lookup order.
///
/// `%USERPROFILE%\vivido` is authoritative for new installs. The roaming
/// `%APPDATA%\vivido` location remains a read-only compatibility fallback.
#[cfg(any(windows, test))]
fn windows_config_candidates(
    home_dir: Option<PathBuf>,
    roaming_config_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let file_name = "vivido.toml";
    let mut candidates = Vec::with_capacity(2);

    if let Some(home_dir) = home_dir {
        candidates.push(home_dir.join("vivido").join(file_name));
    }
    if let Some(roaming_config_dir) = roaming_config_dir {
        let legacy = roaming_config_dir.join("vivido").join(file_name);
        if !candidates.contains(&legacy) {
            candidates.push(legacy);
        }
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn empty_config() {
        toml::from_str::<UiConfig>("").unwrap();
    }

    #[test]
    fn explicit_config_has_precedence_without_discovery() {
        let discovery_called = Cell::new(false);
        let explicit = PathBuf::from("explicit.toml");
        let selected = select_config_path(Some(explicit.clone()), || {
            discovery_called.set(true);
            Some(PathBuf::from("installed.toml"))
        });

        assert_eq!(selected, Some(explicit));
        assert!(!discovery_called.get());
    }

    #[test]
    fn malformed_config_returns_parse_error_for_unicode_path() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let directory = env::temp_dir().join(format!("vivido-config-José Example-{nonce}"));
        let path = directory.join("vivido.toml");
        fs::create_dir(&directory).unwrap();
        fs::write(&path, "[terminal\nshell =").unwrap();

        assert!(matches!(deserialize_config(&path), Err(Error::Toml(_))));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn windows_config_prefers_user_profile_and_keeps_legacy_fallback() {
        let home = PathBuf::from("profile/José Example");
        let roaming = PathBuf::from("profile/José Example/AppData/Roaming");
        let candidates = windows_config_candidates(Some(home.clone()), Some(roaming.clone()));

        assert_eq!(
            candidates,
            [home.join("vivido").join("vivido.toml"), roaming.join("vivido").join("vivido.toml"),]
        );
    }

    #[test]
    fn windows_config_discovers_new_path_before_legacy_and_falls_back() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = env::temp_dir().join(format!("vivido-config-discovery-José-{nonce}"));
        let profile = root.join("profile");
        let roaming = root.join("roaming");
        let new_path = profile.join("vivido/vivido.toml");
        let legacy_path = roaming.join("vivido/vivido.toml");
        fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        fs::write(&legacy_path, "legacy = true\n").unwrap();

        let candidates = windows_config_candidates(Some(profile.clone()), Some(roaming.clone()));
        assert_eq!(first_existing_windows_config(candidates), Some(legacy_path.clone()));

        fs::write(&new_path, "new = true\n").unwrap();
        let candidates = windows_config_candidates(Some(profile), Some(roaming));
        assert_eq!(first_existing_windows_config(candidates), Some(new_path));
        fs::remove_dir_all(root).unwrap();
    }
}

use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::result::Result as StdResult;
use std::{env, fs, io};

use log::{debug, error, info, warn};
use serde::Deserialize;
use serde_yaml::Error as YamlError;
use toml::de::Error as TomlError;
use toml::ser::Error as TomlSeError;
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

    /// Failed toml serialization.
    TomlSe(TomlSeError),

    /// Invalid yaml.
    Yaml(YamlError),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::ReadingEnvHome(err) => err.source(),
            Error::Io(err) => err.source(),
            Error::Toml(err) => err.source(),
            Error::TomlSe(err) => err.source(),
            Error::Yaml(err) => err.source(),
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
            Error::TomlSe(err) => write!(f, "Yaml conversion error: {err}"),
            Error::Yaml(err) => write!(f, "Config error: {err}"),
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

impl From<TomlSeError> for Error {
    fn from(val: TomlSeError) -> Self {
        Error::TomlSe(val)
    }
}

impl From<YamlError> for Error {
    fn from(val: YamlError) -> Self {
        Error::Yaml(val)
    }
}

/// Load the configuration file.
pub fn load(options: &mut Options) -> UiConfig {
    let config_path = select_config_path(options.config_file.clone(), || {
        installed_config("toml").or_else(|| installed_config("yml"))
    });

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
    let config = deserialize_config(path, false)?;

    // Merge config with imports.
    let imports = load_imports(&config, path, config_paths, recursion_limit);
    Ok(serde_utils::merge(imports, config))
}

/// Deserialize a configuration file.
pub fn deserialize_config(path: &Path, warn_pruned: bool) -> Result<Value> {
    let mut contents = fs::read_to_string(path)?;

    // Remove UTF-8 BOM.
    if contents.starts_with('\u{FEFF}') {
        contents = contents.split_off(3);
    }

    // Convert YAML to TOML as a transitionary fallback mechanism.
    let extension = path.extension().unwrap_or_default();
    if (extension == "yaml" || extension == "yml") && !contents.trim().is_empty() {
        warn!("YAML config {path:?} is deprecated, please migrate to TOML using `vivido migrate`");

        let mut value: serde_yaml::Value = serde_yaml::from_str(&contents)?;
        prune_yaml_nulls(&mut value, warn_pruned);
        contents = toml::to_string(&value)?;
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

/// Prune the nulls from the YAML to ensure TOML compatibility.
fn prune_yaml_nulls(value: &mut serde_yaml::Value, warn_pruned: bool) {
    fn walk(value: &mut serde_yaml::Value, warn_pruned: bool) -> bool {
        match value {
            serde_yaml::Value::Sequence(sequence) => {
                sequence.retain_mut(|value| !walk(value, warn_pruned));
                sequence.is_empty()
            },
            serde_yaml::Value::Mapping(mapping) => {
                mapping.retain(|key, value| {
                    let retain = !walk(value, warn_pruned);
                    if let Some(key_name) = key.as_str().filter(|_| !retain && warn_pruned) {
                        eprintln!("Removing null key \"{key_name}\" from the end config");
                    }
                    retain
                });
                mapping.is_empty()
            },
            serde_yaml::Value::Null => true,
            _ => false,
        }
    }

    if walk(value, warn_pruned) {
        // When the value itself is null return the mapping.
        *value = serde_yaml::Value::Mapping(Default::default());
    }
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
pub fn installed_config(suffix: &str) -> Option<PathBuf> {
    let file_name = format!("vivido.{suffix}");

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
pub fn installed_config(suffix: &str) -> Option<PathBuf> {
    let user_profile = env::var_os("USERPROFILE").map(PathBuf::from).or_else(home::home_dir);
    first_existing_windows_config(windows_config_candidates(
        user_profile,
        dirs::config_dir(),
        suffix,
    ))
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
    suffix: &str,
) -> Vec<PathBuf> {
    let file_name = format!("vivido.{suffix}");
    let mut candidates = Vec::with_capacity(2);

    if suffix == "toml"
        && let Some(home_dir) = home_dir
    {
        candidates.push(home_dir.join("vivido").join(&file_name));
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

        assert!(matches!(deserialize_config(&path, false), Err(Error::Toml(_))));
        fs::remove_dir_all(directory).unwrap();
    }

    fn yaml_to_toml(contents: &str) -> String {
        let mut value: serde_yaml::Value = serde_yaml::from_str(contents).unwrap();
        prune_yaml_nulls(&mut value, false);
        toml::to_string(&value).unwrap()
    }

    #[test]
    fn yaml_with_nulls() {
        let contents = r#"
        window:
            blinking: Always
            cursor:
            not_blinking: Always
            some_array:
              - { window: }
              - { window: "Hello" }

        "#;
        let toml = yaml_to_toml(contents);
        assert_eq!(
            toml.trim(),
            r#"[window]
blinking = "Always"
not_blinking = "Always"

[[window.some_array]]
window = "Hello""#
        );
    }

    #[test]
    fn empty_yaml_to_toml() {
        let contents = r#"

        "#;
        let toml = yaml_to_toml(contents);
        assert!(toml.is_empty());
    }

    #[test]
    fn windows_config_prefers_user_profile_and_keeps_legacy_fallback() {
        let home = PathBuf::from("profile/José Example");
        let roaming = PathBuf::from("profile/José Example/AppData/Roaming");
        let candidates =
            windows_config_candidates(Some(home.clone()), Some(roaming.clone()), "toml");

        assert_eq!(
            candidates,
            [home.join("vivido").join("vivido.toml"), roaming.join("vivido").join("vivido.toml"),]
        );
    }

    #[test]
    fn windows_yaml_only_uses_legacy_roaming_location() {
        let home = PathBuf::from("profile/Example");
        let roaming = home.join("AppData/Roaming");
        let candidates = windows_config_candidates(Some(home), Some(roaming.clone()), "yml");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], roaming.join("vivido").join("vivido.yml"));
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

        let candidates =
            windows_config_candidates(Some(profile.clone()), Some(roaming.clone()), "toml");
        assert_eq!(first_existing_windows_config(candidates), Some(legacy_path.clone()));

        fs::write(&new_path, "new = true\n").unwrap();
        let candidates = windows_config_candidates(Some(profile), Some(roaming), "toml");
        assert_eq!(first_existing_windows_config(candidates), Some(new_path));
        fs::remove_dir_all(root).unwrap();
    }
}

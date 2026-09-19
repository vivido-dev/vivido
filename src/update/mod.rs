//! Bounded, platform-aware in-app update discovery and download support.

mod install;

use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::event::{Event, EventSink, EventType};

pub use install::launch_installer;

const MANIFEST_LIMIT: u64 = 64 * 1024;
const INSTALLER_LIMIT: u64 = 512 * 1024 * 1024;
const DOWNLOAD_CHUNK: usize = 64 * 1024;
const MAX_ASSET_NAME_BYTES: usize = 255;
const MAX_PUBLISHER_BYTES: usize = 256;
const MAX_STALE_ENTRIES: usize = 256;
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_UPDATE_BASE_URL: &str =
    "https://github.com/vivido-dev/vivido/releases/latest/download";
const UPDATE_BASE_URL_ENV: &str = "VIVIDO_UPDATE_BASE_URL";
const COMPILED_UPDATE_BASE_URL: Option<&str> = option_env!("VIVIDO_UPDATE_BASE_URL");
const STATE_FILE_NAME: &str = "update-state.toml";

/// A platform-specific update manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateManifest {
    pub schema: u16,
    pub product: String,
    pub version: Version,
    pub published_utc: String,
    pub notes_url: Option<String>,
    pub asset: UpdateAsset,
}

/// The installer artifact described by an [`UpdateManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateAsset {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
    pub kind: String,
    pub publisher: Option<String>,
    pub team_id: Option<String>,
}

/// Events emitted by update workers and consumed by the application processor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    CheckRequested,
    Available {
        manifest: Box<UpdateManifest>,
        version: Version,
        bytes: u64,
        notes_url: Option<String>,
        manual: bool,
    },
    UpToDate {
        current: Version,
    },
    Progress {
        version: Version,
        downloaded: u64,
        total: u64,
    },
    Ready {
        version: Version,
        path: PathBuf,
    },
    InstallRequested,
    Skip {
        version: Version,
    },
    Failed {
        message: String,
        manual: bool,
    },
}

/// Failure returned while validating, discovering, downloading, or verifying an update.
#[derive(Debug)]
pub enum UpdateError {
    InvalidManifest(String),
    Http(String),
    Verification(String),
    UnsupportedPlatform,
    Cancelled,
    Io(io::Error),
    Json(serde_json::Error),
}

impl UpdateError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidManifest(message.into())
    }

    fn verification(message: impl Into<String>) -> Self {
        Self::Verification(message.into())
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifest(message) => {
                write!(formatter, "invalid update manifest: {message}")
            },
            Self::Http(message) => write!(formatter, "update request failed: {message}"),
            Self::Verification(message) => {
                write!(formatter, "installer verification failed: {message}")
            },
            Self::UnsupportedPlatform => {
                formatter.write_str("in-app updates are unsupported on this platform")
            },
            Self::Cancelled => formatter.write_str("update download was cancelled"),
            Self::Io(error) => write!(formatter, "update I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "update manifest JSON is invalid: {error}"),
        }
    }
}

impl Error for UpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidManifest(_)
            | Self::Http(_)
            | Self::Verification(_)
            | Self::UnsupportedPlatform
            | Self::Cancelled => None,
        }
    }
}

impl From<io::Error> for UpdateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for UpdateError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Validate all security- and resource-relevant manifest fields.
pub fn validate(manifest: &UpdateManifest) -> Result<(), UpdateError> {
    if manifest.schema != 1 {
        return Err(UpdateError::invalid("unsupported schema"));
    }
    if manifest.product != "vivido" {
        return Err(UpdateError::invalid("unexpected product"));
    }
    if manifest.published_utc.is_empty() || manifest.published_utc.len() > 64 {
        return Err(UpdateError::invalid("invalid publication timestamp"));
    }
    if let Some(notes_url) = manifest.notes_url.as_deref()
        && !is_https_url(notes_url)
    {
        return Err(UpdateError::invalid("release notes URL must use HTTPS"));
    }

    let asset = &manifest.asset;
    if !is_safe_asset_name(&asset.name) {
        return Err(UpdateError::invalid("asset name is not a bounded file name"));
    }
    if !is_https_url(&asset.url) {
        return Err(UpdateError::invalid("asset URL must use HTTPS"));
    }
    if asset.sha256.len() != 64
        || !asset.sha256.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(UpdateError::invalid(
            "asset SHA-256 must be 64 lowercase hexadecimal characters",
        ));
    }
    if asset.bytes == 0 || asset.bytes > INSTALLER_LIMIT {
        return Err(UpdateError::invalid("asset size is outside the supported range"));
    }

    match asset.kind.as_str() {
        "msi" => {
            let publisher = asset
                .publisher
                .as_deref()
                .filter(|value| !value.trim().is_empty() && value.len() <= MAX_PUBLISHER_BYTES)
                .ok_or_else(|| UpdateError::invalid("MSI asset requires a bounded publisher"))?;
            if asset.team_id.is_some()
                || !asset.name.to_ascii_lowercase().ends_with(".msi")
                || publisher.contains(['\r', '\n', '\0'])
            {
                return Err(UpdateError::invalid("MSI asset metadata is inconsistent"));
            }
        },
        "pkg" => {
            let team_id = asset
                .team_id
                .as_deref()
                .ok_or_else(|| UpdateError::invalid("PKG asset requires a Team ID"))?;
            if asset.publisher.is_some()
                || !asset.name.to_ascii_lowercase().ends_with(".pkg")
                || team_id.len() != 10
                || !team_id.bytes().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            {
                return Err(UpdateError::invalid("PKG asset metadata is inconsistent"));
            }
        },
        _ => return Err(UpdateError::invalid("unsupported installer kind")),
    }

    Ok(())
}

/// Return the update feed platform identifier for this build.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn platform() -> &'static str {
    "windows-x64"
}

/// Return the update feed platform identifier for this build.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn platform() -> &'static str {
    "macos-arm64"
}

/// Return the update feed platform identifier for this build.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn platform() -> &'static str {
    "macos-x86_64"
}

/// Return an unsupported marker on platforms without a suite installer.
#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64")
)))]
pub fn platform() -> &'static str {
    "unsupported"
}

/// Return the suite version compiled into this Vivido build.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION must be a valid semantic version")
}

/// Start a bounded background update check.
pub fn spawn_check(sink: EventSink, manual: bool) {
    let worker_sink = sink.clone();
    let spawn = thread::Builder::new().name("vivido-update-check".into()).spawn(move || {
        let _ = cleanup_stale_updates();
        match fetch_manifest() {
            Ok(manifest) if is_newer_version(&manifest.version, &current_version()) => {
                let event = UpdateEvent::Available {
                    version: manifest.version.clone(),
                    bytes: manifest.asset.bytes,
                    notes_url: manifest.notes_url.clone(),
                    manifest: Box::new(manifest),
                    manual,
                };
                send_update(&worker_sink, event);
            },
            Ok(_) => {
                send_update(&worker_sink, UpdateEvent::UpToDate { current: current_version() });
            },
            Err(error) => {
                send_update(
                    &worker_sink,
                    UpdateEvent::Failed { message: error.to_string(), manual },
                );
            },
        }
    });
    if let Err(error) = spawn {
        send_update(
            &sink,
            UpdateEvent::Failed {
                message: format!("could not start update check: {error}"),
                manual,
            },
        );
    }
}

/// Start a bounded background installer download and verification.
pub fn spawn_download(sink: EventSink, manifest: UpdateManifest, cancel: Arc<AtomicBool>) {
    let worker_sink = sink.clone();
    let spawn = thread::Builder::new().name("vivido-update-download".into()).spawn(move || {
        match download_and_verify(&worker_sink, &manifest, &cancel) {
            Ok(path) => {
                send_update(&worker_sink, UpdateEvent::Ready { version: manifest.version, path })
            },
            Err(UpdateError::Cancelled) => {},
            Err(error) => send_update(
                &worker_sink,
                UpdateEvent::Failed { message: error.to_string(), manual: true },
            ),
        }
    });
    if let Err(error) = spawn {
        send_update(
            &sink,
            UpdateEvent::Failed {
                message: format!("could not start update download: {error}"),
                manual: true,
            },
        );
    }
}

/// Read the update version suppressed by the user, if the state file is valid.
pub fn read_skipped_version() -> Option<Version> {
    let path = state_path()?;
    read_skipped_version_from(&path).ok().flatten()
}

/// Atomically persist the update version suppressed by the user.
pub fn write_skipped_version(version: &Version) -> io::Result<()> {
    let path = state_path().ok_or_else(|| {
        io::Error::other("could not determine the Vivido configuration directory")
    })?;
    write_skipped_version_to(&path, version)
}

/// Return whether a discovered version should be offered to the user.
pub fn should_offer_update(version: &Version, skipped: Option<&Version>, manual: bool) -> bool {
    is_newer_version(version, &current_version()) && (manual || skipped != Some(version))
}

fn is_newer_version(candidate: &Version, current: &Version) -> bool {
    candidate > current
}

fn is_https_url(url: &str) -> bool {
    let Some(authority_and_path) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = authority_and_path.split('/').next().unwrap_or_default();
    !authority.is_empty() && !authority.contains(char::is_whitespace)
}

fn is_safe_asset_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > MAX_ASSET_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn fetch_manifest() -> Result<UpdateManifest, UpdateError> {
    let url = manifest_url()?;
    let agent = http_agent(CHECK_TIMEOUT);
    let mut response =
        agent.get(&url).call().map_err(|error| UpdateError::Http(error.to_string()))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MANIFEST_LIMIT)
        .read_to_vec()
        .map_err(|error| UpdateError::Http(error.to_string()))?;
    let manifest: UpdateManifest = serde_json::from_slice(&bytes)?;
    validate(&manifest)?;
    Ok(manifest)
}

fn manifest_url() -> Result<String, UpdateError> {
    let base = std::env::var(UPDATE_BASE_URL_ENV)
        .ok()
        .or_else(|| COMPILED_UPDATE_BASE_URL.map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_UPDATE_BASE_URL.to_owned());
    let base = base.trim_end_matches('/');
    if !is_https_url(base) {
        return Err(UpdateError::invalid("update base URL must use HTTPS"));
    }
    let platform = platform();
    if platform == "unsupported" {
        return Err(UpdateError::UnsupportedPlatform);
    }
    Ok(format!("{base}/vivido-update-{platform}.json"))
}

fn http_agent(overall_timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(overall_timeout))
        .build()
        .new_agent()
}

fn download_and_verify(
    sink: &EventSink,
    manifest: &UpdateManifest,
    cancel: &AtomicBool,
) -> Result<PathBuf, UpdateError> {
    validate(manifest)?;
    if cancel.load(Ordering::Acquire) {
        return Err(UpdateError::Cancelled);
    }

    let agent = http_agent(DOWNLOAD_TIMEOUT);
    let mut response = agent
        .get(&manifest.asset.url)
        .call()
        .map_err(|error| UpdateError::Http(error.to_string()))?;
    let read_limit = manifest
        .asset
        .bytes
        .checked_add(1)
        .ok_or_else(|| UpdateError::invalid("asset size overflow"))?;
    let mut reader = response.body_mut().with_config().limit(read_limit).reader();
    let temporary = tempfile::Builder::new().prefix("vivido-update-").tempdir()?;
    let installer_path = temporary.path().join(&manifest.asset.name);
    let mut installer = File::create(&installer_path)?;
    let mut digest = Sha256::new();
    let mut downloaded = 0_u64;
    let mut buffer = [0_u8; DOWNLOAD_CHUNK];
    let mut throttle = ProgressThrottle::new(Instant::now());
    send_update(
        sink,
        UpdateEvent::Progress {
            version: manifest.version.clone(),
            downloaded,
            total: manifest.asset.bytes,
        },
    );

    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(UpdateError::Cancelled);
        }
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let count_u64 = u64::try_from(count)
            .map_err(|_| UpdateError::invalid("download chunk length overflow"))?;
        downloaded = downloaded
            .checked_add(count_u64)
            .ok_or_else(|| UpdateError::invalid("download length overflow"))?;
        if downloaded > manifest.asset.bytes {
            return Err(UpdateError::verification(
                "installer is larger than the manifest byte size",
            ));
        }
        installer.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);

        if throttle.should_emit(downloaded, manifest.asset.bytes, Instant::now()) {
            send_update(
                sink,
                UpdateEvent::Progress {
                    version: manifest.version.clone(),
                    downloaded,
                    total: manifest.asset.bytes,
                },
            );
        }
    }
    installer.flush()?;
    installer.sync_all()?;
    drop(installer);

    if downloaded != manifest.asset.bytes {
        return Err(UpdateError::verification(format!(
            "installer byte size mismatch: expected {}, received {downloaded}",
            manifest.asset.bytes
        )));
    }
    if hex::encode(digest.finalize()) != manifest.asset.sha256 {
        return Err(UpdateError::verification("installer SHA-256 mismatch"));
    }
    verify_downloaded_installer(&installer_path, &manifest.asset)?;

    let retained = temporary.keep();
    Ok(retained.join(&manifest.asset.name))
}

#[cfg(windows)]
fn verify_downloaded_installer(path: &Path, asset: &UpdateAsset) -> Result<(), UpdateError> {
    let publisher = asset
        .publisher
        .as_deref()
        .ok_or_else(|| UpdateError::invalid("MSI publisher is missing"))?;
    install::verify_installer(path, publisher)
}

#[cfg(target_os = "macos")]
fn verify_downloaded_installer(path: &Path, asset: &UpdateAsset) -> Result<(), UpdateError> {
    let team_id =
        asset.team_id.as_deref().ok_or_else(|| UpdateError::invalid("PKG Team ID is missing"))?;
    install::verify_installer(path, team_id)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn verify_downloaded_installer(_path: &Path, _asset: &UpdateAsset) -> Result<(), UpdateError> {
    Err(UpdateError::UnsupportedPlatform)
}

fn send_update(sink: &EventSink, event: UpdateEvent) {
    let _ = sink.send_event(Event::new(EventType::Update(event), None));
}

fn cleanup_stale_updates() -> io::Result<usize> {
    cleanup_stale_updates_in(&std::env::temp_dir(), SystemTime::now())
}

fn cleanup_stale_updates_in(directory: &Path, now: SystemTime) -> io::Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(directory)?.take(MAX_STALE_ENTRIES) {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(file_type) = entry.file_type() else { continue };
        let Ok(metadata) = entry.metadata() else { continue };
        let Ok(modified) = metadata.modified() else { continue };
        if file_type.is_dir()
            && is_stale_update_entry(name, modified, now)
            && fs::remove_dir_all(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    Ok(removed)
}

fn is_stale_update_entry(name: &str, modified: SystemTime, now: SystemTime) -> bool {
    name.starts_with("vivido-update-")
        && now.duration_since(modified).is_ok_and(|age| age >= STALE_AFTER)
}

#[derive(Debug)]
struct ProgressThrottle {
    last_emit: Instant,
}

impl ProgressThrottle {
    fn new(now: Instant) -> Self {
        Self { last_emit: now }
    }

    fn should_emit(&mut self, downloaded: u64, total: u64, now: Instant) -> bool {
        if downloaded >= total || now.saturating_duration_since(self.last_emit) >= PROGRESS_INTERVAL
        {
            self.last_emit = now;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateState {
    skipped_version: Version,
}

fn state_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|directory| directory.join(STATE_FILE_NAME))
}

fn read_skipped_version_from(path: &Path) -> io::Result<Option<Version>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let state: UpdateState = toml::from_str(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some(state.skipped_version))
}

fn write_skipped_version_to(path: &Path, version: &Version) -> io::Result<()> {
    let parent =
        path.parent().ok_or_else(|| io::Error::other("update state path has no parent"))?;
    fs::create_dir_all(parent)?;
    let contents = toml::to_string(&UpdateState { skipped_version: version.clone() })
        .map_err(io::Error::other)?;
    let mut temporary =
        tempfile::Builder::new().prefix(".update-state-").suffix(".tmp").tempfile_in(parent)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_directory(parent)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_MANIFEST: &str = r#"{
        "schema": 1,
        "product": "vivido",
        "version": "0.4.10",
        "publishedUtc": "2026-09-18T12:00:00Z",
        "notesUrl": "https://github.com/vivido-dev/vivido/releases/tag/vivido-v0.4.10",
        "asset": {
            "name": "Vivido-0.4.10-x64.msi",
            "url": "https://github.com/vivido-dev/vivido/releases/download/vivido-v0.4.10/Vivido-0.4.10-x64.msi",
            "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "bytes": 104857600,
            "kind": "msi",
            "publisher": "Wensheng Wang"
        }
    }"#;

    fn manifest() -> UpdateManifest {
        serde_json::from_str(VALID_MANIFEST).expect("valid manifest fixture")
    }

    #[test]
    fn accepts_valid_manifest_fixture() {
        assert!(validate(&manifest()).is_ok());
    }

    #[test]
    fn rejects_unknown_manifest_fields() {
        let json = VALID_MANIFEST.replace("\"schema\": 1,", "\"schema\": 1, \"extra\": true,");
        assert!(serde_json::from_str::<UpdateManifest>(&json).is_err());
    }

    #[test]
    fn rejects_non_https_urls() {
        let mut value = manifest();
        value.notes_url = Some("http://example.test/notes".into());
        assert!(validate(&value).is_err());
        value.notes_url = None;
        value.asset.url = "http://example.test/installer.msi".into();
        assert!(validate(&value).is_err());
    }

    #[test]
    fn rejects_bad_sha256_values() {
        for sha256 in [
            "abcd",
            "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let mut value = manifest();
            value.asset.sha256 = sha256.into();
            assert!(validate(&value).is_err(), "accepted {sha256}");
        }
    }

    #[test]
    fn rejects_zero_and_oversize_assets() {
        for bytes in [0, INSTALLER_LIMIT + 1] {
            let mut value = manifest();
            value.asset.bytes = bytes;
            assert!(validate(&value).is_err());
        }
    }

    #[test]
    fn rejects_wrong_schema_product_kind_and_asset_paths() {
        let mut value = manifest();
        value.schema = 2;
        assert!(validate(&value).is_err());

        let mut value = manifest();
        value.product = "other".into();
        assert!(validate(&value).is_err());

        let mut value = manifest();
        value.asset.kind = "exe".into();
        assert!(validate(&value).is_err());

        for name in [
            "../installer.msi",
            "dir/installer.msi",
            "dir\\installer.msi",
            "installer:stream.msi",
            ".",
        ] {
            let mut value = manifest();
            value.asset.name = name.into();
            assert!(validate(&value).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn validates_kind_specific_identity_fields() {
        let mut value = manifest();
        value.asset.publisher = None;
        assert!(validate(&value).is_err());

        let mut value = manifest();
        value.asset.team_id = Some("TEAMID1234".into());
        assert!(validate(&value).is_err());

        let mut value = manifest();
        value.asset.name = "Vivido-0.4.10-macos-arm64.pkg".into();
        value.asset.kind = "pkg".into();
        value.asset.publisher = None;
        value.asset.team_id = Some("TEAMID1234".into());
        assert!(validate(&value).is_ok());
    }

    #[test]
    fn compares_newer_equal_and_older_versions() {
        let current = Version::new(2, 0, 0);
        assert!(is_newer_version(&Version::new(2, 0, 1), &current));
        assert!(!is_newer_version(&Version::new(2, 0, 0), &current));
        assert!(!is_newer_version(&Version::new(1, 9, 9), &current));
    }

    #[test]
    fn skipped_version_only_suppresses_quiet_offer() {
        let version = Version::new(99, 0, 0);
        assert!(!should_offer_update(&version, Some(&version), false));
        assert!(should_offer_update(&version, Some(&version), true));
    }

    #[test]
    fn state_file_round_trip_is_atomic_and_leaves_no_residue() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(STATE_FILE_NAME);
        let version = Version::new(3, 2, 1);
        write_skipped_version_to(&path, &version).expect("write skipped version");
        assert_eq!(read_skipped_version_from(&path).unwrap(), Some(version));
        let residue = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".update-state-"))
            .count();
        assert_eq!(residue, 0);
    }

    #[test]
    fn progress_is_time_throttled_but_always_reports_completion() {
        let start = Instant::now();
        let mut throttle = ProgressThrottle::new(start);
        assert!(!throttle.should_emit(10, 100, start + Duration::from_millis(99)));
        assert!(throttle.should_emit(20, 100, start + Duration::from_millis(100)));
        assert!(!throttle.should_emit(99, 100, start + Duration::from_millis(150)));
        assert!(throttle.should_emit(100, 100, start + Duration::from_millis(151)));
    }

    #[test]
    fn stale_cleanup_selection_requires_prefix_and_age() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3 * 24 * 60 * 60);
        let old = now - STALE_AFTER;
        let recent = now - Duration::from_secs(60);
        assert!(is_stale_update_entry("vivido-update-test", old, now));
        assert!(!is_stale_update_entry("other-update-test", old, now));
        assert!(!is_stale_update_entry("vivido-update-test", recent, now));
    }
}

//! Persistent configuration: the camera list, mosaic slot assignments and app preferences.
//!
//! Stored as TOML at `~/Library/Application Support/camfast/config.toml` with mode 0600,
//! written atomically (temp file + rename) so a crash never leaves a half-written file.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

pub const SLOT_COUNT: usize = 4;
pub const DEFAULT_RTSP_PORT: u16 = 554;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("I/O error at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("Invalid config.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("Failed to serialize the configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("{0}")]
    Invalid(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> ConfigError + '_ {
    move |source| ConfigError::Io { path: path.to_path_buf(), source }
}

/// Stable identity of a camera. Slots reference cameras by id, so renaming never breaks them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CameraId(pub Uuid);

impl CameraId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CameraId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CameraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.0.simple().to_string()[..8])
    }
}

impl fmt::Display for CameraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// A password that never shows up in `Debug`/`Display` output (and therefore never in logs).
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Which RTSP stream of the camera to pull (Dahua/Intelbras `subtype`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    #[default]
    Main,
    Sub,
}

impl StreamKind {
    pub fn subtype(self) -> u8 {
        match self {
            StreamKind::Main => 0,
            StreamKind::Sub => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StreamKind::Main => "Main",
            StreamKind::Sub => "Sub",
        }
    }
}

fn default_port() -> u16 {
    DEFAULT_RTSP_PORT
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraConfig {
    #[serde(default)]
    pub id: CameraId,
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub stream: StreamKind,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: Secret,
}

impl CameraConfig {
    pub fn new(name: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            id: CameraId::new(),
            name: name.into(),
            host: host.into(),
            port: DEFAULT_RTSP_PORT,
            stream: StreamKind::Main,
            username: String::new(),
            password: Secret::default(),
        }
    }

    /// RTSP URL without credentials (credentials are passed separately so they never get logged).
    pub fn rtsp_url(&self) -> String {
        format!(
            "rtsp://{}:{}/cam/realmonitor?channel=1&subtype={}",
            self.host.trim(),
            self.port,
            self.stream.subtype()
        )
    }

    /// True when a running stream must be restarted for this change to take effect.
    pub fn connection_differs(&self, other: &CameraConfig) -> bool {
        self.host != other.host
            || self.port != other.port
            || self.stream != other.stream
            || self.username != other.username
            || self.password != other.password
    }
}

impl fmt::Debug for CameraConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CameraConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("stream", &self.stream)
            .field("username", &self.username)
            .field("password", &self.password)
            .finish()
    }
}

/// Window frame in logical points, restored on the next launch.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub type Slots = [Option<CameraId>; SLOT_COUNT];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub open_at_login: bool,
    /// "Smooth playback": show frames at the camera's cadence, absorbing Wi-Fi jitter
    /// at the cost of ~150 ms extra latency.
    #[serde(default)]
    pub smooth_playback: bool,
    /// Mosaic positions (top-left, top-right, bottom-left, bottom-right). Empty string = "No camera".
    #[serde(default, with = "slots_serde")]
    pub slots: Slots,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowGeometry>,
    #[serde(default, rename = "camera")]
    pub cameras: Vec<CameraConfig>,
}

mod slots_serde {
    use super::*;

    pub fn serialize<S: Serializer>(slots: &Slots, s: S) -> Result<S::Ok, S::Error> {
        let raw: Vec<String> = slots
            .iter()
            .map(|slot| slot.map(|id| id.0.to_string()).unwrap_or_default())
            .collect();
        raw.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Slots, D::Error> {
        let raw: Vec<String> = Vec::deserialize(d)?;
        let mut slots: Slots = [None; SLOT_COUNT];
        for (slot, value) in slots.iter_mut().zip(raw) {
            let value = value.trim();
            if !value.is_empty() {
                *slot = Some(CameraId(Uuid::parse_str(value).map_err(serde::de::Error::custom)?));
            }
        }
        Ok(slots)
    }
}

impl Config {
    /// `~/Library/Application Support/camfast/config.toml`.
    pub fn default_path() -> PathBuf {
        app_support_dir().join("config.toml")
    }

    /// Loads the config, returning the default config if the file does not exist yet.
    /// Tightens permissions to 0600 if the file is readable by others.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(io_err(path)(e)),
        };
        let mode = fs::metadata(path).map_err(io_err(path))?.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!(path = %path.display(), mode = format!("{mode:o}"), "config readable by others; restricting to 0600");
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_err(path))?;
        }
        let mut config: Config = toml::from_str(&text)?;
        config.sanitize();
        Ok(config)
    }

    /// Validates and atomically writes the config with mode 0600.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let text = toml::to_string_pretty(self)?;
        if let Some(dir) = path.parent() {
            // Only directories created here get 0700; never chmod an existing parent (with
            // CAMFAST_CONFIG it can be /tmp or $HOME, where chmod fails or is unwanted).
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(io_err(dir))?;
        }
        let tmp = path.with_extension("toml.tmp");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(io_err(&tmp))?;
            // `mode` only applies when the file is created; a leftover tmp file keeps its mode.
            file.set_permissions(fs::Permissions::from_mode(0o600)).map_err(io_err(&tmp))?;
            file.write_all(text.as_bytes()).map_err(io_err(&tmp))?;
            file.sync_all().map_err(io_err(&tmp))?;
        }
        fs::rename(&tmp, path).map_err(io_err(path))?;
        Ok(())
    }

    /// Drops slot references to unknown cameras and duplicate assignments (hand-edited files).
    fn sanitize(&mut self) {
        let known: Vec<CameraId> = self.cameras.iter().map(|c| c.id).collect();
        let mut seen = Vec::new();
        for slot in &mut self.slots {
            if let Some(id) = *slot {
                if !known.contains(&id) || seen.contains(&id) {
                    *slot = None;
                } else {
                    seen.push(id);
                }
            }
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for (i, camera) in self.cameras.iter().enumerate() {
            validate_camera(camera)?;
            if let Some(other) = self.cameras[..i].iter().find(|c| same_name(&c.name, &camera.name)) {
                return Err(ConfigError::Invalid(format!("A camera named \"{}\" already exists", other.name.trim())));
            }
            if self.cameras[..i].iter().any(|c| c.id == camera.id) {
                return Err(ConfigError::Invalid(format!("Duplicate camera id: {}", camera.id)));
            }
        }
        let mut seen = Vec::new();
        for id in self.slots.iter().flatten() {
            if self.camera(*id).is_none() {
                return Err(ConfigError::Invalid(format!("Layout slot points to unknown camera {id}")));
            }
            if seen.contains(id) {
                return Err(ConfigError::Invalid("The same camera is in two layout slots".into()));
            }
            seen.push(*id);
        }
        Ok(())
    }

    pub fn camera(&self, id: CameraId) -> Option<&CameraConfig> {
        self.cameras.iter().find(|c| c.id == id)
    }

    pub fn camera_mut(&mut self, id: CameraId) -> Option<&mut CameraConfig> {
        self.cameras.iter_mut().find(|c| c.id == id)
    }

    /// Cameras currently placed in the mosaic (the only ones that should be streaming).
    pub fn assigned_cameras(&self) -> Vec<&CameraConfig> {
        self.slots.iter().flatten().filter_map(|id| self.camera(*id)).collect()
    }

    pub fn slot_of(&self, id: CameraId) -> Option<usize> {
        self.slots.iter().position(|slot| *slot == Some(id))
    }

    /// Places `camera` in `slot`. A camera occupies at most one slot, so it is removed from any
    /// other slot first.
    pub fn assign(&mut self, slot: usize, camera: Option<CameraId>) {
        if slot >= SLOT_COUNT {
            return;
        }
        if let Some(id) = camera {
            for other in &mut self.slots {
                if *other == Some(id) {
                    *other = None;
                }
            }
        }
        self.slots[slot] = camera;
    }

    /// Inserts or replaces a camera (matched by id) after validating it against the others.
    pub fn upsert_camera(&mut self, camera: CameraConfig) -> Result<(), ConfigError> {
        validate_camera(&camera)?;
        if let Some(other) = self.cameras.iter().find(|c| c.id != camera.id && same_name(&c.name, &camera.name)) {
            return Err(ConfigError::Invalid(format!("A camera named \"{}\" already exists", other.name.trim())));
        }
        match self.camera_mut(camera.id) {
            Some(existing) => *existing = camera,
            None => self.cameras.push(camera),
        }
        Ok(())
    }

    pub fn remove_camera(&mut self, id: CameraId) {
        self.cameras.retain(|c| c.id != id);
        for slot in &mut self.slots {
            if *slot == Some(id) {
                *slot = None;
            }
        }
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

pub fn validate_camera(camera: &CameraConfig) -> Result<(), ConfigError> {
    if camera.name.trim().is_empty() {
        return Err(ConfigError::Invalid("Camera name is required".into()));
    }
    let host = camera.host.trim();
    if host.is_empty() {
        return Err(ConfigError::Invalid(format!("\"{}\": address (IP or hostname) is required", camera.name.trim())));
    }
    if host.contains(['/', ' ', '@']) {
        return Err(ConfigError::Invalid(format!(
            "\"{}\": enter only the IP or hostname, without rtsp:// or a username",
            camera.name.trim()
        )));
    }
    if camera.port == 0 {
        return Err(ConfigError::Invalid(format!("\"{}\": invalid port", camera.name.trim())));
    }
    Ok(())
}

/// `~/Library/Application Support/camfast`.
pub fn app_support_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support"))
        .join("camfast")
}

/// `~/Library/Logs/camfast`.
pub fn log_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join("Library/Logs/camfast")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(name: &str, host: &str) -> CameraConfig {
        let mut c = CameraConfig::new(name, host);
        c.username = "demo".into();
        c.password = Secret::new("s3cret");
        c
    }

    #[test]
    fn roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        let mut config = Config::default();
        let a = camera("Garage", "192.168.1.150");
        let b = camera("Backyard", "192.168.1.151");
        config.assign(0, Some(a.id));
        config.assign(3, Some(b.id));
        config.cameras = vec![a, b];
        config.open_at_login = true;
        config.smooth_playback = true;
        config.window = Some(WindowGeometry { x: 10.0, y: 20.0, width: 1280.0, height: 720.0 });
        config.save(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, config);
        assert!(!fs::read_to_string(&path).unwrap().contains("toml.tmp"));
    }

    #[test]
    fn missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&dir.path().join("nope.toml")).unwrap(), Config::default());
    }

    #[test]
    fn loose_permissions_are_tightened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "open_at_login = false\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        Config::load(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn old_file_without_smooth_playback_loads_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "open_at_login = true\nslots = [\"\", \"\", \"\", \"\"]\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.open_at_login);
        assert!(!config.smooth_playback);
    }

    #[test]
    fn assign_moves_camera_between_slots() {
        let mut config = Config::default();
        let a = camera("A", "10.0.0.1");
        let id = a.id;
        config.cameras.push(a);
        config.assign(0, Some(id));
        config.assign(2, Some(id));
        assert_eq!(config.slots, [None, None, Some(id), None]);
        config.remove_camera(id);
        assert_eq!(config.slots, [None; SLOT_COUNT]);
    }

    #[test]
    fn duplicate_names_rejected() {
        let mut config = Config::default();
        config.upsert_camera(camera("Garage", "10.0.0.1")).unwrap();
        assert!(config.upsert_camera(camera(" garage ", "10.0.0.2")).is_err());
    }

    #[test]
    fn secrets_do_not_leak_into_debug() {
        let c = camera("A", "10.0.0.1");
        assert!(!format!("{c:?}").contains("s3cret"));
        assert!(!c.rtsp_url().contains("demo"));
        assert_eq!(c.rtsp_url(), "rtsp://10.0.0.1:554/cam/realmonitor?channel=1&subtype=0");
    }

    #[test]
    fn hand_edited_bad_slots_are_sanitized() {
        let text = r#"
slots = ["00000000-0000-0000-0000-000000000001", ""]

[[camera]]
id = "00000000-0000-0000-0000-000000000002"
name = "B"
host = "10.0.0.2"
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.slots, [None; SLOT_COUNT]);
        assert_eq!(config.cameras[0].port, 554);
    }
}

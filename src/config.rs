use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};

pub const DEFAULT_PROPERTIES: &str = "server-ip=127.0.0.1\nserver-port=25565\nmotd=Zerqon 26.3\nmax-players=20\nonline-mode=true\nview-distance=4\nnetwork-compression-threshold=256\nlevel-name=world\n";
const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const PROPERTY_KEYS: &[&str] = &[
    "server-ip",
    "server-port",
    "motd",
    "max-players",
    "online-mode",
    "view-distance",
    "network-compression-threshold",
    "level-name",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub network_threads: usize,
    pub world_threads: usize,
    pub max_connections: usize,
    pub max_packet_bytes: usize,
    pub connection_timeout_seconds: u64,
    pub chunk_cache_capacity: usize,
    pub work_queue_capacity: usize,
    pub data_directory: String,
    pub plugin_fuel: u64,
    pub plugin_memory_mib: usize,
    pub java_plugins: bool,
    pub allow_java_clients: bool,
    pub allow_horizon_clients: bool,
    pub java_executable: String,
    pub update_check: bool,
    pub update_check_interval_hours: u64,
    pub max_entities: usize,
    pub spawn_mobs: bool,
    pub auto_download_runtime: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2);
        Self {
            network_threads: cpus.clamp(2, 4),
            world_threads: cpus.saturating_sub(2).clamp(1, 8),
            max_connections: 128,
            max_packet_bytes: 2_097_151,
            connection_timeout_seconds: 30,
            chunk_cache_capacity: 256,
            work_queue_capacity: 128,
            data_directory: ".cache/vanilla/26.3".into(),
            plugin_fuel: 1_000_000,
            plugin_memory_mib: 16,
            java_plugins: true,
            allow_java_clients: true,
            allow_horizon_clients: true,
            java_executable: "java".into(),
            update_check: true,
            update_check_interval_hours: 6,
            max_entities: 128,
            spawn_mobs: true,
            auto_download_runtime: true,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let value: Self = toml::from_str(&read_config(path)?)
            .map_err(|e| ConfigError::Invalid(format!("server.settings: {e}")))?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=168).contains(&self.update_check_interval_hours) {
            return Err(ConfigError::Invalid(
                "update_check_interval_hours must be 1..=168".into(),
            ));
        }
        for (name, value, min, max) in [
            ("network_threads", self.network_threads, 1, 128),
            ("world_threads", self.world_threads, 1, 128),
            ("max_connections", self.max_connections, 1, 65536),
            ("max_packet_bytes", self.max_packet_bytes, 1024, 2_097_151),
            ("chunk_cache_capacity", self.chunk_cache_capacity, 1, 65536),
            ("work_queue_capacity", self.work_queue_capacity, 1, 65536),
            ("plugin_memory_mib", self.plugin_memory_mib, 1, 256),
            ("max_entities", self.max_entities, 1, 10000),
        ] {
            if !(min..=max).contains(&value) {
                return Err(ConfigError::Invalid(format!(
                    "{name} must be {min}..={max}"
                )));
            }
        }
        if !(1..=300).contains(&self.connection_timeout_seconds)
            || !(1000..=100_000_000).contains(&self.plugin_fuel)
        {
            return Err(ConfigError::Invalid(
                "timeout or plugin fuel outside allowed range".into(),
            ));
        }
        for (name, value) in [
            ("data_directory", &self.data_directory),
            ("java_executable", &self.java_executable),
        ] {
            if value.trim().is_empty() || value.len() > 4096 || value.chars().any(char::is_control)
            {
                return Err(ConfigError::Invalid(format!(
                    "{name} must be a nonempty path of at most 4096 bytes without control characters"
                )));
            }
        }
        Ok(())
    }

    pub fn validate_with_properties(&self, properties: &Properties) -> Result<(), ConfigError> {
        self.validate()?;
        if self.max_connections < properties.max_players {
            return Err(ConfigError::Invalid(
                "max_connections must be at least max-players".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct Properties {
    pub address: String,
    pub port: u16,
    pub motd: String,
    pub max_players: usize,
    pub online_mode: bool,
    pub view_distance: u8,
    pub compression_threshold: Option<usize>,
    pub level_name: String,
}

impl Properties {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::parse(&read_config(path)?)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let values = parse_properties(text, "server.properties", PROPERTY_KEYS)?;
        let get = |key, default| *values.get(key).unwrap_or(&default);
        let number = |key, default, min, max| -> Result<usize, ConfigError> {
            let value = get(key, default)
                .parse::<usize>()
                .map_err(|_| ConfigError::Invalid(format!("invalid {key}")))?;
            if !(min..=max).contains(&value) {
                return Err(ConfigError::Invalid(format!("{key} must be {min}..={max}")));
            }
            Ok(value)
        };
        let level_name = get("level-name", "world").to_owned();
        if !valid_directory_name(&level_name) {
            return Err(ConfigError::Invalid(
                "level-name must be a portable directory name, not a Windows device name".into(),
            ));
        }
        let online_mode = match get("online-mode", "true") {
            "true" => true,
            "false" => false,
            _ => {
                return Err(ConfigError::Invalid(
                    "online-mode must be true or false".into(),
                ));
            }
        };
        let compression_threshold = match get("network-compression-threshold", "256") {
            "-1" => None,
            _ => Some(number(
                "network-compression-threshold",
                "256",
                0,
                2_097_151,
            )?),
        };
        let motd = get("motd", "Zerqon 26.3").to_owned();
        if motd.len() > 4096 {
            return Err(ConfigError::Invalid("motd exceeds 4096 bytes".into()));
        }
        let address = get("server-ip", "127.0.0.1").to_owned();
        if address.len() > 255 || address.chars().any(char::is_control) {
            return Err(ConfigError::Invalid(
                "server-ip exceeds 255 bytes or contains control characters".into(),
            ));
        }
        Ok(Self {
            address,
            port: number("server-port", "25565", 0, 65535)? as u16,
            motd,
            max_players: number("max-players", "20", 1, 65536)?,
            online_mode,
            view_distance: number("view-distance", "4", 1, 32)? as u8,
            compression_threshold,
            level_name,
        })
    }
}

pub fn accepted_eula(path: &Path) -> Result<bool, ConfigError> {
    let text = match read_config(path) {
        Ok(text) => text,
        Err(ConfigError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let values = parse_properties(&text, "eula.txt", &["eula"])?;
    match values.get("eula").copied() {
        Some("true") => Ok(true),
        Some("false") | None => Ok(false),
        _ => Err(ConfigError::Invalid("eula must be true or false".into())),
    }
}

fn read_config(path: &Path) -> Result<String, ConfigError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(ConfigError::Invalid(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(ConfigError::Invalid(format!(
            "{} exceeds the 1 MiB configuration limit",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::Invalid(format!(
            "{} exceeds the 1 MiB configuration limit",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| ConfigError::Invalid(format!("{} must be UTF-8", path.display())))
}

fn parse_properties<'a>(
    text: &'a str,
    source: &str,
    allowed: &[&str],
) -> Result<HashMap<&'a str, &'a str>, ConfigError> {
    if text.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::Invalid(format!(
            "{source} exceeds the 1 MiB configuration limit"
        )));
    }
    let mut values = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            ConfigError::Invalid(format!("{source} line {} needs key=value", index + 1))
        })?;
        let key = key.trim();
        if !allowed.contains(&key) {
            return Err(ConfigError::Invalid(format!(
                "{source} line {} has an unsupported property",
                index + 1
            )));
        }
        if values.insert(key, value.trim()).is_some() {
            return Err(ConfigError::Invalid(format!(
                "duplicate property in {source}: {key}"
            )));
        }
    }
    Ok(values)
}

fn valid_directory_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || name.ends_with(['.', ' '])
        || name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
        || name.chars().any(char::is_control)
    {
        return false;
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    if let Some(suffix) = base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
        && matches!(
            suffix,
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
    {
        return false;
    }
    true
}

pub fn initialize(directory: &Path) -> Result<(), ConfigError> {
    fs::create_dir_all(directory)?;
    for child in [
        "plugins/rust-plugins",
        "plugins/java-plugins",
        "config",
        "logs",
        ".cache",
        "libraries",
    ] {
        fs::create_dir_all(directory.join(child))?;
    }
    create_new(&directory.join("server.properties"), DEFAULT_PROPERTIES)?;
    create_new(
        &directory.join("eula.txt"),
        "# Read https://aka.ms/MinecraftEULA before accepting.\neula=false\n",
    )?;
    create_new(
        &directory.join("server.settings"),
        &toml::to_string_pretty(&Settings::default())
            .map_err(|e| ConfigError::Invalid(e.to_string()))?,
    )?;
    Ok(())
}

fn create_new(path: &Path, content: &str) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} must be a regular file", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match OpenOptions::new().create_new(true).write(true).open(path) {
        Ok(mut file) => {
            file.write_all(content.as_bytes())?;
            file.sync_all()
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::metadata(path)?.is_file() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} must be a regular file", path.display()),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_profile_flags_roundtrip_independently_including_closed_admission() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.settings");
        for (java, horizon) in [(true, true), (true, false), (false, true), (false, false)] {
            fs::write(
                &path,
                format!("allow_java_clients={java}\nallow_horizon_clients={horizon}\n"),
            )
            .unwrap();
            let settings = Settings::load(&path).unwrap();
            settings
                .validate_with_properties(&Properties::parse("").unwrap())
                .unwrap();
            let saved: toml::Value = toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
            assert_eq!(saved["allow_java_clients"].as_bool(), Some(java));
            assert_eq!(saved["allow_horizon_clients"].as_bool(), Some(horizon));
        }
    }

    #[test]
    fn old_settings_keep_both_client_profiles_enabled() {
        let settings: Settings = toml::from_str("network_threads=2\n").unwrap();
        let saved: toml::Value = toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
        assert_eq!(
            saved
                .get("allow_java_clients")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            saved
                .get("allow_horizon_clients")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn update_checker_can_be_disabled_and_interval_is_bounded() {
        let settings: Settings =
            toml::from_str("update_check=false\nupdate_check_interval_hours=1\n").unwrap();
        assert!(settings.validate().is_ok());
        for interval in [0, 169] {
            let settings: Settings =
                toml::from_str(&format!("update_check_interval_hours={interval}\n")).unwrap();
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn parses_properties_without_losing_motd_equals() {
        let config = Properties::parse("server-ip=127.0.0.1\nserver-port=25566\nmotd=Zerqon = Привет\nonline-mode=false\nview-distance=4\n").unwrap();
        assert_eq!(config.port, 25566);
        assert_eq!(config.motd, "Zerqon = Привет");
        assert!(!config.online_mode);
    }

    #[test]
    fn rejects_invalid_and_unsafe_properties() {
        for value in [
            "server-port=65536",
            "max-players=0",
            "online-mode=fales",
            "view-distance=0",
            "view-distance=65",
            "level-name=../lost",
            "level-name=C:\\outside",
            "network-compression-threshold=-2",
        ] {
            assert!(Properties::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn initialization_preserves_user_files_and_creates_plugin_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.properties"), "motd=existing").unwrap();
        initialize(dir.path()).unwrap();
        assert!(dir.path().join("plugins/rust-plugins").is_dir());
        assert!(dir.path().join("plugins/java-plugins").is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("server.properties")).unwrap(),
            "motd=existing"
        );
        assert!(
            std::fs::read_to_string(dir.path().join("eula.txt"))
                .unwrap()
                .contains("eula=false")
        );
    }

    #[test]
    fn unsupported_and_duplicate_properties_are_not_silently_ignored() {
        for text in [
            "online_mode=false",
            "server-port=1\nserver-port=2",
            "=value",
            "online-mode=true\nunknown=value",
        ] {
            assert!(Properties::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn world_names_are_portable_and_cannot_reference_windows_devices() {
        for name in [
            "CON",
            "nul.txt",
            "COM1",
            "lpt9.backup",
            "AUX",
            "PRN",
            "CONIN$",
            "CONOUT$",
            "COM¹",
            "LPT²",
            "world.",
            "bad?name",
            "name|pipe",
            "a<b",
            "quote\"",
            "bad\0name",
            "bad\tname",
            "../world",
            "a/b",
        ] {
            assert!(
                Properties::parse(&format!("level-name={name}")).is_err(),
                "accepted {name:?}"
            );
        }
        for name in ["world", "мир", "dimension-1", "COM10", "my.world"] {
            assert!(
                Properties::parse(&format!("level-name={name}")).is_ok(),
                "rejected {name:?}"
            );
        }
    }

    #[test]
    fn eula_acceptance_requires_one_unambiguous_boolean_key() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("eula.txt");
        for text in [
            "eula=true\neula=false",
            "eula=false\neula=true",
            "eula=true\neula=true",
            "eula=TRUE",
            "eula=yes",
            "eula=true # accepted",
            "eula=true\nunknown=true",
            "eula true",
        ] {
            fs::write(&path, text).unwrap();
            assert!(accepted_eula(&path).is_err(), "accepted {text:?}");
        }
        for (text, expected) in [
            ("# eula=true\n", false),
            ("eula=false", false),
            ("# agreement\n eula = true \n", true),
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(accepted_eula(&path).unwrap(), expected, "{text}");
        }
        assert!(!accepted_eula(&directory.path().join("missing.txt")).unwrap());
    }

    #[test]
    fn all_configuration_files_have_a_bounded_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.txt");
        fs::write(&path, format!("#{}", "x".repeat(1024 * 1024))).unwrap();
        assert!(Settings::load(&path).is_err());
        assert!(Properties::load(&path).is_err());
        assert!(accepted_eula(&path).is_err());
        assert!(Properties::parse(&format!("#{}", "x".repeat(1024 * 1024))).is_err());
    }

    #[test]
    fn existing_directories_cannot_masquerade_as_configuration_files() {
        for name in ["server.properties", "server.settings", "eula.txt"] {
            let directory = tempfile::tempdir().unwrap();
            fs::create_dir(directory.path().join(name)).unwrap();
            assert!(initialize(directory.path()).is_err(), "{name}");
            assert!(directory.path().join(name).is_dir());
        }
    }

    #[test]
    fn initialization_preserves_settings_and_existing_runtime_directories() {
        let directory = tempfile::tempdir().unwrap();
        let settings = "java_plugins=false\njava_executable='custom-java'\n";
        fs::write(directory.path().join("server.settings"), settings).unwrap();
        for name in [".cache", "libraries"] {
            fs::create_dir(directory.path().join(name)).unwrap();
            fs::write(directory.path().join(name).join("keep.txt"), "preserved").unwrap();
        }
        initialize(directory.path()).unwrap();
        initialize(directory.path()).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("server.settings")).unwrap(),
            settings
        );
        for name in [".cache", "libraries"] {
            assert_eq!(
                fs::read_to_string(directory.path().join(name).join("keep.txt")).unwrap(),
                "preserved"
            );
        }
    }

    #[test]
    fn connection_capacity_cannot_be_lower_than_advertised_player_capacity() {
        let properties = Properties::parse("max-players=200").unwrap();
        assert!(
            Settings::default()
                .validate_with_properties(&properties)
                .is_err()
        );
        let settings = Settings {
            max_connections: 200,
            ..Settings::default()
        };
        assert!(settings.validate_with_properties(&properties).is_ok());
    }

    #[test]
    fn settings_reject_unknown_keys_and_empty_required_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.settings");
        for text in [
            "netwrok_threads=2",
            "java_executable=''",
            "data_directory=''",
            "data_directory='x\u{0000}y'",
        ] {
            fs::write(&path, text).unwrap();
            assert!(Settings::load(&path).is_err(), "{text}");
        }
    }

    #[test]
    fn invalid_utf8_and_directory_inputs_report_the_configuration_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken.properties");
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        for error in [
            Properties::load(&path).unwrap_err(),
            Settings::load(&path).unwrap_err(),
            accepted_eula(&path).unwrap_err(),
        ] {
            assert!(error.to_string().contains("broken.properties"));
        }
        let error = Properties::load(directory.path()).unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[test]
    fn files_in_place_of_runtime_directories_are_preserved_and_rejected() {
        for name in [".cache", "libraries"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(name);
            fs::write(&path, "user data").unwrap();
            assert!(initialize(directory.path()).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "user data");
        }
    }
}

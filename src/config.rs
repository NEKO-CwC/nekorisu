//! ~/.nekorisu/config.toml. It never holds passwords: those go into the
//! system keychain through `nekorisu storage login`. `version`, `source` and
//! `signature` are reserved for policies pushed by a trusted source.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::paths::Paths;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub source: Option<String>,
    #[allow(dead_code)]
    pub signature: Option<String>,
    pub storage: BTreeMap<String, StorageConfig>,
    pub update: UpdateConfig,
    pub paths: PathsConfig,
    pub local: LocalConfig,
    /// Keys set on this device that the trusted source's values do not override
    /// (`nekorisu config set <key> <value> --lock`), e.g. `local.copy_form`. Path maps are
    /// locked one by one instead (`lock = true`).
    pub locked: Vec<String>,
}

/// `[paths]`: where sessions from other devices go on restore.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// `[[paths.map]]`, tried before the device homes: a working directory that starts with
    /// `from` is taken to be `to` plus the rest on this device.
    pub map: Vec<PathMap>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct PathMap {
    /// A path as another device wrote it, e.g. `D:\work` or `/home/me/src`.
    pub from: String,
    /// Where that is on this device.
    pub to: String,
    /// A rule from the trusted source with the same `from` does not apply while this is set.
    #[serde(default)]
    pub lock: bool,
}

/// `[local]`: what this device keeps of sessions it reads from the storage.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    /// "pruned" (the default): only what the person said and the agent's replies are fetched
    /// ahead, other lines when asked for. "full": whole segments. Restore always takes the
    /// full original.
    pub copy_form: String,
}

impl LocalConfig {
    pub fn full_copies(&self) -> bool {
        self.copy_form == "full"
    }
}

/// `[update]`.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Download and switch versions on its own. Off means check and tell only.
    pub auto: bool,
    pub channel: String,
    /// Tried before the built-in GitHub address; mirrors are safe because every file is signature-checked.
    pub urls: Vec<String>,
    /// The link the install script made; defaults to ~/.local/bin/nekorisu (nekorisu.exe on Windows).
    pub link: Option<std::path::PathBuf>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            auto: true,
            channel: "stable".into(),
            urls: Vec::new(),
            link: None,
        }
    }
}

/// `[storage.<name>]`. The password is not here: see `secrets`.
#[derive(Clone, Debug, Deserialize)]
pub struct StorageConfig {
    /// "webdav".
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    /// Like `curl --resolve`: connect to this IP (port from the URL) but keep the
    /// URL's host for TLS, certificate checks and the Host header. A pinned
    /// address also means a direct connection, never through a proxy.
    #[serde(default)]
    pub resolve: Option<String>,
}

/// Storage names end up as keychain account names and file names.
pub fn check_storage_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        bail!("存储的名字只能用字母、数字、点、下划线和短横线，并且以字母或数字开头：{name}");
    }
    Ok(())
}

/// config.toml as plain TOML, or an empty table when there is none.
pub fn load_table(paths: &Paths) -> anyhow::Result<toml::Table> {
    let file = paths.config();
    match fs::read_to_string(&file) {
        Ok(text) => text
            .parse::<toml::Table>()
            .with_context(|| format!("{} 不是合法的 TOML，没有改动它", file.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(err) => Err(err.into()),
    }
}

/// Changes config.toml through `change`, keeping everything else (comments are lost: the file
/// is parsed and written back). The old file is copied to `config.toml.bak` first. The result
/// must still read as a config, or nothing is written.
pub fn edit<T>(paths: &Paths, change: impl FnOnce(&mut toml::Table) -> anyhow::Result<T>) -> anyhow::Result<T> {
    let file = paths.config();
    let mut root = load_table(paths)?;
    let out = change(&mut root)?;
    let text = toml::to_string_pretty(&root)?;
    toml::from_str::<Config>(&text).context("改完的 config.toml 读不了，没有写")?;
    fs::create_dir_all(&paths.home)?;
    if file.exists() {
        fs::copy(&file, file.with_extension("toml.bak")).context("备份不了 config.toml")?;
    }
    let tmp = file.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &file)?;
    Ok(out)
}

/// Writes `[storage.<name>]` into config.toml, keeping everything else (`edit`).
/// Returns true if an entry with that name was replaced.
pub fn save_storage(paths: &Paths, name: &str, storage: &StorageConfig) -> anyhow::Result<bool> {
    check_storage_name(name)?;
    let mut entry = toml::Table::new();
    entry.insert("kind".into(), storage.kind.clone().into());
    entry.insert("url".into(), storage.url.clone().into());
    if let Some(username) = &storage.username {
        entry.insert("username".into(), username.clone().into());
    }
    if let Some(resolve) = &storage.resolve {
        entry.insert("resolve".into(), resolve.clone().into());
    }
    edit(paths, |root| {
        let storages = root
            .entry("storage")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .context("config.toml 里的 storage 应该是一张表")?;
        Ok(storages.insert(name.to_string(), toml::Value::Table(entry)).is_some())
    })
}

pub fn load(paths: &Paths) -> anyhow::Result<Config> {
    match fs::read_to_string(paths.config()) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_path_maps_and_copy_form() {
        let config: Config = toml::from_str(
            r#"
            [local]
            copy_form = "full"
            [[paths.map]]
            from = 'D:\work'
            to = "/Users/me/work"
            "#,
        )
        .unwrap();
        assert!(config.local.full_copies());
        assert_eq!(
            (config.paths.map[0].from.as_str(), config.paths.map[0].to.as_str()),
            (r"D:\work", "/Users/me/work")
        );
        assert!(!config.paths.map[0].lock, "not locked unless it says so");
        assert!(!Config::default().local.full_copies());
    }

    #[test]
    fn parses_storage_without_password() {
        let config: Config = toml::from_str(
            r#"
            version = 1
            [storage.home-nas]
            kind = "webdav"
            url = "http://127.0.0.1:5244/dav"
            username = "me"
            "#,
        )
        .unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.storage["home-nas"].kind, "webdav");
        assert_eq!(config.storage["home-nas"].resolve, None);
        assert!(config.update.auto, "auto-update is on unless turned off");
        assert_eq!(config.update.channel, "stable");
    }

    #[test]
    fn parses_update_section() {
        let config: Config = toml::from_str(
            r#"
            [update]
            auto = false
            urls = ["https://mirror.example/nekorisu/stable.json"]
            "#,
        )
        .unwrap();
        assert!(!config.update.auto);
        assert_eq!(config.update.channel, "stable");
        assert_eq!(config.update.urls.len(), 1);
    }

    fn webdav(url: &str, username: Option<&str>, resolve: Option<&str>) -> StorageConfig {
        StorageConfig {
            kind: "webdav".into(),
            url: url.into(),
            username: username.map(str::to_owned),
            resolve: resolve.map(str::to_owned),
        }
    }

    #[test]
    fn save_storage_creates_then_keeps_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: dir.path().join("home"),
        };
        let replaced = save_storage(&paths, "nas", &webdav("https://dav.example/dav", Some("me"), None)).unwrap();
        assert!(!replaced);
        assert!(
            !paths.config().with_extension("toml.bak").exists(),
            "nothing to back up yet"
        );

        // Another section and a second storage must survive the rewrite.
        let mut text = fs::read_to_string(paths.config()).unwrap();
        text.push_str("\n[update]\nauto = false\n");
        fs::write(paths.config(), text).unwrap();
        let replaced = save_storage(
            &paths,
            "cloud",
            &webdav("https://other.example/", None, Some("203.0.113.7")),
        )
        .unwrap();
        assert!(!replaced);
        assert!(paths.config().with_extension("toml.bak").exists());

        let config = load(&paths).unwrap();
        assert_eq!(config.storage.len(), 2);
        assert_eq!(config.storage["nas"].username.as_deref(), Some("me"));
        assert_eq!(config.storage["cloud"].resolve.as_deref(), Some("203.0.113.7"));
        assert!(!config.update.auto, "[update] kept");

        // Same name again replaces the entry as a whole.
        let replaced = save_storage(&paths, "nas", &webdav("https://moved.example/dav", Some("me"), None)).unwrap();
        assert!(replaced);
        assert_eq!(load(&paths).unwrap().storage["nas"].url, "https://moved.example/dav");
    }

    #[test]
    fn save_storage_refuses_a_broken_file_and_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: dir.path().to_path_buf(),
        };
        fs::write(paths.config(), "this = is not [toml").unwrap();
        assert!(save_storage(&paths, "nas", &webdav("https://x.example", None, None)).is_err());
        assert_eq!(fs::read_to_string(paths.config()).unwrap(), "this = is not [toml");
        for bad in ["", ".hidden", "a/b", "a b", "../x", "名字"] {
            assert!(check_storage_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_storage_name("home-nas_1.local").is_ok());
    }
}

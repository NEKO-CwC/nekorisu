//! Where things live. `NEKORISU_HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME`
//! override the defaults (handy for tests and for agents' own overrides).

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Paths {
    /// ~/.nekorisu
    pub home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = std::env::var_os("NEKORISU_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".nekorisu"));
        Self { home }
    }

    pub fn run_dir(&self) -> PathBuf {
        self.home.join("run")
    }

    pub fn socket(&self) -> PathBuf {
        self.run_dir().join("core.sock")
    }

    pub fn lock(&self) -> PathBuf {
        self.run_dir().join("core.lock")
    }

    pub fn core_log(&self) -> PathBuf {
        self.run_dir().join("core.log")
    }

    pub fn device_file(&self) -> PathBuf {
        self.home.join("device.json")
    }

    pub fn config(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.home.join("cache")
    }

    /// Storage passwords on systems without a keychain, one file each.
    pub fn secrets_dir(&self) -> PathBuf {
        self.home.join("secrets")
    }

    /// Installed versions, one directory each.
    pub fn versions_dir(&self) -> PathBuf {
        self.home.join("versions")
    }

    /// Copies of local state taken before switching versions, so a rollback can restore them.
    pub fn backup_dir(&self) -> PathBuf {
        self.home.join("backup")
    }

    /// What the updater remembers: last check, highest certificate serial, bad versions.
    pub fn update_state(&self) -> PathBuf {
        self.home.join("update.json")
    }

    /// Written just before switching versions; cleared once the new core is up.
    pub fn upgrade_marker(&self) -> PathBuf {
        self.run_dir().join("upgrade.json")
    }

    pub fn update_lock(&self) -> PathBuf {
        self.run_dir().join("update.lock")
    }
}

/// Where the install script puts the `nekorisu` symlink.
pub fn default_link() -> PathBuf {
    home_dir().join(".local").join("bin").join("nekorisu")
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn claude_home() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"))
}

pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"))
}

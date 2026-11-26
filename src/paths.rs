//! Where things live. `NEKORISU_HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME`
//! override the defaults (handy for tests and for agents' own overrides).

use std::ffi::OsString;
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

    /// Where core listens on Unix. Windows uses a named pipe instead (`ipc`).
    #[cfg(unix)]
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

    /// Windows only: which versioned binary `~\.local\bin\nekorisu.exe` is a copy of
    /// (its absolute path). Unix reads that off the symlink instead.
    #[cfg_attr(unix, allow(dead_code))]
    pub fn current_pointer(&self) -> PathBuf {
        self.home.join("current")
    }
}

/// The binary's file name: `nekorisu`, or `nekorisu.exe` on Windows.
pub const BINARY_NAME: &str = if cfg!(windows) { "nekorisu.exe" } else { "nekorisu" };

/// The Windows login launcher shipped next to the binary (`src/bin/nekorisu-start.rs`).
#[cfg_attr(unix, allow(dead_code))]
pub const LAUNCHER_NAME: &str = "nekorisu-start.exe";

/// Where the install script puts the `nekorisu` link (a symlink; a hard link or copy on Windows).
pub fn default_link() -> PathBuf {
    home_dir().join(".local").join("bin").join(BINARY_NAME)
}

/// The user's home: `HOME` on Unix; on Windows the profile folder (`USERPROFILE`), where
/// Claude Code and Codex keep their files, then `HOME` (Git Bash sets it too).
pub fn home_dir() -> PathBuf {
    pick_home(cfg!(windows), std::env::var_os("USERPROFILE"), std::env::var_os("HOME"))
}

fn pick_home(windows: bool, profile: Option<OsString>, home: Option<OsString>) -> PathBuf {
    let profile = profile.filter(|_| windows);
    profile
        .into_iter()
        .chain(home)
        .find(|v| !v.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_prefers_the_profile_folder_and_unix_ignores_it() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            pick_home(true, some(r"C:\Users\me"), some("/c/Users/me")),
            PathBuf::from(r"C:\Users\me")
        );
        assert_eq!(pick_home(true, None, some("/c/Users/me")), PathBuf::from("/c/Users/me"));
        assert_eq!(
            pick_home(true, some(""), some("/c/Users/me")),
            PathBuf::from("/c/Users/me")
        );
        assert_eq!(
            pick_home(false, some(r"C:\Users\me"), some("/home/me")),
            PathBuf::from("/home/me")
        );
        assert_eq!(pick_home(false, some(r"C:\Users\me"), None), PathBuf::from("/"));
    }
}

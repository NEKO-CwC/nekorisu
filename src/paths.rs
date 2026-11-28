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

    /// What was last uploaded or restored, per session (`sync`).
    pub fn sync_dir(&self) -> PathBuf {
        self.home.join("sync")
    }

    /// Copies of manifests read from the storage; they never change once written.
    pub fn manifests_dir(&self) -> PathBuf {
        self.home.join("manifests")
    }

    /// Where restore moves a local file it replaces, instead of deleting it.
    pub fn replaced_dir(&self) -> PathBuf {
        self.home.join("replaced")
    }

    /// Restore assembles files here before moving them into the agent's directory.
    pub fn staging_dir(&self) -> PathBuf {
        self.home.join("staging")
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
/// Debug builds only: `NEKORISU_E2E_NAME` (letters, digits, `-`) gives login start and the
/// keychain items names of their own, so end-to-end tests use the real launchd, Task Scheduler
/// and keychain without touching the developer's own. Release builds never read it.
pub fn test_name() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let name = std::env::var("NEKORISU_E2E_NAME").ok()?;
    let ok = !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then_some(name)
}

pub fn home_dir() -> PathBuf {
    // Debug builds only: end-to-end tests on macOS keep the real HOME, which the keychain and
    // Claude Code's sign-ins go by, and give nekorisu a home of its own with this.
    if cfg!(debug_assertions)
        && let Some(home) = std::env::var_os("NEKORISU_E2E_HOME").filter(|h| !h.is_empty())
    {
        return PathBuf::from(home);
    }
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

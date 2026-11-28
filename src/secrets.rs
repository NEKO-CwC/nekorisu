//! Storage passwords and trusted sources' tokens. On macOS they live in the login keychain,
//! written and read through the `security` tool; elsewhere in `~/.nekorisu/secrets/<name>`
//! (tokens in `secrets/source/<name>`, mode 0600). Never in config.toml, never logged, never
//! printed.
//!
//! Windows uses the same file without Unix modes: it sits in the user's profile folder,
//! which only the user, SYSTEM and Administrators can read. Credential Manager and DPAPI
//! are not used because they fail in SSH sessions logged in with a key, where core may run.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, bail};

use crate::config::check_storage_name;
use crate::paths::Paths;

/// What a secret opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A storage's password; the name is the storage's.
    Storage,
    /// A trusted source's token; the name is the source's.
    Source,
}

impl Kind {
    /// Keychain service name; the account is the name.
    fn service(self) -> &'static str {
        match self {
            Kind::Storage => "nekorisu-storage",
            Kind::Source => "nekorisu-source",
        }
    }

    fn dir(self, paths: &Paths) -> PathBuf {
        match self {
            Kind::Storage => paths.secrets_dir(),
            Kind::Source => paths.secrets_dir().join("source"),
        }
    }
}

/// `security` exits with this when the item is not in the keychain.
const KEYCHAIN_NOT_FOUND: i32 = 44;

/// The macOS keychain, unless a debug build is told `NEKORISU_SECRETS=file` (tests, so they
/// leave nothing in the developer's keychain). Release builds never read the variable.
fn keychain() -> bool {
    cfg!(target_os = "macos") && !(cfg!(debug_assertions) && std::env::var("NEKORISU_SECRETS").as_deref() == Ok("file"))
}

pub fn store(paths: &Paths, name: &str, password: &str) -> anyhow::Result<()> {
    store_kind(Kind::Storage, paths, name, password)
}

/// `None` when no password was ever stored for `name`.
pub fn load(paths: &Paths, name: &str) -> anyhow::Result<Option<String>> {
    load_kind(Kind::Storage, paths, name)
}

pub fn store_kind(kind: Kind, paths: &Paths, name: &str, secret: &str) -> anyhow::Result<()> {
    check_storage_name(name)?;
    check_password(secret)?;
    if keychain() {
        keychain_store(kind.service(), name, secret)
    } else {
        file_store(paths, &kind.dir(paths), name, secret)
    }
}

/// `None` when nothing was ever stored for `name`.
pub fn load_kind(kind: Kind, paths: &Paths, name: &str) -> anyhow::Result<Option<String>> {
    check_storage_name(name)?;
    if keychain() {
        keychain_load(kind.service(), name)
    } else {
        file_load(&kind.dir(paths), name)
    }
}

/// Forgets what was stored for `name`; nothing stored is fine.
pub fn delete_kind(kind: Kind, paths: &Paths, name: &str) -> anyhow::Result<()> {
    check_storage_name(name)?;
    if keychain() {
        keychain_delete(kind.service(), name)
    } else {
        file_delete(&kind.dir(paths), name)
    }
}

/// Where `store` puts the password, for the message after `storage login`.
pub fn place() -> &'static str {
    if keychain() {
        "系统钥匙串"
    } else if cfg!(windows) {
        // The profile folder's own permissions: the user, SYSTEM and Administrators.
        "~\\.nekorisu\\secrets\\（在你的用户文件夹里，只有你本人、系统和管理员能读）"
    } else {
        "~/.nekorisu/secrets/（只有你本人能读）"
    }
}

fn check_password(password: &str) -> anyhow::Result<()> {
    if password.is_empty() {
        bail!("密码是空的");
    }
    // A line break would start another `security -i` command.
    if password.contains(['\n', '\r', '\0']) {
        bail!("密码里不能有换行");
    }
    Ok(())
}

// ---- macOS keychain, through `security` ----

/// Marks a value that is the hex form of the password. `security ... -w` prints a password as
/// hex (observed: whenever it holds non-ASCII bytes) and nothing says which of the two it did,
/// so a password that would come back as hex is stored as `hex:<hex>` and decoded when read.
const HEX_MARK: &str = "hex:";

/// One `security -i` command. The password rides on stdin, so it never shows up in
/// the process list. Inside double quotes only `\` and `"` need an escape.
fn keychain_script(service: &str, name: &str, value: &str) -> String {
    let quoted = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("add-generic-password -U -s {service} -a {name} -w \"{quoted}\"\n")
}

/// What `keychain_load` makes of the text `security -w` printed.
fn from_keychain(printed: &str) -> String {
    printed
        .strip_prefix(HEX_MARK)
        .and_then(|digits| hex::decode(digits).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| printed.to_string())
}

fn keychain_store(service: &str, name: &str, password: &str) -> anyhow::Result<()> {
    // Plain first (readable in Keychain Access), the hex form when plain does not come back intact.
    let hex_form = format!("{HEX_MARK}{}", hex::encode(password));
    let plain_may_work = password.is_ascii() && !password.starts_with(HEX_MARK);
    let forms = plain_may_work
        .then_some(password)
        .into_iter()
        .chain([hex_form.as_str()]);
    let mut last_error = String::new();
    for value in forms {
        let mut child = Command::new("security")
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("找不到 security 命令，没法用钥匙串")?;
        child
            .stdin
            .take()
            .context("security 没有接住输入")?
            .write_all(keychain_script(service, name, value).as_bytes())?;
        let output = child.wait_with_output()?;
        // In interactive mode `security` exits 0 even when the command failed, so look it up again.
        if keychain_load(service, name)?.as_deref() == Some(password) {
            return Ok(());
        }
        let first = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        last_error = first.replace(password, "***").replace(value, "***");
    }
    bail!("没能把密码存进钥匙串（存进去的和输入的对不上）{}", suffix(&last_error))
}

fn keychain_load(service: &str, name: &str) -> anyhow::Result<Option<String>> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", name, "-w"])
        .stdin(Stdio::null())
        .output()
        .context("找不到 security 命令，没法用钥匙串")?;
    if output.status.success() {
        let text = String::from_utf8(output.stdout).context("钥匙串里的密码不是 UTF-8 文本")?;
        return Ok(Some(from_keychain(text.strip_suffix('\n').unwrap_or(&text))));
    }
    if output.status.code() == Some(KEYCHAIN_NOT_FOUND) {
        return Ok(None);
    }
    bail!("读不了钥匙串（security 退出码 {}）", output.status.code().unwrap_or(-1))
}

fn keychain_delete(service: &str, name: &str) -> anyhow::Result<()> {
    // On success `security` prints the item's attributes; they stay in `output`.
    let output = Command::new("security")
        .args(["delete-generic-password", "-s", service, "-a", name])
        .stdin(Stdio::null())
        .output()
        .context("找不到 security 命令，没法用钥匙串")?;
    if output.status.success() || output.status.code() == Some(KEYCHAIN_NOT_FOUND) {
        return Ok(());
    }
    bail!(
        "没能从钥匙串删掉（security 退出码 {}）",
        output.status.code().unwrap_or(-1)
    )
}

fn suffix(line: &str) -> String {
    if line.is_empty() {
        String::new()
    } else {
        format!("：{line}")
    }
}

// ---- other systems: a private file ----

fn file_store(paths: &Paths, dir: &std::path::Path, name: &str, password: &str) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(paths.secrets_dir(), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = paths;
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = fs::remove_file(&tmp);
    options.open(&tmp)?.write_all(password.as_bytes())?;
    fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

fn file_load(dir: &std::path::Path, name: &str) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(dir.join(name)) {
        // The file is written without a line break; one left by a hand edit is not part of the password.
        Ok(text) => {
            let text = text.strip_suffix('\n').unwrap_or(&text);
            Ok(Some(text.strip_suffix('\r').unwrap_or(text).to_string()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn file_delete(dir: &std::path::Path, name: &str) -> anyhow::Result<()> {
    match fs::remove_file(dir.join(name)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_script_keeps_the_password_out_of_argv_and_escapes_it() {
        let script = keychain_script("nekorisu-storage", "nas", r#"p"a\ss w#rd"#);
        assert_eq!(
            script,
            "add-generic-password -U -s nekorisu-storage -a nas -w \"p\\\"a\\\\ss w#rd\"\n"
        );
        assert_eq!(script.matches('\n').count(), 1, "exactly one command");
    }

    #[test]
    fn keychain_values_decode_only_their_own_hex_form() {
        let hex_form = format!("hex:{}", hex::encode("pä ss 密码"));
        assert_eq!(from_keychain(&hex_form), "pä ss 密码");
        assert_eq!(from_keychain("plain pass"), "plain pass");
        // Not a hex form after all: left as it is.
        assert_eq!(from_keychain("hex:zz"), "hex:zz");
        assert_eq!(from_keychain("hex:abc"), "hex:abc");
        assert_eq!(from_keychain("hex:ff"), "hex:ff", "not UTF-8");
        let script = keychain_script("nekorisu-source", "nas", &hex_form);
        assert!(script.contains("-w \"hex:") && script.matches('\n').count() == 1);
    }

    #[test]
    fn rejects_passwords_that_could_inject_commands() {
        assert!(check_password("a\nb").is_err());
        assert!(check_password("a\rb").is_err());
        assert!(check_password("").is_err());
        assert!(check_password("p ass\"word").is_ok());
    }

    #[test]
    fn file_backend_round_trips_with_private_modes() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: dir.path().to_path_buf(),
        };
        let dir_of = |kind: Kind| kind.dir(&paths);
        assert_eq!(file_load(&dir_of(Kind::Storage), "nas").unwrap(), None);
        file_store(&paths, &dir_of(Kind::Storage), "nas", "synthetic pass").unwrap();
        file_store(&paths, &dir_of(Kind::Storage), "nas", "synthetic pass 2").unwrap(); // update in place
        assert_eq!(
            file_load(&dir_of(Kind::Storage), "nas").unwrap().as_deref(),
            Some("synthetic pass 2")
        );
        // A source's token of the same name is another secret.
        assert_eq!(file_load(&dir_of(Kind::Source), "nas").unwrap(), None);
        file_store(&paths, &dir_of(Kind::Source), "nas", "synthetic token").unwrap();
        assert_eq!(
            file_load(&dir_of(Kind::Source), "nas").unwrap().as_deref(),
            Some("synthetic token")
        );
        assert_eq!(
            file_load(&dir_of(Kind::Storage), "nas").unwrap().as_deref(),
            Some("synthetic pass 2")
        );
        #[cfg(unix)]
        {
            let mode = |p: std::path::PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(paths.secrets_dir().join("nas")), 0o600);
            assert_eq!(mode(paths.secrets_dir()), 0o700);
            assert_eq!(mode(paths.secrets_dir().join("source")), 0o700);
            assert_eq!(mode(paths.secrets_dir().join("source").join("nas")), 0o600);
        }
        assert!(
            fs::read_dir(paths.secrets_dir()).unwrap().count() == 2,
            "the password, the source folder, no temp file left"
        );
    }

    #[test]
    fn file_backend_forgets_one_secret() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: dir.path().to_path_buf(),
        };
        let (storage, source) = (Kind::Storage.dir(&paths), Kind::Source.dir(&paths));
        file_store(&paths, &storage, "home", "synthetic pass").unwrap();
        file_store(&paths, &source, "home", "synthetic token").unwrap();
        file_delete(&source, "home").unwrap();
        assert_eq!(file_load(&source, "home").unwrap(), None);
        assert_eq!(
            file_load(&storage, "home").unwrap().as_deref(),
            Some("synthetic pass"),
            "a storage of the same name keeps its password"
        );
        file_delete(&source, "home").unwrap();
    }
}

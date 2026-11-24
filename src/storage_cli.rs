//! `nekorisu storage add | login | test`: the storage's address goes into
//! config.toml, its password into the keychain, and neither passes through an Agent.
//! These run in the CLI process itself; only the reload after `add` goes to core.

use std::io::{self, BufRead, Write};
use std::mem::MaybeUninit;
use std::net::IpAddr;

use anyhow::{Context, bail};
use clap::Subcommand;

use crate::client;
use crate::config::{self, StorageConfig};
use crate::paths::Paths;
use crate::proto::ReloadConfigRequest;
use crate::secrets;
use crate::storage;

#[derive(Subcommand)]
pub enum StorageAction {
    /// 添加或修改一个 WebDAV 存储：写进 config.toml，不含密码
    Add {
        /// 存储的名字，比如 home-nas
        name: String,
        /// WebDAV 地址，比如 https://dav.example.com/dav
        #[arg(long)]
        url: String,
        /// 用户名；没有认证的存储可以不写
        #[arg(long)]
        username: Option<String>,
        /// 像 curl --resolve：直接连这个 IP（端口取自地址），TLS、证书和 Host 头仍用地址里的主机名；固定了地址就不走代理
        #[arg(long)]
        resolve: Option<IpAddr>,
    },
    /// 设置存储的密码：终端里输入，不回显；不在终端里就从标准输入读一行。存进系统钥匙串，没有钥匙串的系统存进只有本人能读的文件
    Login {
        /// 存储的名字
        name: String,
    },
    /// 只读检查：用配置的地址和密码问一下根目录（PROPFIND），打印结果；不写任何东西
    Test {
        /// 存储的名字；只配了一个时不用写
        name: Option<String>,
    },
}

pub async fn run(action: StorageAction) -> anyhow::Result<()> {
    let paths = Paths::from_env();
    match action {
        StorageAction::Add {
            name,
            url,
            username,
            resolve,
        } => add(&paths, name, url, username, resolve).await,
        StorageAction::Login { name } => login(&paths, name).await,
        StorageAction::Test { name } => test(&paths, name).await,
    }
}

async fn add(
    paths: &Paths,
    name: String,
    url: String,
    username: Option<String>,
    resolve: Option<IpAddr>,
) -> anyhow::Result<()> {
    config::check_storage_name(&name)?;
    let parsed = storage::parse_url(&url)?;
    let host = parsed.host_str().unwrap_or_default();
    if resolve.is_some() && (host.parse::<IpAddr>().is_ok() || host.starts_with('[')) {
        bail!("地址里的主机已经是 IP，不需要 --resolve");
    }
    let entry = StorageConfig {
        kind: "webdav".to_string(),
        url: url.trim_end_matches('/').to_string(),
        username: username.filter(|u| !u.is_empty()),
        resolve: resolve.map(|ip| ip.to_string()),
    };
    let replaced = config::save_storage(paths, &name, &entry)?;
    println!(
        "已{}存储 {name}，写进了 {}（不含密码）",
        if replaced { "更新" } else { "添加" },
        paths.config().display()
    );
    if parsed.scheme() == "http" && !is_loopback(host) {
        eprintln!("警告：用的是 http，密码会明文经过网络；能用 https 就用 https");
    }
    if entry.username.is_none() {
        println!("没有设置用户名，会不带认证连接。要认证的话，加上 --username 再运行一次");
    } else {
        println!("下一步：nekorisu storage login {name}");
    }
    // A running core reads config.toml again; without one there is nothing to tell.
    if let Some(mut core) = client::connect_existing().await {
        match core.reload_config(ReloadConfigRequest {}).await {
            Ok(_) => println!("已通知正在运行的 core 重新读配置"),
            Err(err) => eprintln!(
                "没能通知 core 重新读配置（{}）；运行 nekorisu core stop 后再用",
                err.message()
            ),
        }
    }
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

async fn login(paths: &Paths, name: String) -> anyhow::Result<()> {
    let config = config::load(paths)?;
    let Some(storage) = config.storage.get(&name) else {
        bail!("config.toml 里没有叫 {name} 的存储；先运行 nekorisu storage add {name} --url <地址>");
    };
    let has_username = storage.username.is_some();
    let password = read_password(&format!("存储 {name} 的密码（输入时不显示）："))?;
    let (store_paths, store_name) = (paths.clone(), name.clone());
    tokio::task::spawn_blocking(move || secrets::store(&store_paths, &store_name, &password)).await??;
    println!("已把存储 {name} 的密码存进{}", secrets::place());
    if !has_username {
        println!(
            "提醒：这个存储没有设置用户名，密码用不上。要认证的话：nekorisu storage add {name} --url <地址> --username <用户名>"
        );
    }
    println!("检查一下：nekorisu storage test {name}");
    Ok(())
}

async fn test(paths: &Paths, name: Option<String>) -> anyhow::Result<()> {
    let config = config::load(paths)?;
    let (name, storage_config) = storage::pick(&config, name.as_deref().unwrap_or_default())?;
    let store = storage::connect(paths, name, storage_config).await?;
    let status = store.probe().await?;
    let (ok, verdict) = judge(status);
    let pinned = match &storage_config.resolve {
        Some(ip) => format!("（连接固定到 {ip}，不走代理）"),
        None => String::new(),
    };
    println!("存储 {name}：{verdict}{pinned}");
    if !ok {
        bail!("存储 {name} 没通过检查（HTTP {status}）");
    }
    println!("这个检查只读，没有写任何东西");
    Ok(())
}

/// What a PROPFIND answer on the storage root means.
fn judge(status: u16) -> (bool, String) {
    match status {
        200 | 207 => (true, format!("可用，认证通过，根目录能读（HTTP {status}）")),
        401 => (
            false,
            "拒绝了用户名或密码（HTTP 401）；密码可以用 nekorisu storage login 重设".to_string(),
        ),
        403 => (false, "不允许访问这个地址（HTTP 403）".to_string()),
        404 => (false, "根目录不存在（HTTP 404）；检查地址里的路径".to_string()),
        405 | 501 => (false, format!("看起来不是 WebDAV 服务（HTTP {status}）；检查地址")),
        _ => (false, format!("回应了 HTTP {status}")),
    }
}

/// A password from the terminal without echo, or, when stdin is not a terminal (scripts, pipes),
/// one line from stdin. Never printed.
fn read_password(prompt: &str) -> anyhow::Result<String> {
    // SAFETY: isatty only inspects a file descriptor.
    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let mut line = String::new();
    if interactive {
        eprint!("{prompt}");
        io::stderr().flush().ok();
        let _echo_off = EchoOff::new(libc::STDIN_FILENO).context("没法关掉终端的回显，不在终端里输入密码")?;
        let read = io::stdin().lock().read_line(&mut line);
        eprintln!(); // the Enter key was not echoed
        read.context("读不了输入")?;
    } else {
        io::stdin().lock().read_line(&mut line).context("读不了标准输入")?;
    }
    let password = line.strip_suffix('\n').unwrap_or(&line);
    let password = password.strip_suffix('\r').unwrap_or(password);
    if password.is_empty() {
        bail!("没有读到密码");
    }
    Ok(password.to_string())
}

/// Turns terminal echo off while alive and restores it afterwards.
struct EchoOff {
    fd: i32,
    saved: libc::termios,
}

impl EchoOff {
    fn new(fd: i32) -> Option<Self> {
        let mut term = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `term` is a valid out-pointer; it is only read after tcgetattr says it filled it in.
        let saved = unsafe {
            if libc::tcgetattr(fd, term.as_mut_ptr()) != 0 {
                return None;
            }
            term.assume_init()
        };
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        // SAFETY: `quiet` is a fully initialised termios copied from the terminal's own settings.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            return None;
        }
        Some(Self { fd, saved })
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restores exactly the settings `new` read from this descriptor.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judges_the_status_of_a_root_listing() {
        assert!(judge(207).0 && judge(200).0);
        for status in [401, 403, 404, 405, 500, 502] {
            assert!(!judge(status).0, "{status}");
        }
        assert!(judge(401).1.contains("storage login"));
    }

    #[test]
    fn knows_loopback_hosts() {
        for host in ["localhost", "127.0.0.1", "[::1]", "a.localhost"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["dav.example.com", "192.168.1.5", "10.0.0.2"] {
            assert!(!is_loopback(host), "{host}");
        }
    }
}

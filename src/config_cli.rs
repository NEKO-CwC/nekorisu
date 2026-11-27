//! `nekorisu config set | unset`: this device's own values for keys the trusted source can also
//! push (`settings`). Written into config.toml; `--lock` keeps the source's value from applying.
//! Path maps are set in config.toml by hand (`[[paths.map]]`, `lock = true`).

use clap::Subcommand;

use crate::client;
use crate::config;
use crate::paths::Paths;
use crate::proto::ReloadConfigRequest;
use crate::settings;

#[derive(Subcommand)]
pub enum ConfigAction {
    /// 设置这台设备自己的值：local.copy_form（pruned/full）、update.channel（stable/beta）、update.urls（地址，逗号分隔）。加 --lock：来源下发的值不生效，要改只能由你在这台设备上确认强制覆盖
    Set {
        key: String,
        value: String,
        #[arg(long)]
        lock: bool,
    },
    /// 去掉这台设备自己的值（同时解锁），改用来源下发的或默认的
    Unset { key: String },
}

pub async fn run(action: ConfigAction) -> anyhow::Result<()> {
    let paths = Paths::from_env();
    match action {
        ConfigAction::Set { key, value, lock } => {
            let value = settings::parse_cli(&key, &value)?;
            config::edit(&paths, |table| settings::set(table, &key, Some(&value), Some(lock)))?;
            println!(
                "已把 {key} 设成 {value}{}，写进了 {}",
                if lock { "并锁定" } else { "" },
                paths.config().display()
            );
        }
        ConfigAction::Unset { key } => {
            config::edit(&paths, |table| settings::set(table, &key, None, Some(false)))?;
            println!("已去掉这台设备自己的 {key}，改用来源下发的或默认的");
        }
    }
    if let Some(mut core) = client::connect_existing().await
        && let Err(err) = core.reload_config(ReloadConfigRequest {}).await
    {
        eprintln!(
            "没能通知 core 重新读配置（{}）；运行 nekorisu core stop，再运行 nekorisu core start",
            err.message()
        );
    }
    Ok(())
}

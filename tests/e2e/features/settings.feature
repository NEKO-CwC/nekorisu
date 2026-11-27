# language: zh-CN
功能: 本机设置和来源下发的值
  local.copy_form、update.channel、update.urls 两边都能设。本机用 nekorisu config set 设，
  --lock 锁定；没设的用来源下发的；锁定的只能经强制覆盖、由用户在终端里确认才改。

  背景:
    假如 有一个 WebDAV 存储
    而且 有一个配置来源，设备令牌是 "synthetic-device-token"
    而且 设备 "B" 已连上这个存储，core 在运行
    而且 设备 "B" 设置了这个来源并登录

  场景: 没设的用来源下发的
    假如 来源下发配置：
      """
      { "local": { "copy_form": "full" } }
      """
    当 在设备 "B" 上运行 "source sync"
    而且 在设备 "B" 上运行 "source status"
    那么 输出里有 "local.copy_form：full（来源下发的）"

  场景: 锁定的值不被下发的覆盖，接受强制覆盖后解锁
    当 在设备 "B" 上运行 "config set local.copy_form full --lock"
    那么 命令成功
    假如 来源下发配置：
      """
      { "local": { "copy_form": "pruned" },
        "force": [ { "id": "f-copy", "devices": [], "changes": [ { "key": "local.copy_form", "value": "pruned" } ] } ] }
      """
    当 在设备 "B" 上运行 "source sync"
    而且 在设备 "B" 上运行 "source status"
    那么 输出里有 "local.copy_form：full（本机设的，已锁定"
    当 在设备 "B" 的终端里运行 "source accept f-copy"，输入 "f-copy"
    那么 命令成功
    而且 设备 "B" 的 config.toml 里有 "pruned"
    而且 设备 "B" 的 config.toml 里没有 "locked"

  场景: 不合法的值不写
    当 在设备 "B" 上运行 "config set local.copy_form half"
    那么 命令失败
    当 在设备 "B" 上运行 "config set source.url https://elsewhere.example"
    那么 命令失败

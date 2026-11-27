# language: zh-CN
功能: 来源请求强制覆盖，用户本人在终端里确认
  本机锁定的值，来源下发的不生效；来源可以发强制覆盖请求，但设备先不应用，
  只列为待确认并上报。用户在这台设备的终端里运行 nekorisu source accept <请求ID>，
  再输入一次请求 ID，才应用：值成为这台设备自己的，并解锁。Agent 没有终端，替不了用户。

  背景:
    假如 有一个 WebDAV 存储
    而且 有一个配置来源，设备令牌是 "synthetic-device-token"
    而且 设备 "A" 已连上这个存储，core 在运行
    而且 设备 "A" 的 Claude Code 里有会话 "s1"，工作目录是 "/srv/proj"
    而且 在设备 "A" 上运行 "archive s1"
    而且 设备 "B" 连着这个存储
    而且 设备 "B" 的 config.toml 里加上：
      """
      [[paths.map]]
      from = '/srv/proj'
      to = '~B/mine'
      lock = true
      """
    而且 设备 "B" 的 core 在运行
    而且 设备 "B" 设置了这个来源并登录
    而且 来源下发配置：
      """
      { "force": [ { "id": "f-1", "devices": ["@B"], "changes": [
        { "key": "paths.map", "value": { "from": "/srv/proj", "to": "~B/forced" } }
      ] } ] }
      """
    而且 在设备 "B" 上运行 "source sync"

  场景: 请求等用户确认，并上报为待确认
    当 在设备 "B" 上运行 "source status"
    那么 输出里有 "待你确认的请求"
    而且 输出里有 "f-1"
    而且 来源收到设备 "B" 的上报，请求 "f-1" 是 "待确认"
    当 在设备 "B" 上运行 "status"
    那么 输出里有 "等你本人在终端里确认"

  场景: 没有终端不能接受
    当 在设备 "B" 上运行 "source accept f-1"
    那么 命令失败
    而且 输出里有 "终端"
    而且 设备 "B" 的 config.toml 里没有 "forced"

  场景: 输错请求 ID 什么都不改
    当 在设备 "B" 的终端里运行 "source accept f-1"，输入 "f-2"
    那么 命令失败
    而且 设备 "B" 的 config.toml 里没有 "forced"

  场景: 在终端里确认后生效，解锁，并上报
    当 在设备 "B" 的终端里运行 "source accept f-1"，输入 "f-1"
    那么 命令成功
    而且 设备 "B" 的 config.toml 里有 "forced"
    而且 设备 "B" 的 config.toml 里没有 "lock = true"
    当 在设备 "B" 上运行 "source sync"
    那么 来源收到设备 "B" 的上报，请求 "f-1" 是 "已接受"
    当 在设备 "B" 上运行 "restore s1"
    那么 设备 "B" 上的会话 "s1" 在工作目录 "~B/forced" 对应的项目文件夹里

  场景: 拒绝
    当 在设备 "B" 上运行 "source reject f-1"
    那么 命令成功
    当 在设备 "B" 上运行 "source sync"
    那么 来源收到设备 "B" 的上报，请求 "f-1" 是 "已拒绝"
    当 在设备 "B" 上运行 "source accept f-1"
    那么 命令失败

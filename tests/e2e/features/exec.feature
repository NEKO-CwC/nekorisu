# language: zh-CN
功能: 运行来源下发的脚本（接受所有内容）
  默认关。只能在这台设备上打开：有来源的放行请求时输入它的 ID，没有时输入这台设备的名字；
  都要在终端里。本机随时可以关，不用确认。来源只能请求打开，收回则马上生效。

  背景:
    假如 有一个 WebDAV 存储
    而且 有一个配置来源，设备令牌是 "synthetic-device-token"
    而且 设备 "B" 已连上这个存储，core 在运行
    而且 设备 "B" 设置了这个来源并登录

  场景: 没有终端不能打开
    当 在设备 "B" 上运行 "source allow-exec"
    那么 命令失败
    而且 输出里有 "终端"

  场景: 没有请求时输入设备名打开，本机关掉不用确认
    当 在设备 "B" 的终端里运行 "source allow-exec"，输入 "%B"
    那么 命令成功
    当 在设备 "B" 上运行 "source status"
    那么 输出里有 "运行来源下发的脚本：允许"
    当 在设备 "B" 上运行 "source deny-exec"
    那么 命令成功
    当 在设备 "B" 上运行 "source status"
    那么 输出里有 "运行来源下发的脚本：不允许"

  场景: 回应来源的放行请求
    假如 来源下发配置：
      """
      { "requests": [ { "id": "r-allow", "action": "EXEC_ACTION_ALLOW", "devices": ["@B"] } ] }
      """
    当 在设备 "B" 上运行 "source sync"
    而且 在设备 "B" 的终端里运行 "source allow-exec"，输入 "r-allow"
    那么 命令成功
    当 在设备 "B" 上运行 "source sync"
    那么 来源收到设备 "B" 的上报，请求 "r-allow" 是 "已接受"
    而且 来源收到设备 "B" 的上报，允许运行脚本

  场景: 来源收回马上生效
    当 在设备 "B" 的终端里运行 "source allow-exec"，输入 "%B"
    那么 命令成功
    假如 来源下发配置：
      """
      { "requests": [ { "id": "r-revoke", "action": "EXEC_ACTION_REVOKE", "devices": [] } ] }
      """
    当 在设备 "B" 上运行 "source sync"
    那么 来源收到设备 "B" 的上报，不允许运行脚本
    而且 来源收到设备 "B" 的上报，请求 "r-revoke" 是 "已接受"

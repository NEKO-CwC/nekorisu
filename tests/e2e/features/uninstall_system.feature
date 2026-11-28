# language: zh-CN
功能: 卸载真的安装
  用真的安装脚本从本地的发布文件夹装上：登录时启动、钥匙串（macOS）、用户的 PATH（Windows）
  都是真的，只是用测试专用的名字。再用装好的命令卸载，查系统里什么都不剩。
  有 Claude Code、Codex 的机器上，用它们自己的命令接上 MCP 再卸载；连着测试服务端时，
  看 Claude Code 退出登录时，服务端那条授权有没有一起撤销。

  场景: 用安装脚本装上，再卸载，什么都不剩
    假如 有一个 WebDAV 存储
    而且 设备 "A" 用安装脚本装好了 nekorisu
    而且 设备 "A" 的存储 "test" 设好了密码
    而且 设备 "A" 上有一个 Agent 正连着 nekorisu mcp
    当 在设备 "A" 上用装好的 nekorisu 运行 "uninstall --no-restore --yes"
    那么 命令成功
    而且 输出里有 "卸载完了"
    而且 设备 "A" 上装的东西都不在了

  @claude @codex
  场景: Claude Code、Codex 里的 MCP，用它们自己的命令删掉
    假如 有一个 WebDAV 存储
    而且 设备 "A" 连着这个存储
    而且 设备 "A" 的 Claude Code 用 claude 命令接了 nekorisu 的 MCP
    而且 设备 "A" 的 Codex 用 codex 命令接了 nekorisu 和 nekorisu-server 的 MCP
    当 在设备 "A" 上运行 "uninstall --no-restore --yes"
    那么 命令成功
    而且 设备 "A" 的 Claude Code 里没有 MCP "nekorisu" 了
    而且 设备 "A" 的 Codex 里没有 MCP "nekorisu" 了
    而且 设备 "A" 的 Codex 里没有 MCP "nekorisu-server" 了
    而且 设备 "A" 的 skill "nekorisu" 不在了
    而且 设备 "A" 的 Codex 的 skill "nekorisu" 不在了
    而且 输出里有 "在 Codex 里登录过 nekorisu-server 的话"

  @server @claude @unix
  场景: Claude Code 退出服务端的登录时，服务端那条授权一起撤销
    假如 有一个真的服务端
    而且 有一个 WebDAV 存储
    而且 设备 "A" 连着这个存储
    而且 设备 "A" 的 Claude Code 用 claude 命令接了这个服务端的 MCP，并登录了
    当 在设备 "A" 上运行 "uninstall --no-restore --yes"
    那么 命令成功
    而且 panel 上没有这个 Claude Code 的授权了
    而且 设备 "A" 的 Claude Code 里没有 MCP "nekorisu-server" 了

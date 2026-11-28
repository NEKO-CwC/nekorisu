# language: zh-CN
功能: 卸载
  nekorisu uninstall 先列出要删的东西，问清楚再删，删完再查一遍。
  Agent 目录里的会话和存储里的备份不动；这台电脑传过的会话要不要先从存储放回来、
  restore 替换下来的本机旧副本留不留，由人决定：不在终端里时用选项回答，没回答就什么都不删。

  背景:
    假如 有一个 WebDAV 存储
    而且 设备 "A" 已连上这个存储，core 在运行
    而且 设备 "A" 的 Claude Code 里有会话 "s1"，工作目录是 "~A/work/proj"
    而且 在设备 "A" 上运行 "archive s1"

  场景: 不在终端里、没带选项时，只列出要删的，什么都不删
    当 在设备 "A" 上运行 "uninstall"
    那么 命令失败
    而且 输出里有 "卸载 nekorisu 会删掉这些"
    而且 输出里有 "存储 test 里的会话和记录：那是你的备份"
    而且 输出里有 "什么都没删"
    而且 输出里有 "--restore 或 --no-restore"
    而且 输出里有 "test 里 1 个"
    而且 输出里有 "--yes"
    而且 设备 "A" 的 nekorisu 目录还在

  场景: 先把这台电脑传过的会话从存储放回来，再删干净
    假如 设备 "A" 上的会话 "s1" 删掉了
    而且 设备 "A" 的 Claude Code 装了 skill "nekorisu"
    而且 设备 "A" 的 Claude Code 里有一个别人的 skill 叫 "nekorisu-server"
    而且 设备 "A" 的 Claude Code 给 MCP "nekorisu" 记过日志
    当 在设备 "A" 上运行 "uninstall --restore --yes"
    那么 命令成功
    而且 输出里有 "1 个放回或补全"
    而且 输出里有 "卸载完了"
    而且 设备 "A" 上的会话 "s1" 在工作目录 "~A/work/proj" 对应的项目文件夹里
    而且 设备 "A" 的 nekorisu 目录不在了
    而且 设备 "A" 的 skill "nekorisu" 不在了
    而且 设备 "A" 的 Claude Code 没有 MCP "nekorisu" 的日志了
    而且 设备 "A" 的 skill "nekorisu-server" 还在
    而且 输出里有 "不是 nekorisu 的 skill，没删"

  场景: 本机这份比存储里的多，放回时不动它
    假如 设备 "A" 上的会话 "s1" 又多了一行
    当 在设备 "A" 上运行 "uninstall --restore --yes"
    那么 命令成功
    而且 输出里有 "本机这份比存储里的多，没动"
    而且 设备 "A" 的 nekorisu 目录不在了

  场景: restore 替换下来的旧副本，要人说留还是删
    假如 设备 "A" 的 nekorisu 目录里有文件 "replaced/1794700000000/x.jsonl"
    当 在设备 "A" 上运行 "uninstall --no-restore --yes"
    那么 命令失败
    而且 输出里有 "--replaced keep 或 --replaced delete"
    而且 设备 "A" 的 nekorisu 目录还在
    当 在设备 "A" 上运行 "uninstall --no-restore --replaced keep --yes"
    那么 命令成功
    而且 设备 "A" 的主目录里有文件 "nekorisu-replaced/1794700000000/x.jsonl"
    而且 设备 "A" 的 nekorisu 目录不在了

  @unix
  场景: 先退出服务端的登录，再删掉 Agent 里的 MCP
    假如 设备 "A" 有一个替身 claude 命令
    而且 设备 "A" 的 Claude Code 里接了 MCP nekorisu 和 nekorisu-server
    当 在设备 "A" 上运行 "uninstall --no-restore --yes"
    那么 命令成功
    而且 设备 "A" 的替身 claude 命令依次收到：
      """
      mcp logout nekorisu-server
      mcp remove -s user nekorisu
      mcp remove -s user nekorisu-server
      """
    而且 输出里有 "卸载完了"

  场景: 在终端里回答"不"，什么都不动
    假如 设备 "B" 连着这个存储
    当 在设备 "B" 的终端里运行 "uninstall"，输入 "n"
    那么 命令成功
    而且 输出里有 "没有卸载，什么都没动"
    而且 设备 "B" 的 nekorisu 目录还在

  场景: 在终端里确认后卸载，core 也退出了
    假如 设备 "B" 已连上这个存储，core 在运行
    当 在设备 "B" 的终端里运行 "uninstall"，输入 "y"
    那么 命令成功
    而且 输出里有 "卸载完了"
    而且 设备 "B" 的 nekorisu 目录不在了
    当 在设备 "B" 上运行 "core status"
    那么 输出里有 "core 没在运行"

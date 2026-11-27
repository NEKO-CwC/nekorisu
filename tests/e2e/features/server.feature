# language: zh-CN
@server
功能: 设备和真的服务端：上报、在 panel 里改映射、强制覆盖
  连一个测试用的 nekorisu-server（tests/e2e/server.sh 会起一个，用测试令牌和测试管理员密码），
  没有就跳过。管理员的操作走 panel 的接口；设备上的确认在终端里。
  每个场景开始时清空 panel 里的路径映射和强制覆盖请求，所以这些场景一个一个跑。

  背景:
    假如 有一个 WebDAV 存储
    而且 有一个真的服务端
    而且 设备 "A" 已连上这个存储，core 在运行
    而且 设备 "A" 的 Claude Code 里有会话 "s1"，工作目录是 "/srv/proj"
    而且 设备 "A" 的 Claude Code 里有会话 "s2"，工作目录是 "/srv/proj"
    而且 在设备 "A" 上运行 "archive s1 s2"
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

  @serial
  场景: panel 看得到设备的规则；锁定的规则只有设备上接受强制覆盖后才变
    那么 panel 上设备 "B" 上报了路径映射 "/srv/proj"
    当 管理员在 panel 里把设备 "B" 的 "/srv/proj" 映射到 "~B/theirs"
    而且 在设备 "B" 上运行 "source sync"
    那么 命令成功
    当 在设备 "B" 上运行 "restore s1"
    那么 设备 "B" 上的会话 "s1" 在工作目录 "~B/mine" 对应的项目文件夹里
    当 管理员在 panel 里请求设备 "B" 把 "/srv/proj" 强制改成 "~B/forced"，记为 "f-1"
    而且 在设备 "B" 上运行 "source sync"
    那么 panel 上请求 "f-1" 是 "待确认"
    当 在设备 "B" 上运行 "source accept f-1"
    那么 命令失败
    当 在设备 "B" 的终端里运行 "source accept f-1"，输入 "f-1"
    那么 命令成功
    当 在设备 "B" 上运行 "source sync"
    那么 panel 上请求 "f-1" 是 "已接受"
    当 在设备 "B" 上运行 "restore s2"
    那么 设备 "B" 上的会话 "s2" 在工作目录 "~B/forced" 对应的项目文件夹里

  @serial
  场景: 设备上拒绝，panel 看得到；撤回以后设备上不再列出
    当 管理员在 panel 里请求设备 "B" 把 "/srv/proj" 强制改成 "~B/forced"，记为 "f-2"
    而且 在设备 "B" 上运行 "source sync"
    而且 在设备 "B" 上运行 "source reject f-2"
    那么 命令成功
    当 在设备 "B" 上运行 "source sync"
    那么 panel 上请求 "f-2" 是 "已拒绝"
    当 管理员在 panel 里撤回请求 "f-2"
    而且 在设备 "B" 上运行 "source sync"
    而且 在设备 "B" 上运行 "source status"
    那么 输出里没有 "f-2"
    而且 设备 "B" 的 config.toml 里有 "lock = true"

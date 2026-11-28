# language: zh-CN
功能: 从配置来源取签名的配置，路径映射两边同步
  设备只认本机设置的来源和它的公钥；令牌在终端里输入。core 拉来配置，验签名、序号，
  留住上一份好的。路径映射按 from 逐条合并：本机写的先生效；来源之后改了这一条，
  就用来源的；本机锁了的一直用本机的；来源下发的 to 为空就是删掉本机那条。
  设备把自己写的映射报给来源，来源那边可以看、可以改。

  背景:
    假如 有一个 WebDAV 存储
    而且 有一个配置来源，设备令牌是 "synthetic-device-token"
    而且 设备 "A" 已连上这个存储，core 在运行
    而且 设备 "A" 的 Claude Code 里有会话 "s1"，工作目录是 "/srv/proj"
    而且 在设备 "A" 上运行 "archive s1"

  场景: 没有令牌取不到配置
    假如 设备 "B" 已连上这个存储，core 在运行
    而且 设备 "B" 设置了这个来源
    当 在设备 "B" 上运行 "source sync"
    那么 命令失败
    而且 输出里有 "source login"

  场景: 来源下发的映射决定恢复到哪
    假如 设备 "B" 已连上这个存储，core 在运行
    而且 设备 "B" 设置了这个来源并登录
    而且 来源下发配置：
      """
      { "paths": { "map": [
        { "from": "/srv/proj", "to": "~B/from-source", "devices": ["@B"] },
        { "from": "/srv/proj", "to": "~B/not-for-b", "devices": ["someone-else"] }
      ] } }
      """
    当 在设备 "B" 上运行 "source sync"
    那么 命令成功
    当 在设备 "B" 上运行 "restore s1"
    那么 命令成功
    而且 设备 "B" 上的会话 "s1" 在工作目录 "~B/from-source" 对应的项目文件夹里

  场景: 本机写的先生效，来源之后改了这一条就用来源的
    假如 设备 "B" 连着这个存储
    而且 设备 "B" 的 config.toml 里加上：
      """
      [[paths.map]]
      from = '/srv/proj'
      to = '~B/mine'
      """
    而且 设备 "B" 的 core 在运行
    而且 设备 "B" 设置了这个来源并登录
    那么 来源收到设备 "B" 的上报，里面有路径映射 "/srv/proj"
    当 来源下发配置：
      """
      { "paths": { "map": [ { "from": "/srv/proj", "to": "~B/theirs", "devices": ["@B"] } ] } }
      """
    而且 在设备 "B" 上运行 "source sync"
    而且 在设备 "B" 上运行 "source status"
    那么 输出里有 "来源下发的"
    当 在设备 "B" 上运行 "restore s1"
    那么 设备 "B" 上的会话 "s1" 在工作目录 "~B/theirs" 对应的项目文件夹里

  场景: 本机锁定的映射，来源下发的不生效
    假如 设备 "B" 连着这个存储
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
      { "paths": { "map": [ { "from": "/srv/proj", "to": "~B/theirs", "devices": [] } ] } }
      """
    当 在设备 "B" 上运行 "source sync"
    而且 在设备 "B" 上运行 "restore s1"
    那么 设备 "B" 上的会话 "s1" 在工作目录 "~B/mine" 对应的项目文件夹里

  场景: 来源删掉本机的映射
    假如 设备 "B" 连着这个存储
    而且 设备 "B" 的 config.toml 里加上：
      """
      [[paths.map]]
      from = '/srv/proj'
      to = '~B/mine'
      """
    而且 设备 "B" 的 core 在运行
    而且 设备 "B" 设置了这个来源并登录
    而且 来源下发配置：
      """
      { "paths": { "map": [ { "from": "/srv/proj", "to": "", "devices": ["@B"] } ] } }
      """
    当 在设备 "B" 上运行 "source sync"
    而且 在设备 "B" 上运行 "restore s1"
    那么 设备 "B" 上的会话 "s1" 在工作目录 "/srv/proj" 对应的项目文件夹里

  场景: 撤掉来源以后，它下发的映射和令牌都不在了
    假如 设备 "B" 已连上这个存储，core 在运行
    而且 设备 "B" 设置了这个来源并登录
    而且 来源下发配置：
      """
      { "paths": { "map": [ { "from": "/srv/proj", "to": "~B/from-source", "devices": ["@B"] } ] } }
      """
    当 在设备 "B" 上运行 "source sync"
    那么 命令成功
    当 在设备 "B" 上运行 "source remove"
    那么 命令成功
    而且 输出里有 "已撤掉来源 home"
    当 在设备 "B" 上运行 "source status"
    那么 输出里有 "还没有设置来源"
    当 在设备 "B" 上运行 "restore s1"
    那么 命令成功
    而且 设备 "B" 上的会话 "s1" 在工作目录 "/srv/proj" 对应的项目文件夹里
    当 在设备 "B" 上运行 "source remove"
    那么 命令成功
    而且 输出里有 "没有设置来源"
    假如 设备 "B" 设置了这个来源
    当 在设备 "B" 上运行 "source sync"
    那么 命令失败
    而且 输出里有 "source login"

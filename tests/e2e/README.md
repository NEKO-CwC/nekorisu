# 端到端测试

`features/` 里每个 `.feature` 文件记录一组功能：用中文写成场景，每个场景就是一个可以执行的测试。测试启动真的 `nekorisu`（每台“设备”各有自己的目录和一个在运行的 core），存储是测试进程里的 WebDAV，配置来源是测试进程里的替身，用测试密钥签名。数据全是合成的。

## 运行

```bash
cargo test --test e2e                       # 全部
E2E_NAME=强制 cargo test --test e2e          # 只跑名字里有“强制”的场景
```

`cargo test` 也会跑它们。CI 在 Linux、macOS、Windows 上各跑一遍。

在别的机器上跑编好的测试程序时，用 `NEKORISU_E2E_BIN` 指向 `nekorisu` 程序，`NEKORISU_E2E_FEATURES` 指向 `features` 文件夹。

## 连真的服务端

标了 `@server` 的场景（`server.feature`）连一个真的 nekorisu-server：管理员的操作走 panel 的接口，设备走 gRPC。没有服务端时这些场景跳过。`server.sh` 用 Docker 起一个一次性的服务端（测试令牌、测试管理员密码、空的数据），跑完就停：

```bash
NEKORISU_E2E_SERVER_IMAGE=<服务端镜像> tests/e2e/server.sh
```

它会设好下面几个变量，再运行后面的命令（默认 `cargo test --test e2e`）：

| 变量 | 内容 |
|---|---|
| `NEKORISU_E2E_SERVER` | panel 的地址，如 `http://127.0.0.1:18080` |
| `NEKORISU_E2E_SERVER_GRPC` | 设备协议的地址，如 `http://127.0.0.1:17070` |
| `NEKORISU_E2E_SERVER_TOKEN` | 设备令牌 |
| `NEKORISU_E2E_SERVER_PASSWORD` | 管理员密码 |

只能指向测试用的服务端：场景会清空它的路径映射和强制覆盖请求。客户端只许用 http 连本机，所以别的机器要连，先用 SSH 端口转发把这两个端口转到那台机器的 `127.0.0.1`。`NEKORISU_E2E_SERVER_KEEP=1` 时服务端留着不停，并打印要设的变量。端口用 `NEKORISU_E2E_SERVER_PORTS="<http> <grpc>"` 换。

## 写场景

文件开头写 `# language: zh-CN`，关键字是 `功能`、`背景`、`场景`、`假如`、`当`、`那么`、`而且`、`但是`。引号用英文的 `"`。

引号里的文字会替换：

| 写法 | 换成 |
|---|---|
| `~B` | 设备 B 的主目录 |
| `@B` | 设备 B 的设备 ID |
| `%B` | 设备 B 的名字 |
| 场景里起的会话名，比如 `s1` | 它的会话 ID |

能用的句子在 `steps.rs`，比如：

- `假如 有一个 WebDAV 存储`、`假如 有一个配置来源，设备令牌是 "..."`
- `假如 设备 "B" 已连上这个存储，core 在运行`；分开写是 `设备 "B" 连着这个存储` 和 `设备 "B" 的 core 在运行`
- `假如 设备 "B" 的 config.toml 里加上：`，下面跟一段 `"""` 文本（core 在运行就重启它）
- `假如 设备 "A" 的 Claude Code 里有会话 "s1"，工作目录是 "~A/work"`
- `假如 设备 "B" 设置了这个来源并登录`
- `当 来源下发配置：`，下面跟一段 JSON（强制覆盖的值可以写 `"value"`，会换成 `value_json`）
- `当 在设备 "B" 上运行 "restore s1"`（不在终端里，像 Agent 调命令）
- `当 在设备 "B" 的终端里运行 "source accept f-1"，输入 "f-1"`（像人在终端里确认）
- `那么 命令成功`、`那么 命令失败`、`那么 输出里有 "..."`
- `那么 设备 "B" 上的会话 "s1" 在工作目录 "~B/x" 对应的项目文件夹里`
- `那么 来源收到设备 "B" 的上报，请求 "f-1" 是 "已接受"`

还没实现的功能，场景先写上，标 `@wip`，就不会执行；实现以后去掉标签。只在一种系统上成立的场景标 `@unix` 或 `@windows`。没有对应句子的步骤会让测试失败，所以写错了会看得到。

## 真的系统

`uninstall_system.feature` 用真的安装脚本装上再卸载（`system.rs`）：测试把要测的 `nekorisu` 打成和发布版一样的压缩包，放在本地文件夹里交给 `install.sh` / `install.ps1`，装到设备目录下。

- 开机自启：macOS 用真的 launchd（launchd 起不来 core 的机器，比如没有登录界面的 CI，就从装好的命令启动）；Windows 的安装脚本跳过计划任务（计划任务带不过去设备目录），另建一个测试名字的计划任务代替；Linux 这里没有 `systemd --user`，写下 `core enable` 在那种情况下会留下的单元文件。
- macOS 的密码存进真的钥匙串，Windows 的安装脚本改真的用户 PATH。
- 名字都是测试专用的：调试版认 `NEKORISU_E2E_NAME`，launchd 的标签、计划任务名、钥匙串的服务名都带上它，碰不到开发机自己装的那份。场景结束时（不管过没过）把剩下的都清掉。发布版不认这个变量。
- macOS 上不改 `HOME`：钥匙串和 Claude Code 的登录都按真的 `HOME` 找，改了会弹出“找不到钥匙圈”的对话框。nekorisu 自己的主目录用调试版认的 `NEKORISU_E2E_HOME` 换成设备目录。
- 标 `@claude`、`@codex` 的场景用开发机上装的 `claude`、`codex`（配置目录换成设备目录），没装就跳过；也可以用 `NEKORISU_E2E_CLAUDE`、`CODEX_CLI_PATH` 指定。
- 标 `@server @claude` 的场景让真的 Claude Code 在测试服务端上走一遍 OAuth 登录：`BROWSER` 换成一个记下网址的脚本，场景用测试管理员密码在授权页上同意，再看卸载时 `claude mcp logout` 有没有把服务端那条授权撤掉。`server.sh` 为此给服务端设了 `NEKORISU_PUBLIC_URL`。
- 在别的机器上跑时，`NEKORISU_E2E_SCRIPTS` 指向 `scripts` 文件夹。

## 碰不到的

- 真的 Claude Code、Codex 的会话：用合成的会话文件；Codex 的恢复要调 `codex` 程序，这里没有。
- 其他场景的钥匙串：用调试版的 `NEKORISU_SECRETS=file`，密码和令牌存在设备目录下的文件里。
- 自动更新：不在这里测。

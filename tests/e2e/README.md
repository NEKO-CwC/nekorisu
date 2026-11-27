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

## 碰不到的

- 真的 Claude Code、Codex 程序：用合成的会话文件；Codex 的恢复要调 `codex` 程序，这里没有。
- 系统钥匙串：测试用调试版的 `NEKORISU_SECRETS=file`，密码和令牌存在设备目录下的文件里，不碰开发机的钥匙串。发布版不认这个变量。
- 开机自启、自动更新：不在这里测。

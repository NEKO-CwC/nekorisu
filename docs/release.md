# 发版和签名钥匙（给维护者）

> 2026-11-23。这里只写怎么操作。

## 一次性准备

### 1. 生成两把根钥

根钥只在换签名钥时用，一年一两次。放在密码管理器里，再存一份离线备份（比如 U 盘），不需要服务器。

```bash
cargo run --bin nekorisu-release -- keygen --out ~/nekorisu-root-1.key
```

```bash
cargo run --bin nekorisu-release -- keygen --out ~/nekorisu-root-2.key
```

每条命令都会打印一行公钥。然后：

1. 把两个私钥文件的内容分别存进密码管理器，各存一份离线备份，再删掉这两个文件。
2. 把两行公钥按顺序写进 `keys/root.pub`：第一行是正在用的，第二行是备用的。提交到仓库。

`keys/root.pub` 会编进程序。没有公钥时，自动更新是关的，发版流程也会失败。

### 2. 生成签名钥，用根钥给它签证书

```bash
cargo run --bin nekorisu-release -- keygen --out ~/nekorisu-signing-1.key
```

```bash
cargo run --bin nekorisu-release -- certify --root-key ~/nekorisu-root-1.key --signing-pub <上一步打印的公钥> --serial 1 --out keys/release-cert.json
```

- 证书 `keys/release-cert.json` 是公开的，提交到仓库。默认一年有效，到期前要重签一张（见"换签名钥"）。
- 用完把根钥文件删掉，它只该留在密码管理器里。

### 3. 把签名钥放进 GitHub

1. 仓库 Settings → Environments → 新建 `release`：
   - Deployment branches and tags 选 "Selected"，只允许 `v*` 这类 tag；
   - 在这个环境里加 secret `NEKORISU_SIGNING_KEY`，值是 `~/nekorisu-signing-1.key` 的全部内容。
2. 删掉本机的 `~/nekorisu-signing-1.key`。
3. Settings → General 里打开 "Immutable releases"：发布后文件和 tag 都改不了。
4. 给 GitHub 账号开两步验证：签名钥在 CI 里，账号被盗就能签任意版本。

## 每次发版

1. 改 `Cargo.toml` 里的 `version`，提交。
2. 打 tag 并推送，tag 必须和版本号一致：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

- `v0.1.0` 这种发到 stable 渠道；`v0.2.0-beta.1` 这种（带 `-`）发到 beta 渠道，标成预发布。
- `.github/workflows/release.yml` 会构建四个平台，签名 `<渠道>.json`，验证一遍，再上传：
  - 压缩包 `nekorisu-<平台>.tar.gz`；
  - `SHA256SUMS`；
  - `<渠道>.json`；
  - `install.sh`。

  你不用做别的。
- 客户端只升不降，所以版本号只能往上走。

**beta 渠道的限制。** GitHub 的 "latest" 地址不包含预发布，所以 beta 渠道暂时没有固定地址。用 beta 的设备要在配置里写 `update.urls`，指到对应版本的 `beta.json`。

## 换签名钥（泄露了，或者证书快到期）

1. 生成新的签名钥。
2. 从密码管理器取出根钥，签一张新证书，**序号加一**：`--serial 2`。
3. 替换 GitHub 里的 `NEKORISU_SIGNING_KEY`，提交新的 `keys/release-cert.json`。
4. 发一个新版本。

客户端见过序号 2 以后，就不再接受序号 1 的证书签的东西，所以泄露的旧签名钥作废了。客户端不用改。

## 根钥丢了或泄露了

- **一把出问题**：用另一把（备用的）签证书，然后发一个新版本，在 `keys/root.pub` 里换上新生成的一对。
- **两把都出问题**：老版本的客户端没法再自动更新，用户只能用安装脚本重装。

## 本地验证

用测试钥匙在本机跑一遍完整的更新，不碰真钥匙：编译时用 `NEKORISU_ROOT_PUB=<测试公钥文件>` 指定内置的根公钥。验证结束后要重新编译，不要拿测试版本去发布。

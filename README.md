# nekorisu

**你和 AI 的对话属于你：原样保存，逐字恢复，跨设备、跨 Agent。**

nekorisu 把你在 Claude Code、Codex 里的会话原样存到你自己选的存储里，需要时按行读回来，逐字节恢复。

## 安装

点代码框右上角复制，原样发给你的 Agent：

```text
请读 https://raw.githubusercontent.com/NEKO-CwC/nekorisu/main/docs/agent-install.md ，先用手册开头的开场白跟我打个招呼，再按手册帮我装好 nekorisu。动手前先跟我说一声。
```

Agent 会先跟你打个招呼，再一步步带你装好。不懂的地方直接问它。存储密码在它替你打开的终端里输入，服务端在浏览器里登录，都不经过 Agent。

自己部署了 [nekorisu 服务端](https://github.com/NEKO-CwC/nekorisu-server)，就把它的地址一起告诉 Agent。服务端的 `https://<地址>/connect` 不用登录就能看，写着怎么把电脑接上去；存储的地址和用户名，Agent 登录服务端以后自己去取，不用你找。

想先看看它会做什么：[`docs/agent-install.md`](docs/agent-install.md)。

支持 macOS、Linux 和 64 位的 Windows 10 / 11。在 Windows 上想自己装，在 PowerShell 里运行（不需要管理员权限）：

```powershell
powershell -ExecutionPolicy Bypass -NoProfile -Command "irm https://github.com/NEKO-CwC/nekorisu/releases/latest/download/install.ps1 | iex"
```

## 存储放在哪

nekorisu 会在你给的存储地址下面自己建一个 `nekorisu/` 文件夹，所有东西都放在 `nekorisu/v1/` 里。所以地址填它的**上一层**就行，不用自己再建一个叫 nekorisu 的文件夹，否则会变成 `nekorisu/nekorisu/v1/`。

- **普通 WebDAV（NAS、网盘）**：地址填你想放东西的那个文件夹。比如填 `https://dav.example.com/backup`，数据就在 `backup/nekorisu/v1/`。
- **OpenList / Alist**：它的 `/dav` 根是虚拟的，只能写在挂载的存储里。先加一个挂载路径为 `/nekorisu` 的存储，地址填 `https://<你的 OpenList>/dav`，数据就直接在这个挂载里的 `v1/`。
- **用了 [nekorisu 服务端](https://github.com/NEKO-CwC/nekorisu-server)**：服务端的 `NEKORISU_WEBDAV_URL` 要和这里填同一个地址。登录服务端的 panel，"存储"页上就写着电脑该填的地址和用户名。

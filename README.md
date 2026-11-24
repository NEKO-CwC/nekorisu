# nekorisu

把你和 AI Agent（Claude Code、Codex）的会话原样存到你自己选的存储里，需要时按行读回来，逐字节恢复。

## 安装

点代码框右上角复制，原样发给你的 Agent：

```text
请读 https://raw.githubusercontent.com/NEKO-CwC/nekorisu/main/docs/agent-install.md ，先用手册开头的开场白跟我打个招呼，再按手册帮我装好 nekorisu。动手前先跟我说一声。
```

Agent 会先跟你打个招呼，再一步步带你装好。不懂的地方直接问它；密码和令牌由你自己输入，不经过 Agent。

想先看看它会做什么：[`docs/agent-install.md`](docs/agent-install.md)。

## 存储放在哪

nekorisu 会在你给的存储地址下面自己建一个 `nekorisu/` 文件夹，所有东西都放在 `nekorisu/v1/` 里。所以地址填它的**上一层**就行，不用自己再建一个叫 nekorisu 的文件夹，否则会变成 `nekorisu/nekorisu/v1/`。

- **普通 WebDAV（NAS、网盘）**：地址填你想放东西的那个文件夹。比如填 `https://dav.example.com/backup`，数据就在 `backup/nekorisu/v1/`。
- **OpenList / Alist**：它的 `/dav` 根是虚拟的，只能写在挂载的存储里。先加一个挂载路径为 `/nekorisu` 的存储，地址填 `https://<你的 OpenList>/dav`，数据就直接在这个挂载里的 `v1/`。
- **用了 [nekorisu 服务端](https://github.com/NEKO-CwC/nekorisu-server)**：服务端的 `NEKORISU_WEBDAV_URL` 要和这里填同一个地址。

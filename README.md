# Emoo 数据同步工具

把本地文件 / 文件夹同步到 [Emoo AI](https://app.emooai.com) 知识库、成为可检索文档的跨平台桌面客户端。基于 Tauri v2（Rust + Vue 3）。

## 功能

- **文件 / 文件夹同步**：单个文件或整个文件夹（顶层，≤ 20 个）批量上传为知识库文档
- **增量同步**：按内容哈希（SHA-256）识别变更 —— 相同跳过、改动覆盖（删旧建新）、本地删除则远端保留作备份
- **定时同步**：按小时调度，最低 1 小时
- **开机自启**：可选，登录后自动运行
- **文档权限**：为同步的文档配置「可见 / 可删除」范围
- 每个任务独立配置：本地路径、目标知识库文件夹、定时、权限，并带独立日志

## 下载

前往 [Releases](https://github.com/emoo-ai/data-sync-tool/releases) 下载最新版：

| 平台 | 安装包 |
|---|---|
| macOS（Intel + Apple Silicon） | `.dmg`（universal） |
| Windows（x64） | `.msi` / `.exe` |

> 安装包未做代码签名：macOS 首次打开需在「系统设置 → 隐私与安全性」放行；Windows 可能弹出 SmartScreen，选「仍要运行」。

## 使用

首次启动后填入 Emoo 开放平台的 **API Key**（`emoo_` 开头的绑定型密钥，在 Emoo 后台获取），即可新建同步任务。若要设置文档权限，还需填写调用者 open_id（需为工作区超管）。

## 本地开发

前置：Node.js ≥ 20、Rust（stable），系统依赖见 [Tauri 文档](https://tauri.app/start/prerequisites/)。

```bash
npm install
npm run tauri dev      # 开发，前端热更新
npm run tauri build    # 本地打包
```

## 发布

推送 `v*` 形态的 tag（如 `v0.1.0`）即触发 GitHub Actions 自动构建 macOS / Windows 安装包并发布 Release，配置见 [.github/workflows/release.yml](.github/workflows/release.yml)。

```bash
git tag v0.1.0
git push origin v0.1.0
```

## 技术栈

Tauri v2 · Rust · Vue 3 · TypeScript · Vite · SQLite（rusqlite） · reqwest（rustls）

# 网易云音乐适配器

`nons-adapter-netease` 是默认随包提供的受信任原生 Rust 适配器库，使用共享音乐契约版本 `2`。宿主通过 `MusicAdapter` 调用搜索、内容浏览、账号、曲库、收藏、歌单编辑、推荐、私人 FM、单曲与播放解析；平台 SDK、协议与响应转换均在本包。

依赖共享后端契约 `packages/music-adapter-sdk` 和本包的 `vendor/ncm-api-rs`，不依赖宿主 crate、Tauri、AccountManager、SQLite 或系统凭据库。账号数据通过宿主绑定的 `ProviderAccounts` 读取；凭据完成结果交回宿主，不独立保存凭据。固定上游及 HTTP 补丁见 [PATCHES](vendor/ncm-api-rs/PATCHES.md)。

网络目标不依赖 manifest 或封面主机清单。HTTP 客户端自动跟随重定向直到实际目标 URL 重复并返回最终 URL，播放解析把最终 URL 交给宿主；请求默认没有固定期限，`RequestContext.deadline` 的显式正值按原值执行。响应体和通用 JSON 不设包内字节上限，平台分页、歌词、音质及音频试听范围校验保持不变。

独立验收：

```powershell
cargo test --manifest-path Cargo.toml --locked
cargo build --manifest-path Cargo.toml --release --locked
```

上游契约提交尚不可从 git 源获取时，用 `cargo --config` 临时把 `nons-music-adapter-sdk` patch 到本地 `packages/music-adapter-sdk`；提交可获取后刷新 `Cargo.lock` 再执行 `--locked` 验收。测试使用本地模拟 HTTP 服务，不包含真实账号登录与真实播放。

推送到 `main` 或提交 Pull Request 会运行 GitHub Actions；推送 `v*` 标签会在测试和 release 构建通过后发布根目录包含源码 manifest 的 ZIP 和 SHA-256 校验文件。网易云适配器是随宿主编译链接的原生库，不是可独立安装的 WASM Component。

这是原生库源码包，随宿主编译和链接；升级版本需要重新构建应用。它不是阶段四的 WASM Component，不可通过功能插件 ZIP 安装，不具有 WASM 隔离或运行时卸载原生代码的能力。设置停用与重载撤销管理器代次和凭据访问，保留原生代码及共享连接池。详细修改记录、缓存与验收范围见 [适配器文档](../../docs/music-adapters.md)。

## 项目许可证

本项目代码采用 [MIT](License)。内置 ncm-api-rs 的 WTFPL 条款保留在 `vendor/ncm-api-rs/License`，其他依赖遵循各自的原始许可证。

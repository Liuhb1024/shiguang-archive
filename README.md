<div align="center">
  <img src="src/assets/brand/shiguang-mark.png" width="104" height="104" alt="拾光档案图标" />
  <h1>拾光档案</h1>
  <p><strong>把还能找回的空间记忆，留在自己的电脑里。</strong></p>
  <p>Shiguang Archive · QQ 空间本地归档工具</p>
  <p>
    <a href="#快速开始">快速开始</a> ·
    <a href="#功能">功能</a> ·
    <a href="#平台支持">平台支持</a> ·
    <a href="#隐私与安全">隐私与安全</a> ·
    <a href="#常见问题">常见问题</a>
  </p>
  <p>Tauri 2 · Rust · Vue 3 · SQLite · GPLv3</p>
</div>

---

## 为什么做拾光档案

很多年后再打开 QQ 空间，我们想找的可能只是一张旧照片、一段当时写下的话，或评论区里某个熟悉的名字。

我希望做一个简单的本地工具：**每个人使用自己的账号，资料保存在自己的电脑，不把大家的回忆集中到我的服务器。** 拾光档案在开源项目 [QzoneArchive](https://github.com/Gaoshu705/QzoneArchive) 的基础上，继续完善本地数据处理、加载体验与内容来源提示。

它不是完整空间镜像，也不是万能恢复工具。我的原则是：保存实际拿到的资料，明确告诉你缺了什么，不把摘要当全文，不把失败当成功。

> [!IMPORTANT]
> v1.0.0 已在 [Releases](https://github.com/Liuhb1024/shiguang-archive/releases) 提供 **Windows x64 安装包**；其余平台仍以源码运行为主。Apple Silicon Mac 已有本机运行与构建记录；Windows x64 已完成实机编译、打包与启动验证，核心归档流程的端到端验证仍在进行；Intel Mac 尚未完成实机验收。请先查看下方平台状态，不要将跨平台技术栈理解为全平台已验证。

## 功能

| 功能 | 现在可以做什么 |
| --- | --- |
| 扫码登录 | 通过 QQ 扫码授权，无需把 QQ 密码交给本工具 |
| 内容归档 | 整理接口返回的动态、点赞、评论、回复等互动记录 |
| 媒体时间轴 | 按时间浏览照片与视频，下载仍可访问的资源 |
| 任务管理 | 分页获取、请求间隔控制、任务取消与断点续传 |
| 加载体验 | 正文先显示；图片独立加载，使用受控队列、缓存与超时提示 |
| 来源说明 | 区分历史长文本与疑似截断摘要；部分日志可跳转官方页面核验 |
| 多账号隔离 | 按登录账号隔离归档查询与任务状态 |
| 导出与清理 | 导出 HTML 文本记录，或清理应用管理的本地数据 |

**获取范围取决于数据源。** 当前归档基于 QQ 空间移动端互动通知接口，不是完整历史内容接口。没有出现在通知里的记录可能无法找回；已经删除、无访问权限或地址失效的资源，也不保证可以恢复。HTML 导出不是包含全部媒体的离线整站备份。

## 平台支持

| 平台 | 当前证据 | 使用建议 |
| --- | --- | --- |
| macOS · Apple Silicon | 已在维护者本机运行；前端构建、Rust 检查和自动测试通过 | 可按下文尝试源码运行；全新设备安装流程仍待验证 |
| macOS · Intel | 尚未进行实机编译和功能验收 | 待验证，不承诺开箱即用 |
| Windows · x64 | v1.0.0 已提供 NSIS 安装包与免安装单文件；在 Windows 11（build 26200）完成实机编译、打包与启动，扫码登录、归档落库与媒体下载的端到端验证仍在进行 | 可直接从 [Releases](https://github.com/Liuhb1024/shiguang-archive/releases) 下载安装包；源码运行见下文 |
| Linux / Android / iOS | 不在本次发布的验收范围 | 暂不作为受支持平台承诺 |

源码运行、单元测试通过、安装包可以分发，是三件不同的事。签名、公证、安装升级和跨平台回归尚未完成，当前没有自动发布工作流。

## 快速开始

这是桌面应用，不需要部署云服务器或单独安装数据库服务。首次运行需要准备本机开发环境，下载源码本身不会自动安装这些工具。

### 1. 准备基础工具

- **Git**：用于拉取源码，也可以从本仓库下载源码 ZIP 后解压。
- **Node.js 与 npm**：从 [Node.js 官网](https://nodejs.org/en/download) 安装仍受维护的 LTS 版本。
- **Rust 与 Cargo**：通过 [Rust 官方安装入口](https://rust-lang.org/tools/install/) 安装 rustup。项目在 [rust-toolchain.toml](rust-toolchain.toml) 固定了 Rust `1.98.0`；进入项目后，rustup 会按该文件选择工具链。

安装完成后重新打开终端，确认 `git --version`、`node --version`、`npm --version` 和 `cargo --version` 可以执行。依赖安装、工具链下载与首次构建可能需要联网。

### 2. 安装系统编译依赖

<details open>
<summary><strong>macOS</strong></summary>

安装 Xcode Command Line Tools：

```sh
xcode-select --install
```

完成系统安装提示后，用 `xcode-select -p` 确认工具路径。已经安装完整 Xcode 的用户，请先打开 Xcode 完成首次配置。仅开发桌面端不要求额外安装移动端工具链。参见 [Tauri 的 macOS 前置要求](https://v2.tauri.app/start/prerequisites/#macos)。

</details>

<details open>
<summary><strong>Windows</strong></summary>

按照 [Tauri 的 Windows 前置要求](https://v2.tauri.app/start/prerequisites/#windows) 准备以下环境：

1. 安装 Microsoft C++ Build Tools，选择 **Desktop development with C++（使用 C++ 的桌面开发）** 工作负载及其所需 SDK。
2. 确认已安装 Microsoft Edge WebView2 Runtime。
3. 安装 Rust 时选择与设备架构相符的 **MSVC** 工具链，不使用 GNU 工具链作为本项目 Windows 开发配置。
4. 安装完成后重新打开终端，再执行下方命令。

这份步骤已在本机 Windows 11 x64 环境走通编译与打包流程；不代表核心归档流程已在 Windows 验收通过。

</details>

### 3. 拉取并运行

在终端中执行，macOS 与 Windows 使用同一组项目命令：

```sh
git clone https://github.com/Liuhb1024/shiguang-archive.git
cd shiguang-archive
npm ci
npm run tauri dev
```

`npm ci` 使用提交的锁文件安装依赖。首次 Rust 编译可能较慢，等待终端编译完成后会出现桌面窗口。

> `npm run tauri dev` 才会启动完整桌面应用；`npm run dev` 只启动前端开发服务器，不能代替 Rust 后端。不要把开发服务器暴露到公网或不受信任的局域网，也不要为了安装方便关闭系统安全保护。

### 4. 第一次使用

1. 在应用中扫码登录自己的 QQ 账号，只处理本人或已获得授权的资料。
2. 进入任务页开始归档。先使用默认请求间隔，不要为了加速反复重启任务。
3. 在归档页与媒体页查看结果；遇到摘要提示、旧图失效或权限错误时，查看具体说明。
4. 遇到验证码、限流或账号异常，停止请求并稍后再试。归档期间不要切换 QQ 客户端账号。
5. 需要长期保存的内容请另做备份；分享前检查导出文件是否含有自己或好友的隐私。

## 隐私与安全

**我不提供用于收集用户归档的服务端。** 当前应用没有开发者遥测、广告或集中上传归档的接口，每个人使用自己的会话和本地存储。

- **凭据边界**：登录 Cookie 仅保留在 Rust 后端内存，不返回前端，不附加到媒体下载请求，不主动写入日志或磁盘。
- **请求边界**：后端限制请求域名与协议，校验 DNS 地址和重定向；下载有大小限制、超时和取消处理。
- **本地边界**：前端内容安全策略、文件授权和账号范围检查约束资源访问。
- **数据边界**：数据库与媒体在本机保存，但没有应用层加密。操作系统备份、同步软件、其他有权限的本机程序或用户主动分享，仍可能使资料离开当前设备。

> [!WARNING]
> 本地保存不等于完全离线，也不等于零风险。扫码、归档和下载仍会请求腾讯服务，腾讯能够看到这些请求。我不能承诺不会限流、不会触发验证或不会封号，也不会通过绕过平台保护来实现所谓“稳定”。

完整说明与已知依赖风险见 [SECURITY.md](SECURITY.md)。不要把扫码页面、Cookie、数据库、好友名单、私人正文或带签名的资源链接提交到公开 Issue。

## 常见问题

### 为什么有些旧照片找不到？

可能是源记录没有提供图片、资源已删除、权限变化或旧地址失效。软件会显示可确认的状态，但不能仅凭一次加载失败断言照片永久消失。反复重试不一定有用，也可能增加平台请求压力。

### 为什么文章仍然只有一部分？

上游通知可能只返回摘要。程序会保留已经收到的正文，并尝试从同账号、同条动态的本地历史通知中选择更长文本，但“更长”不代表“完整”。如果本地和官方可访问来源都没有后半篇，就不能可靠补全；软件不会生成文字冒充原文。

### 不把数据上传给维护者，就不会被腾讯限制了吗？

不会。维护者是否收集数据，与腾讯是否限制接口调用，是两件不同的事。本地请求仍使用你的账号和网络环境；请求间隔只是降低负载的措施，不是免封号保证。

### 能直接双击运行吗？

Windows x64 可以：从 [Releases](https://github.com/Liuhb1024/shiguang-archive/releases) 下载 v1.0.0 安装包或免安装单文件即可。macOS 与 Linux 目前仍需按下方步骤准备开发环境。本版安装包未做代码签名与公证，首次运行如出现 SmartScreen 提示，请核对下载来源与附件中的 SHA256。

### 退出登录会删除归档吗？

不会，退出登录与删除数据是不同操作。需要清理时使用设置页的“删除所有数据”，并先确认已经备份。该操作不会撤回你另存的文件、系统备份或已经分享的副本。

### 启动时报错怎么办？

- **找不到 cargo、npm 或 git**：确认工具已安装，并重新打开终端使 PATH 生效。
- **Windows 提示找不到链接器或 SDK**：检查 C++ 工作负载、Windows SDK 与 MSVC 工具链。
- **PowerShell 阻止执行 npm.ps1**：可使用 `npm.cmd ci` 和 `npm.cmd run tauri dev`，不必全局放开脚本执行策略。
- **1420 端口被占用**：确认是否已经运行了一份开发实例；先正常结束自己的旧实例，不要盲目终止其他程序。
- **依赖下载失败**：区分网络下载失败和编译错误，保留锁文件，不要随意粘贴第三方加速脚本或泄露带凭据的代理配置。

仍有问题时，请在 [Issues](https://github.com/Liuhb1024/shiguang-archive/issues) 提供系统版本、CPU 架构、复现步骤和脱敏错误。安全漏洞或凭据问题请按 [安全说明](SECURITY.md) 处理，不要直接公开细节。

## 开发与验证

技术栈：Tauri 2 / Rust / Vue 3 / TypeScript / Vite / PrimeVue / Pinia / SQLite。

```text
src/                       前端页面、组件与状态管理
src-tauri/src/             登录、请求策略、归档、媒体与本地数据处理
src-tauri/capabilities/    桌面权限配置
src-tauri/icons/shiguang/  应用图标
tests/                    前端回归测试与合成界面夹具
```

在项目根目录执行：

```sh
npm test
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo test --manifest-path src-tauri/Cargo.toml --locked
cargo check --manifest-path src-tauri/Cargo.toml --locked
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings
```

Rust 依赖已缓存时，可给 test/check/clippy 添加 `--offline`。`npm run build` 只做前端类型检查与生产构建，不生成桌面安装包。

2026-09-05 的本机公开源码检查：**11 项前端测试、60 项 Rust 测试，以及前端构建、fmt、check、clippy 通过**。测试使用合成数据；这不是 Windows 验收、全新设备安装验证或安全认证。依赖锁文件保留用于复现，不应将历史测试结果当成后续每次提交的通过证明。

2026-09-06 的 Windows 实机记录：Windows 11 x64（build 26200，WebView2 152.0.4191.66）下 `npm run tauri:build:windows` 构建通过，产出 NSIS 安装包并成功启动，应用数据目录与 SQLite 表结构正常创建。扫码登录、归档落库与媒体下载仍未验证；这不是安全认证或跨平台验收。

2026-09-13 的发布记录：Windows x64 以 `v1.0.0` 发布，提供 NSIS 安装包与免安装单文件，并附 SHA256 校验和。本版仍只完成编译、打包与启动验证；扫码登录、归档落库与媒体下载的端到端验证仍在进行。

## 接下来要做

- [x] Windows 实机编译、打包与启动。
- [x] Windows x64 安装包与免安装单文件的构建与发布（v1.0.0）。
- [ ] Windows 核心流程验收：扫码登录、归档落库与媒体下载。
- [ ] Intel Mac 与全新 macOS 环境验证。
- [ ] 完善异常状态、来源标记与可恢复内容的处理。
- [ ] 重新审计依赖，逐项处理已知风险。
- [ ] 完成 macOS 安装包，以及发行身份、签名/公证和升级回退方案。
- [ ] 建立清晰的版本支持范围与私密安全报告渠道。

这些是待办，不是已经实现的能力或交付时间承诺。内部包名与 App 标识暂时保留既有值，以免无计划地改变用户数据目录；独立产品发行前会先明确兼容与迁移方案。

## 贡献与致谢

本修改版由 [Liuhb1024](https://github.com/Liuhb1024) 维护。欢迎提交脱敏问题、合成测试和聚焦单个问题的 Pull Request；参与前请阅读 [贡献说明](CONTRIBUTING.md)。

拾光档案基于 [Gaoshu705/QzoneArchive](https://github.com/Gaoshu705/QzoneArchive)，保留原作者、版权和 [GNU GPL v3 许可证](LICENSE)。修改范围见 [CHANGES.md](CHANGES.md)。本仓库不是上游官方发行版，与腾讯、QQ、QQ 空间没有隶属、授权或合作关系。

如果这个项目帮助到了你，欢迎为本仓库和 [QzoneArchive 上游项目](https://github.com/Gaoshu705/QzoneArchive) 点个 Star。获取上游源码或安装包请使用上游官方仓库；本修改版的问题请优先反馈到本仓库。

---

<p align="center">留住能留住的记忆，也尊重那些无法还原的空白。</p>

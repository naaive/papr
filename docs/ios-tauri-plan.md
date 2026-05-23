# Papr iOS 移植方案(Tauri v2)

> 状态:**方案文档,未改代码**。本文给出把现有 Tauri v2 桌面版 Papr
> 扩展出 iOS 版本的完整技术路线、改动清单与执行步骤。

## 0. 结论先行

- Tauri v2 原生支持 iOS,**复用同一套 React 前端 + Rust 后端**,无需重写。
- **但 iOS 的 `init` / `dev` / `build` 必须在 macOS + Xcode 上执行**(底层调用
  `xcodebuild`、Apple SDK、`cargo-mobile2`)。当前 CI/开发环境是 Linux,
  只能完成「代码与配置改造」,生成 Xcode 工程和出包要在 Mac 上做。
- 主要工作量集中在两块:
  1. **后端按平台条件编译**——托盘 / 开机自启 / 窗口状态 / 自动更新等桌面
     专属插件要从 mobile 构建里隔离掉,否则编译失败。
  2. **前端移动端布局**——当前是固定三栏桌面网格,手机上完全不可用,需要
     一套**栈式导航**(订阅 → 文章列表 → 阅读页,带返回)。

---

## 1. 现状盘点(哪些是桌面专属)

### 1.1 Rust 依赖(`src-tauri/Cargo.toml`)

| 依赖 / feature | iOS 可用性 | 处理 |
| --- | --- | --- |
| `tauri` 的 `tray-icon` feature | ❌ mobile 无托盘 | 移到 `cfg(desktop)` |
| `tauri-plugin-autostart` | ❌ 桌面专属 | `cfg(desktop)` |
| `tauri-plugin-window-state` | ❌ 桌面专属 | `cfg(desktop)` |
| `tauri-plugin-updater` + `-process` | ❌ iOS 走 App Store 更新 | 已 `cfg(desktop)`,保持 |
| `tauri-plugin-deep-link` | ✅ 支持 mobile | 需补 iOS URL scheme 配置 |
| `tauri-plugin-notification` | ✅ | 保留,iOS 需运行时授权 |
| `tauri-plugin-os` / `-opener` | ✅ | 保留 |
| `rusqlite`(bundled) | ✅ 随源码编译进包 | 保留 |
| `reqwest` / `feed-rs` / `scraper` / `ammonia` 等 | ✅ 纯 Rust + rustls | 保留 |
| `imap`(newsletter)/ `lettre`(Send to Kindle) | ⚠️ 能编译(rustls),但移动端属边缘功能 | 保留编译,UI 隐藏 |

### 1.2 后端入口(`src-tauri/src/lib.rs`)

- `lib.rs:57` 已有 `#[cfg_attr(mobile, tauri::mobile_entry_point)]` —— 入口
  钩子就绪。
- `lib.rs:63-68` 无条件挂载 `window_state` + `autostart` 插件 → **mobile 会编译失败**。
- `lib.rs:72-77` `updater`/`process` 已 `cfg(desktop)` 隔离 → OK。
- `lib.rs:137-143` 启动主题背景色用 `get_webview_window("main")` → mobile 无
  命名窗口,需 `cfg(desktop)` 或改取 `webview_windows().values().next()`。
- `lib.rs:146` `tray::build(...)` → 必须 `cfg(desktop)`。
- `lib.rs:117-127` deep-link 注册中 `register("papr")` 仅 Windows/Linux,iOS
  靠 Info.plist 的 URL scheme,设置侧无需改,但配置侧要补(见 §3.2)。

### 1.3 托盘 / 通知 / 调度

- `src-tauri/src/tray.rs` 整个模块用了 `tray::TrayIconBuilder` → **整模块
  `cfg(desktop)`**,并把 `scheduler.rs:269`、`commands.rs` 里对 `tray::refresh`
  的调用一并隔离。
- `src-tauri/src/notify.rs:22` `win.set_badge_count(...)` —— iOS 上 app 角标走
  通知中心(`UNUserNotificationCenter.setBadgeCount`),Tauri 的
  `set_badge_count` 在 iOS 上目前是 no-op,需接受「iOS 暂无角标」或后续用
  插件桥接。`notify_new_articles` 里的 `is_focused()` 判断在 iOS 上语义不同
  (前后台),可保留但效果近似。
- `scheduler.rs:388-412` 背景刷新循环**依赖进程常驻**(注释明确写了 macOS 退出
  后不执行)。iOS 会在切后台数秒内挂起进程,**后台定时刷新不可行**。iOS 版策略:
  **仅前台 / 启动时刷新**;真要后台刷新需接 `BGAppRefreshTask`(Tauri 未直接
  暴露,需自写 Swift 插件,列为后续可选)。

### 1.4 前端(`src/`)

- **布局**:`styles.css:135` `grid-template-columns: var(--col-sidebar)
  var(--col-list) 1fr;` —— 固定三栏,除 `prefers-reduced-motion` 外**无任何
  响应式**。手机必须重做(见 §4)。
- **平台误判**:`src/lib/platform.ts:9-11` 把 `iPhone|iPad|iPod` 也判成
  `isMac=true`,会在 iOS 错误渲染 macOS 红绿灯标题栏(`Sidebar.tsx:467`)。
  需新增独立的 `isIOS`,且红绿灯标题栏只在真·macOS 桌面渲染。
- **自动更新**:`src/lib/updater.ts` + `App.tsx:211` 启动自检 → iOS 上要跳过
  (插件不存在,调用会抛错)。
- **开机自启 UI**:`SettingsDialog.tsx:5` import `plugin-autostart` →
  iOS 上隐藏「开机自启」设置项,且**不要在 iOS 构建里 import 该插件**(运行时
  会因命令缺失报错;需用动态 import 或平台分支)。
- **桌面专属设置项**:托盘相关、检查更新、开机自启在 iOS 设置面板隐藏。

---

## 2. 后端改造清单

### 2.1 `Cargo.toml`:按平台拆依赖

把桌面专属插件移入平台块,`tauri` 的 `tray-icon` feature 也从默认拆出:

```toml
# 通用(所有平台)
[dependencies]
tauri = { version = "2", features = ["image-png"] }   # 去掉 tray-icon
tauri-plugin-opener = "2"
tauri-plugin-notification = "2"
tauri-plugin-os = "2"
tauri-plugin-deep-link = "2"
# ...(rusqlite / reqwest / feed-rs 等通用依赖保持不变)

# 仅桌面
[target.'cfg(not(any(target_os = "ios", target_os = "android")))'.dependencies]
tauri = { version = "2", features = ["tray-icon", "image-png"] }
tauri-plugin-autostart = "2"
tauri-plugin-window-state = "2"
tauri-plugin-updater = "2"
tauri-plugin-process = "2"
```

> 注:`tauri` 重复声明 + features 合并是 Cargo 允许的写法;也可用 workspace
> feature 统一管理。`imap`/`lettre` 暂保留在通用块,先确保能交叉编译。

### 2.2 `lib.rs`:入口条件编译

```rust
let mut builder = tauri::Builder::default()
    .plugin(tauri_plugin_opener::init())
    .plugin(tauri_plugin_notification::init())
    .plugin(tauri_plugin_os::init())
    .plugin(tauri_plugin_deep_link::init());

#[cfg(desktop)]
{
    builder = builder
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init());
}
```

`setup()` 内:
- 启动主题背景色块(`lib.rs:137`)整段包 `#[cfg(desktop)]`,或把
  `get_webview_window("main")` 换成 mobile 也成立的取窗口方式。
- `tray::build(...)`(`lib.rs:146`)包 `#[cfg(desktop)]`。
- dock badge 初始化(`lib.rs:152-155`)在 iOS 是 no-op,可保留但不指望生效。

`mod tray;`(`lib.rs:19`)改为 `#[cfg(desktop)] mod tray;`。

### 2.3 tray / scheduler 调用点

- `tray.rs` 文件顶部无需改(整模块只在 desktop 编译);所有 `tray::refresh(app)`
  调用点(`scheduler.rs:269`、`commands.rs` 的 `refresh_tray` 等)用
  `#[cfg(desktop)]` 包裹,或提供一个 `#[cfg(not(desktop))]` 的空 stub
  `pub async fn refresh(_: &AppHandle) {}`,避免散落大量 `cfg`。
- `commands::refresh_tray`(`lib.rs:225` 注册)在 mobile 下变空实现即可。

### 2.4 调度策略

- iOS 保留 `refresh_all`(手动 / 启动 / 前台触发),**移除常驻定时循环依赖**:
  在 `lib.rs` 的 setup 里 `spawn_scheduler` 仅 `#[cfg(desktop)]`;iOS 改为
  前端在 app 进入前台时调用 `refresh_feeds` 命令(监听 visibilitychange)。
- 后台刷新(可选,后续):自写 Swift 插件注册 `BGAppRefreshTask`,在回调里
  调 Rust 的 `refresh_all`。**列为 P2,不在首版**。

---

## 3. 配置改造

### 3.1 `tauri.conf.json`:窗口配置与平台覆盖

当前 `app.windows[0]`(`tauri.conf.json:13-25`)的 `titleBarStyle: Overlay`、
`trafficLightPosition`、`width/height/minWidth` 都是桌面语义。Tauri 在 iOS 上
会忽略多数窗口字段(iOS 单窗口全屏),但建议**用平台特定配置文件**隔离,保持
桌面体验不变:

- 保留主 `tauri.conf.json` 的桌面窗口配置;
- 新增 `src-tauri/tauri.ios.conf.json`,用 `tauri ios build --config` 合并,
  iOS 段去掉桌面窗口装饰、设最低系统版本等。

```jsonc
// src-tauri/tauri.ios.conf.json(示意)
{
  "app": { "windows": [{ "label": "main", "title": "Papr" }] },
  "bundle": {
    "iOS": { "minimumSystemVersion": "14.0" }
  }
}
```

### 3.2 deep-link mobile 配置

`tauri.conf.json:31-36` 当前只有 `deep-link.desktop.schemes`,补 mobile:

```jsonc
"deep-link": {
  "desktop": { "schemes": ["papr"] },
  "mobile": [{ "scheme": ["papr"] }]    // 生成 iOS Info.plist 的 URL scheme
}
```

> 若要支持 Universal Links(`https://…` 直达),需 Apple 关联域名 +
> apple-app-site-association 文件,**P2 可选**;首版用自定义 scheme 即可。

### 3.3 capabilities(`src-tauri/capabilities/default.json`)

当前 `windows: ["main"]` + 一堆桌面权限(`autostart:*`、`updater:default`、
`process:default`),iOS 上这些权限对应的命令不存在 → 需拆分:

- 新增 `capabilities/mobile.json`,`platforms: ["iOS", "android"]`,只列
  通用权限(`core:default`、`opener:*`、`notification:default`、`os:default`、
  `deep-link:default`)。
- 现有 `default.json` 加 `"platforms": ["macOS", "windows", "linux"]`,
  保留桌面权限。

### 3.4 图标

`tauri.conf.json:49-55` 现有 icon 列表面向桌面(icns/ico)。iOS 需
`AppIcon.appiconset`,由 `tauri icon path/to/source.png` 自动生成到
`gen/apple`。准备一张 ≥1024×1024 的源图(可由 `docs/logo.svg` 导出)。

---

## 4. 前端改造:移动端栈式导航(推荐方案)

### 4.1 平台检测(`src/lib/platform.ts`)

```ts
const ua = navigator.userAgent || "";
const plat = navigator.platform || "";
// 真·iOS(含 iPadOS 桌面级 UA 的回退判断)
export const isIOS =
  /iPhone|iPad|iPod/.test(plat) ||
  (/Mac/.test(plat) && "ontouchend" in document);   // iPadOS 13+ 伪装成 Mac
// 真·macOS 桌面:Mac 且非触摸
export const isMac = /Mac/.test(plat) && !isIOS;
export const isMobile = isIOS;   // 目前移动端仅 iOS;Android 加入时并入
```

`main.tsx:22` 的 `data-platform` 改为三态(`mac` / `ios` / `other`),
`Sidebar.tsx:467` 的红绿灯标题栏条件改成 `isMac`(已排除 iOS)。

### 4.2 布局:从三栏到栈

`App.tsx:428-441` 的 `app-shell > window`(三栏)在移动端改为**单视图栈**:

- 新增 UI 状态(放进 `store.ts`):`mobileView: 'feeds' | 'list' | 'reader'`。
- 桌面:三栏同屏(现状不变,`isMobile=false` 时渲染原结构)。
- 移动:同一时刻只显示一栏,导航事件推进/回退:
  - 选订阅 → `list`;选文章 → `reader`;返回手势/按钮回退。
- CSS 用媒体查询接管:

```css
@media (max-width: 768px) {
  .window { grid-template-columns: 1fr; }      /* 单列 */
  /* 由 data-mobile-view 决定显示哪一栏 */
  .window[data-mobile-view="feeds"]  .article-list,
  .window[data-mobile-view="feeds"]  .reader-pane { display: none; }
  .window[data-mobile-view="list"]   .sidebar,
  .window[data-mobile-view="list"]   .reader-pane { display: none; }
  .window[data-mobile-view="reader"] .sidebar,
  .window[data-mobile-view="reader"] .article-list { display: none; }
  :root[data-platform="ios"] .titlebar { display: none; }
}
```

- 顶部加移动专用导航条(返回箭头 + 当前层级标题),复用现有 i18n。
- **安全区**:全局加 `padding: env(safe-area-inset-*)`,处理刘海 / Home 指示条;
  `index.html` 的 viewport 加 `viewport-fit=cover`。
- 触摸目标 ≥44px;`PlayerBar`、列表行等加大点击区。
- 右滑返回(可选):监听 touch 手势驱动 `mobileView` 回退。

### 4.3 设置面板按平台裁剪(`SettingsDialog.tsx`)

- `import { ... } from "@tauri-apps/plugin-autostart"`(`:5`)改为**动态 import
  + `isMobile` 守卫**,避免 iOS 加载缺失插件。
- 隐藏分区/项:开机自启、检查更新(`checkForUpdates`)、托盘/菜单栏相关、
  Send to Kindle / Newsletter(若决定移动端不暴露)。
- `SettingsDialog` 的两栏网格(`styles.css:1136`)在窄屏也需折叠为单栏。

### 4.4 自动更新跳过(`App.tsx:207-212` / `src/lib/updater.ts`)

`checkForUpdates` 内首行加 `if (isMobile) return;`,或在 `App.tsx` 启动自检处
用 `isMobile` 守卫;`updater.ts` 对 `@tauri-apps/plugin-updater` /
`-process` 的 import 也需在 iOS 构建中不被求值(动态 import)。

---

## 5. Mac 上的执行步骤(无法在当前 Linux 环境完成)

前置:macOS + Xcode + 命令行工具,Apple 开发者账号(真机/上架需要)。

```bash
# 1. Rust iOS targets
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios

# 2. 安装依赖
pnpm install

# 3. 生成 iOS 图标(源图 ≥1024²)
pnpm tauri icon path/to/icon-1024.png

# 4. 初始化 iOS 工程(生成 src-tauri/gen/apple 的 Xcode 工程)
pnpm tauri ios init

# 5. 模拟器跑起来(热重载)
pnpm tauri ios dev

# 6. 真机 / 出包(需在 Xcode 配 Team & 签名)
pnpm tauri ios build
#   或 open src-tauri/gen/apple/papr.xcodeproj 用 Xcode 出 Archive
```

注意:`gen/apple` 默认进 `.gitignore`(`cargo-mobile2` 生成物),团队协作时
约定是否纳入版本管理。

---

## 6. 功能取舍表(桌面 vs iOS 首版)

| 功能 | 桌面 | iOS 首版 | 说明 |
| --- | --- | --- | --- |
| 订阅 / 文件夹 / 文章阅读 | ✅ | ✅ | 核心,复用 |
| 本地 SQLite | ✅ | ✅ | rusqlite bundled |
| 全文抓取 / 清洗 | ✅ | ✅ | 纯 Rust |
| AI 摘要 / 问答 / digest | ✅ | ✅ | 走 HTTP |
| FreshRSS 同步 | ✅ | ✅ | HTTP |
| OPML 导入导出 | ✅ | ⚠️ | 走 iOS 文件选择器,需验证 |
| 通知 | ✅ | ✅ | 需运行时授权 |
| 角标 badge | ✅ | ❌→后续 | iOS set_badge_count 暂 no-op |
| 后台定时刷新 | ✅ | ❌→P2 | iOS 挂起;改前台刷新 |
| 菜单栏托盘 | ✅ | —— | 平台无此概念 |
| 开机自启 | ✅ | —— | 平台无此概念 |
| 自动更新 | ✅ | —— | iOS 走 App Store |
| Newsletter(IMAP)| ✅ | ⚠️ | 能编译,首版建议隐藏 |
| Send to Kindle(SMTP)| ✅ | ⚠️ | 同上 |
| 深链 `papr://` | ✅ | ✅ | 需 Info.plist scheme |
| 音频播放器 | ✅ | ✅ | WKWebView 媒体,需验证后台音频 |

---

## 7. 风险与未决项

1. **无 Mac 无法验证**:本仓库改造后只能保证桌面 `cargo check` / `pnpm build`
   /`vitest` 通过;iOS 实际编译、链接(`imap`/`lettre` 在 iOS target 上的
   交叉编译)、运行行为必须在 Mac 上确认。`imap` 是 alpha 版,iOS 链接风险最高。
2. **后台刷新缺失**:iOS 首版只能前台刷新,与桌面「常驻自动刷新」体验有差。
3. **WKWebView 差异**:CSP(`tauri.conf.json:28` 现为 `null`)、混合内容、
   `marked` 渲染的远程图片加载、音频自动播放策略,需真机回归。
4. **存储位置**:`app_data_dir()`(`lib.rs:82`)在 iOS 落到沙盒
   `Library/Application Support`,iCloud 备份策略需确认(大库可能不希望进备份)。
5. **上架合规**:隐私清单(PrivacyInfo)、网络用途说明、最低 iOS 版本。

---

## 8. 分阶段路线

- **P0 — 可编译**:Cargo 平台拆依赖、`lib.rs`/tray/scheduler 的 `cfg`、
  capabilities/conf 拆分。产物:Mac 上 `tauri ios init` + 模拟器能起。
- **P1 — 可用**:前端 `isIOS` 检测、栈式导航 + 安全区、设置面板裁剪、
  跳过 updater/autostart、前台刷新。产物:手机上能正常读 RSS。
- **P2 — 打磨**:右滑返回手势、角标(Swift 插件)、后台刷新
  (`BGAppRefreshTask`)、OPML 文件选择器适配、Universal Links、上架素材。

---

## 9. 验收标准

- 桌面三平台行为零回归(`cargo check` + `pnpm build` + `pnpm test` 全绿)。
- Mac 上 `tauri ios init` 成功,模拟器 `tauri ios dev` 能启动并读取本地库。
- iPhone 窄屏:订阅→列表→阅读三级栈式导航顺畅,安全区无遮挡,无 macOS
  红绿灯误渲染。
- iOS 上不出现因桌面插件缺失导致的运行时报错(updater/autostart/tray)。

# GPUI Fast 引擎（fork 分支）

本分支的 GPUI 引擎来自 **`moonmicah/gpui-fast` 的 `nyaterm` 分支**，由根
`Cargo.toml` 的 `[patch.crates-io]` 用 `git + rev` 钉住；仓库内不再有
`vendor/` 副本。

## 为什么是 fork 分支而不是 vendor

上游 `longbridge/gpui-fast` 不接受新的公共 API（`script/check-upstream` 会
拦截），而 nyaterm 需要的原生叠层、虚拟文件拖出、隐藏光标、指针取消都必须改
引擎源码。把补丁放在 fork 分支上，升级就从「重放补丁」变成「`git merge` 上游
tag」——冲突只落在补丁本身。

## 分支结构

`nyaterm` 分支基于上游 main，叠加以下提交（2026-11-16 重新分组为五个主题提交，
后续按需追加）：

| 提交 | 内容 |
| --- | --- |
| `release: use the published registry manifests...` | 九个发布 crate 改用 crates.io 发布形态的 manifest（包名 `gpui` → `gpui-fast`、`gpui_macos` → `gpui-fast-macos` 等，`lib.name` 保持原名，消费方 `use gpui::…` 不变）；同一提交里把站内依赖的 `path` 补回、给 `gpui-fast-apple` 打开 `runtime_shaders`，并把 `gpui_wgpu` 源码适配到 registry wgpu 30.0.1（去掉 `default_queue`、`has_srgb_suffix` → `is_srgb`） |
| `fast: answer CursorStyle::Hidden...` | 隐藏光标：`CursorStyle::Hidden` + macOS/Windows 透明原生光标；同一提交向 `script/check-upstream` 注册这条例外 |
| `nyaterm: carry the local engine patches...` | 其余四组补丁（见下） |
| `macos: request native damage when waking an idle Metal window (#137)` | backport 上游 PR #137 的两半（空闲帧的 native damage + frame-source 附着检查），新增 `fast/frame_demand.rs` |
| `docs: document the nyaterm branch and the rebases it took` | 分支说明、验证方式与历次 rebase 记录 |
| `fast: keep the newer upstream's planar-video API compiling` | 补上 `VideoTexture` / `VideoColorMatrix` 与五个 `Window` 视频方法的降级实现，见下节 |

`gpui-fast` 仓内的 `NYATERM.md` 也记录了同样的内容。

## 临时 backport 的上游 PR

引入时 #137 与 #138 在上游仍是 **open**，两者修的都是 NyaTerm 会踩到的行为，
因此先挂在分支上：

- **#137**（`hesong12:fix/macos-idle-redraw`，仍在分支上）：macOS 窗口空闲后
  `CAMetalDisplayLink` 回调停送，GPUI 仍在唤脏窗口，但画面要等别的 compositing
  才出来（后台任务完成、远程桌面帧更新都可能卡住）。现在共享 waker 顺带用
  `displayLayer` 标记 `NSView` 重绘。
- **#138**（`ilianovi3:fix/scroll-layer-ineligible-backoff`，**已上游化**）：
  120 Hz 下滚动层内容持续不 eligible 会逐帧重试，退避间隔逐次拉长。上游
  `e1ed545`（tag `v0.1.15`）合入的 patch-id 与本地 cherry-pick 完全相同，
  2026-11-13 rebase 时 git 自动 drop 了重复提交。

**上游一旦合并**，用 `git rebase --onto <上游合并点> <最后一条 backport 提交>
nyaterm` 把对应提交摘掉，不要长期重复携带；目前只剩 #137 待摘。#139（macOS
菜单死锁）与 #140（Windows 11 acrylic 背景可读性）已在上游 base 里，不占分支
提交。两处手工解的冲突：`fast/mod.rs` 的模块声明并列（`cursor` +
`frame_demand` + 上游 `menu`）、`script/upstream-allowlist` 中 `window.rs`
那行描述合并后取 #137 的数字（`added=48 removed=30`）。

## 与 gpui-kit 的耦合

`gpui-kit` 的 `nyaterm` 分支同时携带两套后端：默认走 `nyakang/zed` 的
`gpui`，打开 `gpui-fast` feature 后改走 `gpui-fast`。本仓库的
`gpui-base` / `gpui-kit` 依赖必须固定到该分支的 rev 并打开这个 feature，
否则 `gpui-kit` 会按 zed 版本的类型编译，和 fork 引擎的类型对不上。

2026-11-16 起这两个依赖指向 **`moonmicah/gpui-kit`**（fork 自
`nyakang/gpui-kit`）的 `nyaterm` 分支，不再直接钉 nyakang 的仓。原因是引擎
那次跨 zed 同步（`#148`）之后，`gpui-kit` 里为旧引擎准备的兼容层反而成了
coherence 错误源，必须在 `gpui-kit` 侧改；`nyakang/gpui-kit` 没有本仓库的
写权限，改不了就直接钉自己的 fork：

```toml
gpui-base = { git = "https://github.com/moonmicah/gpui-kit", rev = "af2cc417…", features = ["gpui-fast"] }
gpui-kit  = { git = "https://github.com/moonmicah/gpui-kit", rev = "af2cc417…", features = ["gpui-fast"] }
```

fork 的 `nyaterm` 分支是「nyakang 的 `7119d819` + 一个适配提交」，两者一一
对应；fork 上被覆盖掉的老线留在 `nyaterm-legacy-2026-11-16`。等
`nyakang/gpui-kit` 收下同样的改动，把这两行改回 nyakang 的仓即可，别的不用动。

**注意跟线方向**：nyakang/gpui-kit 的 `main`（0.10.0 线）与 `nyaterm` 线
是分叉的——main 缺少 nyaterm 线独有的定制 API（`PopupMenuAppearance`、
`open_tracked_dialog`、`capture_right_click` 等，两线相差 49+12 条提交）。
2026-11-17 曾误把 fork rebase 到 main，应用侧 `nyaterm-ui` 立刻缺符号，
随即回退到 nyaterm 线。上游有新提交时先看它落在哪条线：只有 nyaterm 线
（`upstream/nyaterm`）前进才跟进；main 线的新特性要等 nyakang 合入
nyaterm 线。同日 fork 引擎分支 bump 到 0.1.16（`2fe3d518`），与 gpui-kit
nyaterm 线的 `gpui-fast ^0.1.14` semver 兼容，nyaterm 侧版本要求同步改
`="0.1.16"`。

## 与 main 的 API 差异

main 追随 `nyakang/zed` 的 gpui。2026-11-16 上游 `#148`（Sync with zed
78f5d74）之后，fork 与 main 的这层差异基本抹平：

- **`Element` 与 `IntoElement`**：fork 现在同样有
  `impl<T: Element> IntoElement for T` blanket impl，应用里不再写显式
  `impl IntoElement`（此前为旧引擎补回的四处——`SelectionRetention`、
  `NyaTerminalElement`、`TabMotionElement`、`TabStripViewportElement`——已随
  本次 rebase 再次删除，留着会撞 coherence 错误）。
- **平面视频纹理**：main 的 Android 投射用 `gpui::VideoTexture` /
  `VideoColorMatrix` 和 `Window::{supports,create,update,paint,remove}_video_texture`
  （I420 直传，省约 1/3 带宽）。fork 提供同名降级实现：
  `supports_video_textures()` 返回 `false`，应用因此向设备请求 BGRA、走
  `DynamicTexture`；`create` / `update` / `paint` 被调用时返回错误，`remove` 空
  实现。要真正启用 I420，得在 Metal / DirectX / wgpu 三套 atlas 里各做一个平面
  采样器。

## 各补丁组

- **核心（gpui-fast）**：`ExternalDragPayload::{VirtualFiles, VirtualFileTree,
  PromisedFiles}`、`file_promise.rs` / `virtual_file.rs`、`PointerCancelled`、
  `set_cursor_hidden` 跨平台入口、原生子窗口叠层
  （`elements/native_overlay.rs` + `PaintOperation::Primitive(primitive,
  on_overlay)`）、a11y checkpoint（`Window::transact` 重试回滚）、
  `App::has_internal_drag`、`VisualTestContext::activate_accessibility`。
- **Windows**：OLE 虚拟文件流（`external_drag.rs`）、原生叠层
  （`native_overlay.rs`，双 `DirectComposition` target）、指针取消、隐藏光标。
- **macOS**：Finder promise 拖出（`file_promise_drag.rs`，每个顶层项一个
  `NSFilePromiseProvider`，worker 线程写入，completion 回主线程，基于
  `objc2`）、透明光标、失焦时投递 `PointerCancelled`。
- **Linux**：Wayland 拖出按载荷变体匹配，虚拟文件流与 promise 返回 `false`
  （URI list 只能承载真实路径）；原生实现只为适配扩宽的载荷枚举打编译补丁，
  不新增远程拖出行为。

平台能力开关：promise 拖出仅 macOS 开启，虚拟文件流仅 Windows 开启。

## 已回到上游的能力

- **动态纹理（远程桌面帧）**：上游 0.1.14（PR #86）提供
  `Window::{create,update,paint,remove}_dynamic_texture` 与
  `DynamicTexturePlatform`，应用层调用点无需改动。
- **Windows 动态纹理后端**：上游 #131 已合并我们提交的实现
  （`gpui_windows/src/fast/dynamic_textures.rs`），fork 分支不再重复携带。
- **滚动层退避**：上游 #138 / `v0.1.15`（`e1ed545`）合入，与本地 backport
  的 patch-id 一致，rebase 时自动丢弃；`fast/layers/policy.rs` 直接用上游版本。
- **贴边 hitbox 的 hover 残留**：上游 #144（`cd2e68f`）新增 `fast/pointer.rs`，
  记住指针退出的位置直到下一个指针事件。rebase 时在 `dispatch_mouse_event`
  解过一处冲突：原生叠层的 outside-down 块正好压在它重写的那行上，解法是把叠层
  块夹在上游的 `read_pointer` 与 `pointer::hit_test` 之间，按下仍先给叠层看到。
- **web 后端的画布尺寸回退**：上游 #143（`719b645`），NyaTerm 不使用 web 后端。
- **窗口回调重入**：上游 #146（`31a0d8f`）新增 `fast/misc/reentrant_window.rs`，
  Windows 窗口过程在 `App` 被借用期间重入时，把 resize / move / display-change
  合并成一次延后回调、input 按序排队、hit test 先答 `None`，不再刷
  `RefCell already borrowed`、`on_display_changed` 也不再 panic。`window.rs`
  的 allowlist 计数随之升到 `added=110`。
- **zed 78f5d74 同步**：上游 #148 把 base 推进到 zed `78f5d74`，并随之删掉全部
  手写 `IntoElement` impl（blanket impl 接管），`reset_cursor_rects` 重构为
  `native_cursor(style)`（`CursorStyle::Hidden` 分支并入该 match）。本次
  rebase 的机械冲突：release manifest 补一行 `serde_json`、Windows
  `events.rs` 的 `handle_move_msg` 改为 `self: &Rc<Self>`。
- **wgpu GLES 3.0 路径与彩色 emoji 回退**：上游 #141（`454c29e`）。发布形态的
  `gpui_wgpu` 源码按 `script/prepare-release.ts` 的规则适配到 registry wgpu
  30.0.1（`default_queue` 行删除、`has_srgb_suffix` → `is_srgb`），该适配在
  `script/upstream-allowlist` 以 `any` 登记；`cargo check --workspace` 首次把
  wgpu crate 编进来时发现此前从未适配，已并入 release-manifest 提交。
- **字体平滑关闭生效**：上游 #142（`0d153dd`）。
- **重复状态与偏移读取的哈希去重**：上游 #153（`0f5bf7f`，2026-11-17 rebase
  携入）。`unique_states` 与 `OffsetReads::of` 原来逐项回看去重，多 retained
  子树的窗口每帧付出平方级代价（profile 显示主线程 CPU 65–75% 耗在这两处），
  现改用 `FxHashSet` / `FxHashMap` 按状态 id 哈希查重，输出不变。终端正是
  该瓶颈的典型场景。
- **IME 文本输入通知**：上游已吸收。

## 验证基线

本机（8G 内存，必须 `-j 1`）：

```bash
cargo check -p nyaterm-desktop -j 1        # 约 11 分钟
cargo test -p nyaterm-desktop
cargo fmt --all -- --check
```

fork 分支侧（在 gpui-fast 工作区）：

```bash
cargo check --workspace    # 含 gpui_wgpu：发布形态适配到 registry wgpu 30.0.1
cargo check -p gpui-fast -p gpui-fast-windows \
  --target x86_64-pc-windows-msvc \
  --features gpui-fast/windows-manifest-provided-by-host
script/check-upstream      # 违规数应与历次基线一致（当前 36）
```

`windows-manifest-provided-by-host` 用于跳过 Windows 资源编译器（macOS 主机
没有 `llvm-rc`）。Linux 侧未在本机验证：交叉编译缺少 `x86_64-linux-gnu-gcc`。

真实平台交互（Finder 多选、重名、取消、长暂停、目录恢复、大文件；Explorer
拖放目标与重名；Windows 叠层与 RDP 嵌入）无法由编译和单测替代，需人工验收。

## 升级流程

1. 在 gpui-fast 工作区：`git switch nyaterm && git merge vX.Y.Z`，冲突只出现在
   上述补丁提交里；
2. 跑完「验证基线」两段检查（fork 侧用 `cargo check --workspace`，不要只查
   两个 crate——wgpu 的发布形态适配就是这样被漏掉的）；
3. 把 nyaterm 根 `Cargo.toml` 中九条 `rev`（`[patch.crates-io]` 里实现引擎
   重命名的全部 crate）更新为新 tip；`gpui-base` / `gpui-kit` 的 rev 同步到
   `moonmicah/gpui-kit` 的 `nyaterm` 分支最新提交（见「与 gpui-kit 的耦合」）。

引擎每次跨 zed 同步，`gpui-kit` 要跟着走一遍：凡是为旧引擎补的
`#[cfg(feature = "gpui-fast")] impl IntoElement` 都要删（新引擎有 blanket
impl），`cargo check --workspace --features gpui-fast` 通过后再推。
2026-11-16 那次在 `gpui-kit` 的提交是 `9af47f94`：删掉 49 处与新 blanket
impl 冲突的显式 `impl IntoElement`（41 处带 cfg、8 处无条件），`WindowBounds::centered`
统一成三参数，story 与两个示例里 zed 的 `reqwest_client` 在 `gpui-fast` 下
cfg 掉，story 补 `extern crate gpui_kit as gpui`。默认 zed 后端本就存在
`Path` 泛型等既存不兼容，不在本次范围内。

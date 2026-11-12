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

`nyaterm` 分支基于上游 main，叠加以下提交：

| 提交 | 内容 |
| --- | --- |
| `release: use the published registry manifests...` | 九个发布 crate 改用 crates.io 发布形态的 manifest：包名 `gpui` → `gpui-fast`、`gpui_macos` → `gpui-fast-macos` 等，`lib.name` 保持原名，因此消费方 `use gpui::…` 不变 |
| `build: wire the release crates by path...` | 把站内依赖的 `path` 补回（发布形态只写 registry 版本），`gpui-fast-apple` 打开 `runtime_shaders` |
| `fast: answer CursorStyle::Hidden...` | 隐藏光标：`CursorStyle::Hidden` + macOS/Windows 透明原生光标 |
| `nyaterm: carry the local engine patches...` | 其余四组补丁（见下） |
| `docs: document the nyaterm branch` | 分支说明与验证方式 |
| `macos: damage the native view when requesting an idle frame` + `macos: preserve nonblocking frame-source attachment checks` | backport 上游 PR #137：空闲 Metal 窗口请求帧时经 `displayLayer` 主动打 native damage，新增 `fast/frame_demand.rs` |
| `docs: record ...` | 每次 rebase 到上游新 tag / 合并点时补记分支状态 |

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
cargo check -p gpui-fast -p gpui-fast-macos
cargo check -p gpui-fast -p gpui-fast-windows \
  --target x86_64-pc-windows-msvc \
  --features gpui-fast/windows-manifest-provided-by-host
```

`windows-manifest-provided-by-host` 用于跳过 Windows 资源编译器（macOS 主机
没有 `llvm-rc`）。Linux 侧未在本机验证：交叉编译缺少 `x86_64-linux-gnu-gcc`。

真实平台交互（Finder 多选、重名、取消、长暂停、目录恢复、大文件；Explorer
拖放目标与重名；Windows 叠层与 RDP 嵌入）无法由编译和单测替代，需人工验收。

## 升级流程

1. 在 gpui-fast 工作区：`git switch nyaterm && git merge vX.Y.Z`，冲突只出现在
   上述补丁提交里；
2. 跑完「验证基线」两段检查；
3. 把 nyaterm 根 `Cargo.toml` 中九条 `rev`（`[patch.crates-io]` 里实现引擎
   重命名的全部 crate）更新为新 tip；`gpui-base` / `gpui-kit` 的 rev 同步到
   `gpui-kit` 仓 `nyaterm` 分支的最新提交。

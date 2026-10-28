# GPUI Fast 引擎 vendor 说明

本分支以 `vendor/` 中的 gpui-fast 源码作为唯一 GPUI 引擎（`vendor/**` 在
`.gitattributes` 中为 `-text`，保留上游字节），根 `Cargo.toml` 的
`[patch.crates-io]` 用 path 依赖覆盖已发布 crate。上游没有这些目录；同步
上游提交时，`[patch.crates-io]` 一带是固定冲突点——保留上游对其它 fork 的
revision 更新，同时保住 gpui-fast 三行 path 条目。

各目录职责：

- `vendor/gpui-fast`：核心契约，提供 `ExternalDragPayload::PromisedFiles`、
  可变图集纹理、`PointerCancelled` 与 `set_cursor_hidden` 的跨平台入口。
- `vendor/gpui-fast-windows`：OLE 虚拟文件流、atlas 子区域更新、指针取消、
  隐藏光标。
- `vendor/gpui-fast-macos`：Finder promise 拖出（每个顶层项一个
  `NSFilePromiseProvider`，worker 线程写入，completion 回主线程）、
  `NSCursor` 隐藏、失焦时投递 `PointerCancelled`。
- `vendor/gpui-fast-apple`：Metal 图集的 `replaceRegion` 子区域更新。
- `vendor/gpui-fast-linux`：Wayland 拖出按载荷变体匹配，虚拟文件流与 promise
  返回 `false`（URI list 只能承载真实路径）；其余核心新增走 trait 默认实现，
  图集原地更新仍不可用，远程桌面帧继续回退到 CPU 缓冲。Linux 原生实现只为
  适配引擎扩宽的载荷枚举打了编译补丁，不新增远程拖出行为。

`crates/nyaterm-desktop/src/gpui_compat.rs` 只按平台声明真实能力：远程桌面帧在
Windows／macOS 走 atlas 原地更新，Linux 仍回退到 CPU 缓冲重建；promise 拖出仅
macOS 开启，虚拟文件流仅 Windows 开启。

同步上游后的验证基线：`cargo check --workspace`、`cargo test -p
nyaterm-desktop`、`cargo fmt --all -- --check`。真实平台交互（Finder 多选、
重名、取消、长暂停、目录恢复、大文件；Explorer 拖放目标与重名）无法由编译和
单测替代，需人工验收。

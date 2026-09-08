NyaTerm UI/UX 迁移审计 · 2026-09-08

基线：GPUI `b4e09782`；对照本地 `temp/nyaterm-tauri`（CHANGELOG 至 1.2.8）。用户提供的 `temp/nyatrm-tauri` 实际目录为 `temp/nyaterm-tauri`。

初始审计（2026-09-08）是源码静态审计；实施与验证记录见文末。初始审计：从页面/控件追踪状态写入、启动参数、后台运行和结果呈现，并检查字段的全仓消费者。未启动应用、未连接真实主机、未操作用户数据库，未运行 Cargo 构建或测试。以下“确认”指代码链路确认，不等于已完成 Windows 点击复现。没有修改业务代码。

结论：主要风险是“可编辑、可保存，但运行时不消费”，其次是平台集成和高级文件操作没有迁完。不能用页面存在或配置 round-trip 测试作为功能验收。

**优先处理：用户操作失效或关键流程受阻**

| ID | 问题与影响 | 当前代码证据 | 建议复测 / 修复方向 |
|---|---|---|---|
| 01 | **操作反馈未形成全局通知链路。** 例如快捷键冲突被拒绝，错误原因可能不可见。并非所有功能都无反馈：部分表单已有局部状态。 | `features/shell/state.rs:314` 的 `set_status` 仅赋值；`features/shell/keybinding_runtime/keybindings.rs:63` 冲突只写状态；`features/formatting/labels.rs:505` 将状态压成三种英文概括。`nyaterm-ui/src/root.rs:65` 挂通知层但未找到推送通知调用。终端 `features/terminal/terminal_surface/canvas.rs:820` 的详细状态受非活动面板/空会话条件限制。 | 单个活动会话下制造快捷键冲突，验证原因是否可见；建立带严重程度的通知事件，区分操作结果和运行状态。 |
| 02 | **SSH 登录后命令未执行。** 高级页能保存命令、开关和延迟，启动路径无读取。 | `features/pages/connections/editor/connection/ssh.rs:1127`；`features/connections/connection_runtime/helpers.rs:659`。全仓 `.post_login` 消费止于编辑器加载/保存，没有会话启动执行钩子。 | 保存无害命令，重新连接后检查只执行一次；执行时机、重连和外部链接确认应单独定义。 |
| 03 | **登录后延迟是空输入框。** 数值实体已创建，视图却从文本实体表读取。 | `features/connections/state/mod.rs:681` 数值字段插入 `number_fields` 后 `continue`；`:1949` 将 `PostLoginDelay` 分类为数值；`features/pages/connections/editor/connection/ssh.rs:1144` 调用 `editor_field`；`features/pages/connections/list.rs:1284` 只读 `fields.get`。 | 打开 SSH → 登录后命令，验证延迟可输入、Tab 聚焦并保存；改用数值输入包装。 |
| 04 | **Ctrl+H 退格选项保存后失效。** SSH / Telnet / Serial 均受影响。 | `features/pages/connections/editor/connection/mod.rs:522` 选项值为 `ctrl-h`；`features/connections/connection_runtime/helpers.rs:373` 等原样保存；`nyaterm-transport/src/lib.rs:912`、`:1009`、`:1057` 仅比较 `ctrl_h`。未发现启动归一化。 | 新选 Ctrl+H、保存、重连，检查发送字节为 `0x08`；兼容 `ctrl-h` / `ctrl_h` / `bs` 旧值。 |
| 05 | **隧道自动开启只是配置与徽章。** SSH 建立后没有自动打开动作。 | `features/tunnels/tunnel_runtime/tunnel_editor.rs:284` 保存 `auto_open`；`features/pages/tunnels/tunnel/row.rs:119` 画徽章；全仓其余读取未进入会话启动/隧道启动钩子。 | 关闭隧道后重连绑定 SSH，检查监听实际建立；与重复连接、重连及失败反馈一起验收。 |
| 06 | **RDP/VNC 缺代理、跳板路由接线。** 经堡垒机或企业代理才能到达的主机无法按原配置连接。 | `features/session/session_runtime/start.rs:392` 构造 RDP 配置，仅带直连主机等参数；VNC 同类分支。`nyaterm-remote-desktop/src/protocol.rs` 配置没有对应网络字段；`load_proxy_config_with_context` / `load_proxy_jump_config_with_context` 仅被 SSH 构建路径消费。RDP/VNC 表单也没有对应网络配置入口。 | 分别测试直连不可达、代理可达和跳板可达主机；网络通道应实际传递到 helper。 |
| 07 | **远程桌面 IME 提交入口未接通。** 已有发送 Unicode 文本能力，但这不等于可直接用输入法输入。 | `features/remote_desktop/view.rs:244` 起注册按键事件，未注册 GPUI `handle_input`；`features/remote_desktop/runtime.rs:699` 的 `send_remote_committed_text` 当前调用来自 `features/terminal/terminal_runtime/paste.rs:24`。`EntityInputHandler for NyaTermApp` 位于 `features/terminal/terminal_selection_runtime/helpers.rs:86`，处理终端和粘贴审阅，没有远程桌面分支。 | Windows 中文输入法分别测试 RDP/VNC 的组合、候选确认、取消、重复字符；粘贴中文单独测试，不能代替 IME 验收。 |
| 08 | **Docker 面板缺少旧版提权/PATH 回退。** 用户有 sudo 权限但不在 docker 组时，面板会判断不可用。 | `nyaterm-transport/src/docker.rs:127` 直接 `command -v docker` / `docker info`；失败即 `DOCKER_AVAILABLE 0`。该服务未实现旧版 sudo 重试或群晖 ContainerManager 路径。 | 测试普通 docker 组用户、仅 sudo 用户及非标准安装路径；避免把“需要授权”呈现成“未安装”。 |
| 09 | **本地 shell 参数按空白拆分，破坏引号。** 如 `-Command "Write-Output 'hello world'"` 会变成错误 argv。 | `features/formatting/labels.rs:520` 使用 `split_whitespace`；`features/session/session_runtime/start.rs:274` 用其结果构造本地会话。Tauri 的 `src-tauri/src/core/terminal_session/local/args.rs` 有专门参数解析器。 | 测试引号、空参数、带空格路径和反斜杠；按平台参数语义解析。 |
| 10 | **自定义连接图标未迁移。** 内建图标/自动识别不能替代导入用户图标。另有兼容性风险。 | 复核修正：Tauri 的 `custom_icons` 属于根 `SessionsConfig` 共享图标库，连接通过 `icon` 引用；初始 GPUI 缺少该共享库。Tauri 在 `src-tauri/src/config/connection.rs`、`src-tauri/src/storage/sessions.rs` 保存相关数据。 | 用含自定义图标的旧版连接做读取、编辑后保存、导出回读；避免在尚不支持显示时丢弃这些字段。未知字段通过类型模型重写的保留性需补兼容测试。 |

上表及后续表中不带 crate 前缀的 `features/...` 均相对 `crates/nyaterm-desktop/src/`；其他 `nyaterm-*/src/...` 均相对 `crates/`。

**可见设置未生效**

| ID | 设置 / 功能 | 当前证据与实际差异 | 验收要点 |
|---|---|---|---|
| 11 | 粘贴图片为路径 | `features/pages/settings/terminal/general.rs:212` 暴露 `terminal_paste_image_as_path`，但全仓没有粘贴运行时消费者；`features/terminal/terminal_runtime/paste.rs:13` 只提取 `item.text()`。 | 截图剪贴板、文件剪贴板、普通文本分别测试；明确本地路径与远端上传后路径的区别。 |
| 12 | 下载线程数 | `features/pages/settings/inputs.rs:345` 创建输入；`features/transfers/transfer_options.rs:7` 只传入上传线程数，未读取 `transfer_download_threads`。全仓其他命中为设置/存储/测试。 | 修改后用多文件下载确认并发改变；不能仅检查保存值。 |
| 13 | 硬件加速 | `features/pages/settings/terminal/general.rs:101` 有开关；`terminal_hardware_acceleration` 没有渲染路径消费者。 | GPUI 若不支持切换，应撤下可交互开关或说明当前渲染方式，保留旧数据兼容。 |
| 14 | 关键词跨折行匹配 | `features/pages/settings/terminal/keywords.rs:69` 开关控制 `across_wrapped_lines`；全仓只在配置、存储、设置中读取；`nyaterm-terminal-gpui/src/keywords.rs:269` 按折行组匹配，没有该设置参数。 | 窄终端让关键词跨软折行，开关两态分别验收。 |
| 15 | SSH strict / compatible keepalive | `features/session/session_runtime/start.rs:705` 仅区分 disabled；`nyaterm-transport/src/lib.rs:2397` 只设置 interval / max，未映射 `keepalive_mode`。 | 显式映射模式；测试忽略 keepalive 回复的设备，避免配置标签与协议行为不一致。 |
| 16 | 最小化到托盘 | `features/pages/settings/workspace/general.rs:127` 可切换；`features/terminal/terminal_runtime/sessions.rs:270` 明确注释为 no-op，两个分支均 `window.minimize_window()`。 | 开启后实际隐藏到托盘并可恢复，或在实现前不提供误导性开关。旧版托盘菜单也未迁移。 |

**迁移缺口与交互退化**

| ID | 问题 | 当前证据 | 影响 / 验收 |
|---|---|---|---|
| 17 | RDP/VNC 启动恢复仅恢复断开占位 | `features/session/startup_restore_runtime.rs:547`、`:606` 调 `insert_disconnected`，元数据 `disconnected: true`，随后提前返回；其他保存连接走 `:632` 的启动逻辑。 | 每个远程桌面标签要手动 Retry；与旧版自动建立会话不同。测试混合协议、分屏、保存密码和提示密码。 |
| 18 | 取消连接不取消后台拨号 | `features/session/session_runtime/background.rs:62` 关闭 pending；`:513` 收到已取消任务的成功结果后才关闭会话；`features/session/state/mod.rs` 只维护 cancelled 请求集合。 | 关闭慢连接标签后后台任务仍可继续；需要取消令牌到达传输/认证等待，而非只丢最终结果。 |
| 19 | 缺应用内下载、安装与重启更新流程 | `features/update/update_runtime.rs` 仅检查版本；`features/panels/update_overlay.rs` 的可更新动作是 `update-open-releases`，调用外部 URL。Tauri `src/components/dialog/app/UpdateDialog.tsx:203` 调 `downloadAndInstallUpdate`。 | 安装版也要手动下载安装；便携版手动更新可保留为明确分支。 |
| 20 | SFTP pipeline depth 未迁移 | Tauri `src/components/sessions/SshForm.tsx:1918` 可选择 `pipeline_depth`，`src-tauri/src/config/connection.rs` 有字段与归一化测试；当前 `nyaterm-core/src/models/connection.rs:376` 的 `SftpSettings` 没有该字段，crates 全仓无命中。 | 高延迟链路调优入口缺失；旧配置类型化回写时该字段不能保留，需要兼容测试及参数下传。 |
| 21 | 文件属性缺软链接目标编辑 | 当前 `features/pages/transfers/properties.rs:145` 仅识别 mode / owner / group；Tauri `src/components/dialog/file-explorer/PropertiesDialog.tsx:187` 调 `update_remote_symlink_target`。 | 能创建软链接不等于能查看/修改现有链接目标。测试相对目标、绝对目标和悬空链接。 |
| 22 | 内建编辑器缺高级编辑能力 | `features/transfers/remote_text_editor.rs:24` 只有单组 anchor/head；`:149` 和 `:885` 用 `match_indices` 做字面查找。没有替换面板、正则/大小写/整词搜索模式、折叠、多光标/矩形选择的对应实现。Tauri `src/lib/codeMirrorFileView.ts` 基于 CodeMirror 提供这些编辑能力。 | 查找、撤销、IME 和保存已经存在，不能算编辑器整体缺失；大配置文件的查找替换与批量修改明显退化。 |
| 23 | 关于页缺支持信息复制 | `features/panels/about_dialog.rs:26` 只展示应用名、版本、描述及两个链接；Tauri `src/components/dialog/app/AboutDialog.tsx` 显示 OS/架构/运行模式并可复制。 | 用户报障需手工补环境；增加脱敏的支持信息与复制反馈。 |
| 24 | 已有语言包但部分关键状态硬编码英文 | `features/terminal/terminal_surface/canvas.rs:808` 的断开状态、`:815` 的重连提示；`features/formatting/labels.rs:505`（复核修正：快捷命令菜单的英文命中来自测试断言，实际菜单已使用翻译键）。 | 中文模式仍有英文交互入口。应扫用户可见字面量，而非仅对齐 locale key 数。 |
| 25 | SSH 密钥拒绝后缺普通密码回退 | `nyaterm-transport/src/ssh_auth.rs:303` 的 `authenticate_ssh_key` 在公钥失败后仅尝试 keyboard-interactive，再报 `public-key authentication rejected`；没有进入普通 password 认证分支。Tauri `src-tauri/src/core/ssh/auth.rs` 有该回退流程。 | 对支持 password 但不支持 keyboard-interactive 的主机，用户需取消并修改连接配置。应在服务器声明支持的认证方式范围内提示切换。 |

**不能继续按旧审计判定为“整体缺失”的项目**

本地 `temp/nyaterm-parity-audit.html` 已落后于当前实现。本次检查到以下对应代码，不能直接照搬旧报告；这也不代表整个模块已通过实机验收。

| 模块 | 已存在的实现证据 |
|---|---|
| 笔记 | `features/notes/` 有功能实现。 |
| 文件预览 | `features/transfers/preview/`、`features/pages/transfers/preview/` 已有解码、打开和子窗口流程，包括 PDF 页面渲染任务。 |
| 资产工作区 | `features/assets/`、`features/layout/workspace/surface/assets.rs`。 |
| 繁中、韩语等语言 | `src/i18n/mod.rs:22` 的语言目录已列 en / zh-CN / zh-TW / ja / ko / fr；当前有相应 locale 文件。 |
| RDP Ctrl+Alt+Del | `features/panels/tab_actions_overlay/compact.rs:613` 已有标签页操作入口，调用 `send_rdp_secure_attention`。没有旧版悬浮工具栏不等于该动作不存在。 |
| RDP 证书变更警告 | `features/remote_desktop/view.rs:505` 已区分 `CertificatePromptReason::Changed` 并展示专用警告。 |
| 系统关闭确认 | `src/app_shell/mod.rs:538` 已转到 `handle_window_close_request`，不能再直接认定 Alt+F4 绕过确认。 |
| 单实例 | `nyaterm-app/src/single_instance.rs` 已存在；外部 URL scheme 注册与系统关联仍需单独验收。 |
| 动态标签标题 | `features/terminal/terminal_runtime/buffer/mod.rs:1409` 消费 `dynamic_tab_title_enabled`。 |
| AI CLI 运行时 | `features/ai/codex_runtime.rs`、`claude_code_runtime.rs` 已存在。CLI 探测、登录、账号状态与完整设置体验未在本次做端到端验收。 |
| 含笔记的快照哈希 | `nyaterm-core/src/portable_snapshot/tests.rs` 已有 notes 哈希保护测试，旧报告的“没有 notes 分支”不再成立。 |
| 文件保存内容校验 | `nyaterm-transport/src/remote_file/revision.rs` 已有 `content_sha256`，SFTP 中也已有 revision 写入路径；不能仅看旧的 mtime/size API 就判定所有编辑保存都缺哈希。 |

**实机验证边界与建议顺序**

尚未验证：Windows 中文输入法时序（尤其 AI 输入回车）、Tab/Shift+Tab 焦点顺序、弹窗取消后焦点恢复、子窗口关闭/重开、拖拽目标和指示线、多显示器 DPI、窄窗口/长列表滚动、远端断网重连、剪贴板格式与权限、RDP/VNC helper 的发布包寻址。源码没有发现问题不等于这些交互已经合格。

推荐先修 01–09 的关键链路，尤其“静默失败”和错误输入字节；随后处理 10、20 的旧字段保留，避免迁移后编辑数据造成不可逆丢失；再接通其余空设置和平台集成。每一项都应覆盖“修改 → 保存 → 重启/重连 → 实际行为 → 失败反馈”的完整流程。

初始审计未执行 Cargo 检查；2026-09-09 实施后的自动测试结果列于文末。自动测试不能代替 UI/UX 的平台实机验收。


**实施记录 · 2026-09-09（代码已推进，未完成全量实机验收）**

| 审计项 | 实施内容 | 当前边界 |
|---|---|---|
| 01 | nyaterm-ui 类型化通知；快捷键冲突、参数错误、图标/剪贴板/隧道/更新失败接入 | 未将所有历史 set_status 调用逐一转换；部分功能继续使用局部错误状态 |
| 02、03 | 登录命令多行编辑、延迟数值控件、SSH 初始化完成后定时执行；外部链接确认 | 实机登录/重连时序待验收 |
| 04、09 | 退格别名统一；shell 参数引号/空参数/Windows 路径解析下沉 core | 自动测试覆盖；跨平台实际 shell 待验收 |
| 05、08、18、25 | 自动隧道；Docker 探测提权及 stdin 密码；连接尝试取消；公钥失败密码回退 | SSH/Telnet 拨号、密码和主机密钥提示取消有接线与测试；SSH Agent 专有等待、所有系统 DNS/驱动边界仍需实测 |
| 06、07、17 | 认证 loopback route、RDP/VNC helper 接线、独立 IME 实体、串行启动恢复 | IPC 升至 7；生产代理/跳板、CJK IME 与恢复密码流程未实机验收 |
| 10、20 | 共享图标库及导入/删除/预览、遗留 data URL；SFTP pipeline；JSON 导入和存储扩展字段保留 | 原审计修正：custom_icons 是根 SessionsConfig 字段；图标列表、编辑器和工作区/标签渲染已接入，其他次要菜单还需视觉核验 |
| 11、12、21 | 剪贴板图片暂存/上传后插入；下载并发和取消；软链接目标与原子替换 | SFTP 原子替换要求服务器支持 posix-rename；真实大文件/断线/非 SFTP 后端验收待完成 |
| 13、14、15 | 无效 GPU 开关改说明；跨折行设置进入缓存/匹配；strict/compatible keepalive | russh 正式 fork 增加模式接口，默认兼容设置仍按旧配置读取 |
| 16 | tray-icon/muda 菜单、连接/同步/锁屏/更新/退出，Windows 原生隐藏与恢复、macOS hide、Linux GTK 事件线程 | Linux 隐藏 API 缺失，暂回退普通最小化并提示；不能宣称三平台最小化到托盘完全对齐 |
| 19 | 实际发布 CDN manifest、平台包选择、Minisign 校验、进度/取消、安装及重启准备 | 未执行真实安装器升级；debug/portable 禁用自更新，Homebrew/包管理器路径保留手动流程；安装/失败回退仍需三平台实机验收 |
| 22 | core 文档事务；原生查找替换、正则/大小写/整词、折叠、Ctrl+D/Alt-click 多选区和矩形选择 | 后台语法解析及折叠坐标映射有测试；大型文件性能、复杂 IME、多光标导航细节仍需实机核验 |
| 23、24 | 支持信息复制、新增六语言文案、断开提示本地化 | 原审计修正：Save as Quick Command 英文命中来自测试断言；未全量翻译所有历史诊断字面量 |

正式 fork 提交（均已推送 nyaterm 分支）：

- russh `0de037cd096b3e76efd9292d5637856ecee8af24`：keepalive 模式与无回复请求队列修复。166 个相关库测试通过；完整上游库测试的 `compression::tests::partial_flush_packets_round_trip` 在未修改的 cf257f6 基线上也失败，详见该分支 NYATERM.md。
- russh-sftp `e2caffef6d9d3c0ff97cbfedb321116f79f6d68d`：posix-rename 扩展，14 个库测试通过。
- IronRDP `b6658790647b757e1b129859ff421300b53467a1`：loopback TCP 路由与原始 TLS/CredSSP 身份分离，client check 和专用身份测试通过。此 revision 包含分支已存在的 c44dcc77/8649c7c6 gateway 合并。

自动验证（Windows，2026-09-09）：

- `cargo test --workspace --no-fail-fast -j 2`：32 个测试结果汇总，2547 passed / 0 failed / 9 ignored。Ignored 项包括真实 SFTP 环境及既有性能/平台测试，不视为通过。
- `cargo clippy --workspace --all-targets -j 2`：通过。
- `cargo fmt --all -- --check`、`git diff --check`：通过。
- `python scripts/ci/check_architecture.py`：通过，没有增加线程/架构豁免。
- 发布 manifest、原生包及包校验脚本：28 个 Python 测试通过。
- `cargo check --workspace --all-targets`：接口集成检查通过；最终编译也由 workspace tests / clippy 覆盖。

开发日志位于 `target/ui-ux-*.log`，不提交日志或用户数据。

实机限制：按 computer-use 技能初始化 Windows 自动化时 `@oai/sky` 报 Module not found，插件未提供可加载的模块；没有执行原生窗口点击截图验收。未获取 macOS/Linux 实机。未操作用户真实数据库、未实际连接生产服务器、未发布版本、未实际安装更新。

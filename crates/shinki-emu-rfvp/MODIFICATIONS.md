# RFVP 原生嵌入差异（shinki-emu-rfvp）

## 来源与提交结构

- 上游为 [`xmoezzz/rfvp`](https://github.com/xmoezzz/rfvp)，历史基线 `af557db750c7833274f22a6d82b0dc33cdea2f85`，crate 版本 0.6.0。
- `shinki` 分支 = 通用原生修复基线 `3359d8fbea3c53d6d262a9f71b6d4d79888c2368` + 一个适配提交 `feat(shinki): ShinkiEMU C ABI v3 adapter shinki-emu-rfvp`。基线含原生生命周期修复 `7e2016a0e0ef1c0b102185914cc7a8459bb2c9e1` 与原生音频 worker 所有权接入，与适配分开。
- 适配提交合并了此前的 C ABI v3 移植、原偏移翻译表、movie/BGM ASS 字幕、读档 epoch 修复、ABI pin 与 plugin ID 更名；合并前的历史保留在 [`1e947a5ccbd2c7a698bd30d48347454ef1b908a9`](https://github.com/Project-Shinkirou/rfvp/commit/1e947a5ccbd2c7a698bd30d48347454ef1b908a9) 等旧引用中，旧 tree 不代表当前构建源码。
- 适配包为 `crates/shinki-emu-rfvp`（package `shinki-emu-rfvp`，lib `shinki_emu_rfvp`），是 ShinkiEMU 唯一的 RFVP 适配。
- 适配包的 `shinki-emu-abi`（C ABI v3）与构建辅助 `shinki-emu-build` 固定 `Project-Shinkirou/ShinkiEMU@cb8d9dd22d7d0ab129085b324c94d6dc81fa19e3`；原生 `rfvp` 的可选 `roukaku-subtitle` 固定 `Project-Shinkirou/RoukakuEngine@ba28948b5dd00188fe53a24881a64dcaf0c69e2a`。
- Kira 运行层保持 0.10.8，固定 [`Project-Shinkirou/kira@d2f10db15d9c451c7b2fd33043ae7732e75bee5d`](https://github.com/Project-Shinkirou/kira/commit/d2f10db15d9c451c7b2fd33043ae7732e75bee5d)，不改 mixer 运行实现；workspace `[patch.crates-io]` 的 CPAL 0.15.3 固定 `Project-Shinkirou/cpal@65ac8a6ac1a635cc26bfc5af331295b052044908`。

## 适配内容

- `crates/shinki-emu-rfvp`：C ABI v3 函数表、typed 配置、探测、输入映射、平台服务与 CPU lease 包装。描述符声明 `display_name=RFVP`、`family_name=FVP`、`CAP_CPU_SURFACE | CAP_RENDER_EXTENT`；`SessionState.flags=0`；无 module-owner 扩展，OpenRequest 的该字段须为 null。`dynamic-plugin-export` 导出 `shinki_emu_query` 并启用 `shinki-emu-abi` 诊断桥；不启用时为静态注册，不安装第二个 logger。
- `crates/rfvp`：新增 `embedded` 与 `subtitles` feature；`app/embedded.rs`、`app/embedded/session.rs` 提供原生 owner 会话；`app/presentation.rs` 与 `render_target.rs` 提供完整帧离屏目标；`script/translation.rs` 与 `parser.rs` 的原偏移翻译表；`subtitles.rs` 及 videoplayer、BGM 时钟接入；VM worker 取消与 join、音频暂停、关闭顺序修复；`legacy_save_load_ui`、`exit_confirm_ui` 的合成调整。`flate2` 放开 `=1.0.28` 精确锁定。
- `crates/rfvp/tests/localization_resources.rs`：翻译表与字幕映射的纯逻辑测试；`README.md` 增加翻译表、字幕映射与渲染尺寸说明。

## 所有权与完整帧

App、调度、VM、资源、renderer、游戏 UI、音视频与保存继续由 RFVP 原生代码持有。旧 ProviderModule、公共 advance、HostedSession、Host PCM、替代文件系统、Windows D3D12 交付及相应旧 ABI 测试已移除。

`App::render_frame` 先绘制原场景 RenderTarget，再将场景、`legacy_save_load_ui` 和 `exit_confirm_ui` 合成到独立最终离屏目标。CPU 回读只读取这个完整目标；SaveCreate 缩略图和遮罩转场仍在原时点读取原场景目标，不借用 Host 输出 lease。

窗口外部尺寸 O 不改变 L/R，也不推进 VM。渲染尺寸由 `OpenRequest.render_extent` 指定，描述符声明范围 160×90 – 8192×8192、默认 0×0 并保持宽高比：0×0 使用 1:1 缩放；否则 `scale = min(w/Lw, h/Lh)` 截断到 0.25–4，`R = round(L × scale)`，任一轴超过 8192 返回 `RFVP_RENDER_SIZE_UNSUPPORTED`，并校验设备尺寸上限；`SurfaceInfo.raster` 报告实际 R，未声明在线重配置。原 `render_scale`、`surface` 配置项已删除。输入携带当前会话/输出代次和帧像素坐标，再复用原生窗口到逻辑/UI 映射。plugin ID 为上游仓库名 `rfvp`，保留文件 stem 游戏身份和 `script_encoding=shift_jis/gbk/utf8`（选项名 Shift-JIS（日文原版）/GBK（简体中文）/UTF-8）。配置标签、分组、说明均为中文。

原生 typed 配置增加默认空的 `translation_table` 与 `subtitle_map`，声明为 `CONFIG_FLAG_FILE_PATH | CONFIG_FLAG_GAME_RELATIVE`，均读取游戏根内的显式相对资源。`AFVPTR1` 翻译表按原 HCB 字符串偏移保存 UTF-8，核对最终 HCB 哈希与真实指令边界；未命中保留原编码，不改写 VM 地址或原脚本。字幕映射按 movie/BGM 资源关联 UTF-8 ASS 和显式字体，由原生媒体时钟驱动并绘入原场景目标；暂停、换曲、结束、seek/读档与输出尺寸变化沿原生命周期更新。该能力不补充游戏字体授权。

原生 owner 使用独立 8 MiB 栈线程；平台线程只入队输入/控制及取得完成帧。三槽不可变 CPU 池满时跳过显示发布，VM、音频和存档继续执行。Ready/Pending/Unchanged 不以像素 hash 判断；已交付 lease 可跨关闭持有，归还只销毁纯 CPU owner。

## 暂停、平台与关闭

暂停停止原生推进并暂停原 Kira mixer，恢复移动电影时钟原点，不补跑等待时长。WindowMode、F11 和指针位置/显隐经 typed 平台服务执行并反馈结果；等待期间保留最后一帧，取消独立唤醒平台服务。无窗口 Host 必须明确拒绝窗口请求。本核心不新增文字 Hook。

原 VM worker 增加指令边界取消和显式 join；原生读档成功递增 context epoch 并向 Host invalidation 发布，取消旧平台请求和未完成回读；shutdown 设置取消标志之前记录原取消状态，避免清理过程吞掉真实请求失败。新 App 清空上一会话的 legacy 配置、文字历史、角色表和 UI 请求。关闭停止回读、VM、电影、原设备 watcher 和全部 streaming decoder，再调用原 RFVG 保存；即使关闭出错也完成 worker 回收。库不卸载，下一次启动重建会话资源。未改原生 VM、renderer、音视频或存档格式。

游戏自身结束会话有两条原生路径：脚本 `ExitMode(3)` 指定的上下文执行结束；或在 `ExitMode(1)`（默认）下确认退出，来源包括 `ExitDialog` 与内置退出确认框。原 App 在这两种情况下让 `try_host_step` 返回 true。owner 线程收到后立即发布 `SESSION_FINISHED`（flags 为 0），不再发起本帧的窗口模式或指针请求，随后沿正常关闭顺序停止 VM、媒体与音频并保存 RFVG。单个脚本线程返回不算游戏退出。Finished 之后 Host 仍在途的输入和暂停请求直接丢弃并返回成功，`close` 照常回收 worker。嵌入构建中遇到未知 opcode 时报告 VM 失败，不再调用 `process::exit` 结束宿主进程。

Kira 的 DecoderOwner 独立于播放 handle；handle Drop 语义不变，Sound Drop 只发取消，不在设备 callback join。CPAL watcher 的启动握手、失败路径、唤醒与 join 在同版本 fork 修复；不替换 mixer/decoder/device。

## 测试与验收

`shinki-emu-rfvp` 的 `config.rs`、`input.rs` 纯逻辑单元测试与 `rfvp` 的 `localization_resources` 测试保留；`lib.rs` 用合成 endpoint 验证游戏自身退出经 C ABI 报告 `SESSION_FINISHED`、其后输入与暂停不报错且 `close` 正常回收；`rfvp` 的 `vm_runner` 测试验证 `ExitMode(3)` 只在其上下文返回后才结束游戏。原生上游普通测试不变。没有自动输入、截图、定时关闭、Headless、性能采样入口或动态 DLL 验收驱动；实际游戏验收由用户使用 Release 产物手动完成。

## Release 构建与交付

在 ShinkiEMU 根目录构建：

```sh
cargo xtask build --target x86_64-pc-windows-msvc --core rfvp --release
```

适配 crate 的 `build.rs` 生成 `OUT_DIR/core-build.json`，记录 ABI、目标、profile 和 fork 内的许可文件；许可变化会触发重新生成。xtask 从 Cargo 消息读取该清单和真实 cdylib 路径，不要求预构建库。

独立构建使用 `cargo build --locked --release -p shinki-emu-rfvp --features dynamic-plugin-export`，并设置 `ROUKAKU_SUBTITLE_VCPKG_ROOT`，指向安装了 overlay triplet `x64-windows-static-md-dynbidi` 的独立 vcpkg 树。产物为所选 Cargo target 目录内的 `shinki_emu_rfvp.dll`，导出 `shinki_emu_query`。

运行需要兼容 C ABI v3 的 Release Manager、x64 Windows 系统组件、VC++ 运行库及可用 GPU/音频驱动。静态导入包括 `VCRUNTIME140.dll`、UCRT、`mfplat.dll`（Media Foundation）、`d3dcompiler_47.dll`，以及随核心目录分发的 `fribidi-0.dll`；原生媒体实现没有 FFmpeg DLL 依赖。Vulkan 需驱动提供 loader；帧只经 CPU lease 交付。

原生保存写入游戏目录下的 `save/`。字体、媒体与字幕依赖来源见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

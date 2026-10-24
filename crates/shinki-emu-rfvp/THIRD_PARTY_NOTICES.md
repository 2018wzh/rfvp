# 第三方来源与许可（shinki-emu-rfvp）

## RFVP 覆盖源码

本仓库为 [`xmoezzz/rfvp`](https://github.com/xmoezzz/rfvp) 的完整 fork，发布分支为 [`Project-Shinkirou/rfvp@shinki`](https://github.com/Project-Shinkirou/rfvp/tree/shinki)。通用基线为 `3359d8fbea3c53d6d262a9f71b6d4d79888c2368`，上游历史基线为 `af557db750c7833274f22a6d82b0dc33cdea2f85`，crate 版本 0.6.0。变更分类见 [MODIFICATIONS.md](MODIFICATIONS.md)。

完整 MPL-2.0 许可位于仓库根 [`LICENSE`](../../LICENSE)。发行 `shinki_emu_rfvp.dll` 时须保留与二进制对应的覆盖源码（或有效的 source offer）、该许可与本目录的变更记录，不以旧已发布基线替代当前二进制实际使用的源码。

适配包只提供 C ABI v3 注册、原生 owner、CPU lease 与渲染尺寸换算，复用原 App、VM、GPU、视频、设备音频和 V1/RFVS 保存，没有 Host advance 或 PCM sink。

## ShinkiEMU 与 RoukakuEngine 组件

`shinki-emu-abi`（C ABI v3）与 host-side `shinki-emu-build` 来自 [`Project-Shinkirou/ShinkiEMU@cb8d9dd22d7d0ab129085b324c94d6dc81fa19e3`](https://github.com/Project-Shinkirou/ShinkiEMU/commit/cb8d9dd22d7d0ab129085b324c94d6dc81fa19e3)，采用 MIT OR Apache-2.0。`roukaku-subtitle` 来自 [`Project-Shinkirou/RoukakuEngine@341d3dc8d5ed79170cc7b3b197f8f65093356a9f`](https://github.com/Project-Shinkirou/RoukakuEngine/commit/341d3dc8d5ed79170cc7b3b197f8f65093356a9f)。manifest 与 `Cargo.lock` 固定这些 git revision。

## ASS 字幕依赖

`roukaku-subtitle` 使用 [libass](https://github.com/libass/libass)（ISC），原生依赖按 overlay triplet `x64-windows-static-md-dynbidi`（RoukakuEngine `Engine/Source/Runtime/roukaku-subtitle/triplets/`）安装：libass、FreeType（选择 FreeType License）、HarfBuzz（Old MIT）、Brotli（MIT）、bzip2、libpng、zlib 静态链接进核心，FriBidi（LGPL-2.1）以 `fribidi-0.dll` 动态链接并随核心目录分发，用户可替换为自行构建的兼容 DLL。发行时须逐字保留实际 vcpkg 安装 `share/<port>/copyright` 的全部条款，附 LGPL-2.1 文本及 FriBidi 对应源码；静态依赖即使没有单独 DLL 也须保留条款。本产品使用 FreeType Project（<https://freetype.org/>）的软件。字幕字体由游戏侧显式资源提供，须保留各自许可。

## Kira 与 CPAL

Kira 0.10.8 来自 [`Project-Shinkirou/kira@d2f10db15d9c451c7b2fd33043ae7732e75bee5d`](https://github.com/Project-Shinkirou/kira/commit/d2f10db15d9c451c7b2fd33043ae7732e75bee5d)，CPAL 0.15.3 经 workspace `[patch.crates-io]` 来自 `Project-Shinkirou/cpal@65ac8a6ac1a635cc26bfc5af331295b052044908`，均按各自上游许可发布，须保留其许可文本。

## 系统字体

原生 App 路径沿用上游 `FontEnumerator::new`，编译期嵌入 `MSGOTHIC.TTF`、`MSMINCHO.TTF`、`MS-PGothic.ttf`、`MS-PMincho-2.ttf`。这四个字体文件由上游 [`xmoezzz/rfvp@f60d8660`](https://github.com/xmoezzz/rfvp/commit/f60d8660d800fe16a4da485ef50e010e7b1ec2e5) 加入，存在于上游基线 `af557db750c7833274f22a6d82b0dc33cdea2f85`，核心二进制按上游原样包含并发布。

## WMV 解码来源

WMV 由本仓库原生 `crates/na_wmv_player` 及原生 Movie 路径解码、合成和播放音轨。上游 decoder 清单没有独立 `license` 字段，随完整 fork 保留上游许可与来源记录；本记录不将 FFmpeg 算法名称视为复制 FFmpeg 源码或额外许可授权，也不据此假定 LGPL/GPL/MIT。发布前仍需核实实际覆盖范围。

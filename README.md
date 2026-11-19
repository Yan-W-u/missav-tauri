# MissAV 下载器 · Tauri 2 版

[![Release](https://img.shields.io/github/v/release/Yan-W-u/missav-tauri?label=%E4%B8%8B%E8%BD%BD)](https://github.com/Yan-W-u/missav-tauri/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Android-lightgrey)]()

一个用 **Rust + Tauri 2** 写的视频下载与在线观看客户端，带 Windows 桌面版和 Android 版。

界面是原生的 HTML/CSS/JS，后端是 Rust，打包体积小、启动快、不需要装 Python 或 Node 运行时。

> 仅用于个人学习和私有部署。请自行确认所在地法律与目标站点的服务条款。

---

## 下载

**[⬇ 下载 Windows 版](https://github.com/Yan-W-u/missav-tauri/releases/latest)**

解压后双击 `MissAV下载器.exe`。同目录下必须保留 `ui/` 文件夹，否则界面白屏。

| 平台 | 文件 | 大小 |
| --- | --- | --- |
| Windows | `MissAV-Downloader-Windows.zip` | 2.6 MB |
| Android | 见 Releases 里的 `*-arm64.apk` | 9.1 MB |

Android 版目前只支持 CDN 直连下载，站点搜索和详情不可用（原因见下方「已知限制」）。

---

## 它能干什么

- **搜索**：输入番号（`SSIS-001`）或关键词，卡式分页浏览
- **分区浏览**：新作上市、最近更新、无码流出、中文字幕
- **标签筛选**：单个标签浏览；支持多选（并集 / 交集两种语义）
- **详情查看**：封面、时长、发行日期、演员、分类、简介，并自动标注**有码 / 无码**
- **在线观看**：独立的播放页，主界面和播放互不阻塞，清晰度可切换
- **下载**：HLS 分片并发下载 → 解密 → 拼接 → ffmpeg 无损转封装为 mp4
- **断点续传**：按「番号 + 清晰度」缓存分片，中断后重跑只补缺的片
- **导入外部链接**：直接粘贴任意页面地址或 `.m3u8`，绕过站点本身使用
- **历史 / 收藏**：任务进度、已完成记录（文件丢失会自动标记）、收藏列表

---

## 已知限制

**Android 版站点功能不可用。** Android 系统没有 curl，而站点在 Cloudflare 后面会对 Rust 客户端的 TLS 请求返回 JS 挑战页。带完整 Chrome UA、关掉 HTTP/2、换代理方式都试过，无效；桌面端能通是因为有系统 curl 兜底。手机端因此只保留 CDN 直连下载分片的能力。

**ffmpeg 不是必装但强烈建议。** 没有它只能输出 `.ts`（能播，但不能拖进度条）。程序按「同目录 → `FFMPEG` 环境变量 → PATH → 常见安装位置」的顺序查找。

**解析依赖 HTML 结构。** 目标站改版会导致抓取失败，需要跟着改正则 —— 这是所有同类工具的通病。

**需要能访问目标站点的代理。** 程序只做代理探测和流量转发，不提供任何翻墙能力。

---

## 怎么用

### Windows

解压后双击 `MissAV下载器.exe`。同目录下必须有 `ui/` 文件夹。

首次启动会在 `下载/MissAV` 建输出目录，可在界面里改。

### Android

安装 `*-arm64.apk`（arm64-v8a，覆盖绝大多数现代机型）。需要允许「安装未知来源应用」。

自签证书，仅供自己安装；上架应用商店才需要正式签名。

### 关于代理

程序会自动探测本地代理（Windows 读系统代理设置，Android 读系统代理配置），也可以手动指定。

数据链路是分层的，**站点页需要代理，CDN 通常直连更快**：

| 环节 | 是否走代理 |
| --- | --- |
| 搜索结果 / 详情页 | 需要 |
| 封面图 | 建议直连 |
| 视频分片 | 默认直连 |
| 播放（m3u8 与分片） | 经本地反向代理，补齐 Referer 后转发 |

---

## 技术设计

### 架构

```
src-tauri/src/
├── fetch.rs    抓取层：代理探测与降级、重试、空闲超时流式下载
├── hls.rs      HLS：master/media 解析、AES-128 解密、并发下载、拼接、转封装
├── site.rs     站点解析：列表/详情、播放源分级提取、JS packer 解包
├── api.rs      本地 HTTP 服务 + m3u8 反向代理 + 静态资源
├── tasks.rs    任务队列、进度上报、历史与收藏持久化
├── lib.rs      桌面与移动端共用入口、IPC 命令
└── main.rs     桌面可执行入口（转发到 lib）
```

**桌面端**起一个本地 HTTP 服务（绑定 `0.0.0.0`），窗口加载 `http://127.0.0.1:<port>`。
好处是：手机在同一局域网扫码即可访问同一套界面，前端代码只有一份。

**Android 端**没有本地 HTTP 服务的位置，改为走 Tauri `invoke` 命令。前端有一层适配层，同一份 `index.html` 在两种环境下都能跑。

### 几个踩过的坑

这些都是实测出来的，写在代码注释里：

**播放源必须分级。** 页面里既有真播放源，也有广告位的 `playlist.m3u8`。只按出现顺序取第一个，会被广告位抢占，结果就是选了「无码」下载下来还是有码。所以按「显式赋值 > 解包结果 > 裸链接」三级排序，`playlist.m3u8` 一律靠后。

**播放源被 JS 压缩。** 站点用 Dean Edwards packer（base62）压地址。Rust 标准库 `u32::from_str_radix` 最大只支持 36 进制，直接用会 panic，得自己实现 base62 解码。

**分片伪装扩展名。** 分片地址是 `.jpeg`，内容是 TS。不能靠扩展名判断类型，得看首字节 `0x47`。

**固定总超时会把慢分片判死。** 有的分片在 CDN 上要 50 秒才下完（约 18 KB/s），用固定超时必然失败。改成**空闲超时**：只要还在来数据就一直等，停住 N 秒才放弃。

**一次性提交全部分片 Future 会爆内存。** 2600 片 × 每片 460 KB 的 Future 结果能涨到 GB 级。改成滑窗并发（窗口 = 线程数 × 2）。

**CDN 防盗链只认站点页 Referer。** 浏览器不能给 `<video>` 或 hls.js 的请求设 Referer，所以只能由本地服务做反向代理，把 m3u8 里的所有地址重写成本地路径，由服务端补上 Referer 再转发。

**本地代理做 TLS 中间人会导致证书链失败。** 报 `UnknownIssuer`。所以要优先用系统证书库，并在探测到这种情况时自动降级不校验证书。

**代理端口开着不等于能用。** 客户端会残留监听但实际不通。必须真发一次请求验证，且要多个探测目标（某些站点本身被封，单一目标会把好代理误判成坏）。

---

## 构建

### 依赖

- Rust 1.77+（含对应平台 target）
- Windows 需要 MSVC 工具链；Android 需要 JDK 17+、Android SDK、NDK
- 桌面打包用 `cargo tauri`（`cargo install tauri-cli --version "^2.0"`）

### 桌面

```bash
cargo tauri build --no-bundle
# 产物: src-tauri/target/release/<name>.exe，需与 ui/ 放在一起
```

### Android

```bash
cargo tauri android init      # 首次，生成 gen/android 工程
cargo tauri android build --target aarch64 --apk
```

**Android 构建的三个常见坑：**

**Gradle 下载慢。** 用国内镜像下载 `gradle-x.x-bin.zip`，解压到 `~/.gradle/wrapper/dists/<版本>/<hash>/`。

**项目路径含中文**会被 Android Gradle 插件拒绝。两种解法：放到纯 ASCII 路径，或用 junction 映射过去并加 `android.overridePathCheck=true` 到 `gradle.properties`。

**Windows 非开发者模式下没有符号链接权限**，而 Tauri 用 symlink 把 `.so` 放进 `jniLibs`，会失败。绕过方式：先编译 Rust 拿到 `.so`，手动复制到 `jniLibs/<abi>/`，再用 `-x rustBuild<Abi>Release` 跳过该任务直接打包。

```bash
# 手动放 .so 后
cd src-tauri/gen/android
./gradlew assembleArm64Release -x rustBuildArm64Release
```

签名用自己生成的 keystore，密码自己保管，**不要提交到仓库**。

---

## 测试

```bash
cd src-tauri
cargo test --lib
```

28 个单元测试，覆盖：

- HLS：master 排序、清晰度选择、相对/协议相对地址、加密与 IV 检测、纯音频流过滤
- 站点：番号规范化、有码判定、标题清洗、packer 解包与 base62 解析、播放源分级
- 任务：文件名清洗、长度限制、哈希稳定性
- API：m3u8 地址改写（含 `EXT-X-KEY`）、URL 查询解析
- 抓取：UTF-8 与非法字节解码、代理 URL 规范化

---

## 许可

代码以 MIT 发布。程序本身不包含、不分发任何视频内容，所有内容版权归原权利方所有。

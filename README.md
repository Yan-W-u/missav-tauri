# MissAV 下载器 · Tauri 2 版

用 Tauri 2 重写的版本。**原 Python 版（`D:\工作区\missav-dl`）原封未动**，两套可以并存。

## 先回答你的问题：手机电脑都能用吗？

**不是同一个包。** Tauri 2 一套代码可以出三个平台，但产物是分开的：

| 平台 | 产物 | 能否现在给 | 说明 |
| --- | --- | --- | --- |
| Windows | `MissAV下载器.exe`（6.1 MB） | ✅ 已构建 | 双击即用 |
| Android | `MissAV下载器-arm64.apk`（9 MB） | ✅ 已构建 | 传手机安装 |
| iOS | `.ipa` | ❌ | **必须有 Mac + Xcode**，这台 Windows 做不到 |
| macOS / Linux | `.dmg` / `.AppImage` | ❌ | 需要对应系统编译 |

所以「手机电脑都能用」只在 **Windows + Android** 这个范围成立。iOS 无解（除非换 Mac）。

---

## 交付物

```
D:\工作区\missav-tauri\dist\
├── desktop\
│   ├── MissAV下载器.exe     6.1 MB   ← 桌面版
│   └── ui\                            ← 必须和 exe 放一起
└── MissAV下载器-arm64.apk   9 MB     ← 安卓版（已签名）
```

体积对比：Python 版 exe 58 MB → Rust 版 6.1 MB（**缩到 1/10**）。
内存：Python 版约 100 MB → Rust 版约 31 MB。

### 桌面版用法

1. 把 `desktop\` 整个目录拷到目标机器（**exe 和 ui 必须在一起**）
2. 双击 `MissAV下载器.exe`

### 安卓版用法

1. 把 APK 传到手机（微信/QQ/数据线都行）
2. 手机上点开安装，需要允许「安装未知来源应用」
3. 装好直接运行

自签证书，仅供自己安装；上架应用商店才需要正式签名。

---

## 架构

```
src-tauri/
├── src/
│   ├── main.rs    桌面入口（转发到 lib）
│   ├── lib.rs     共享入口 + Tauri 命令（桌面/移动端共用）
│   ├── fetch.rs   HTTP 抓取层（代理探测、重试、空闲超时流式下载）
│   ├── hls.rs     HLS 解析/解密/下载/拼接/转封装
│   ├── site.rs    站点解析（列表、详情、播放源分级、packer 解包）
│   ├── api.rs     本地 HTTP 服务 + 静态资源 + 反向代理
│   └── tasks.rs   任务队列、进度、历史、收藏
└── gen/android/   Android 工程（tauri android init 生成）
```

### 为什么桌面端还开 HTTP 服务

现有前端（`index.html` / `player.html`）是 `fetch('/api/...')` 写的。走 HTTP 就能**一行不改地复用**，
同时手机能通过局域网访问同一套接口。

移动端则走 Tauri `invoke` 命令（`cmd_search`、`cmd_download` 等），因为手机上没有"本地 HTTP 服务"的位置。

### 从 Python 版继承的关键决策

这些坑都是原版实测踩出来的，Rust 版逐条保留：

| 问题 | 处理 |
| --- | --- |
| CDN 防盗链只认站点 Referer | 所有请求带 Referer + Origin |
| 播放源被广告位抢占（无码变有码） | 三级分级：`source=` 赋值 > packer 解包 > 裸链接；`playlist.m3u8` 排最后 |
| 站点用 Dean Edwards packer 压缩播放源 | 自己实现 base62 解包（标准库 `from_str_radix` 只到 36 进制，会 panic） |
| 分片伪装 `.jpeg` 实为 TS | 按首字节 `0x47` 判断，不看扩展名 |
| 固定总超时导致慢分片永远失败 | 空闲超时：只要还在来数据就等 |
| 一次性提交全部 Future 撑爆内存 | 滑窗并发（窗口 = 线程数 × 2） |
| 本地代理做 TLS 中间人 → UnknownIssuer | 启用 `rustls-tls-native-roots` + 自动降级不校验证书 |
| 代理端口开着但实际不通 | 必须真发请求验证，且系统代理优先于端口扫描 |

---

## 已知限制（如实说明）

### 1. 站点被墙时需要能转发 missav 的代理

实测你当前环境：

```
百度              → 200   （代理正常）
fourhoi.com CDN   → 200   （CDN 正常）
missav.live 站点  → 超时   （被阻断）
```

当前代理是 SteamTools（`26561`），它只加速 Steam，**不转发 missav**。
这不是 Rust 版的问题——PowerShell 用同一代理访问 missav 也超时。

要能搜索，需要一个**能访问 missav 的代理**（Clash、v2rayN 等），程序会自动探测。

### 2. 安卓版 — 已在模拟器实测

测试环境：`hghub_test` AVD（Android 14，x86_64 + arm64 转译），APK 安装后 `primaryCpuAbi=arm64-v8a`。

**跑通的（有截图证据）**：

| 项目 | 结果 |
| --- | --- |
| 安装 | Success，包名 `com.missav.downloader`，versionName 2.0.0 |
| 启动 | 无崩溃，WebView 正常渲染，内存约 250 MB |
| 界面 | 搜索框、分区、标签、导入按钮全部正常显示 |
| IPC 通路 | 搜索返回 Rust 侧生成的错误文案；导入检测真实跑了 11.9 秒并回传结果 |
| 裸域名补全 | 输入 `example.com` → 自动补 `https://example.com` |
| 三级判定 | `可播放 0 · 仅可达 1 · 失败 0`，逐条给出原因 |
| 连通性横幅 | `分片可直连` 变绿点（Rust 真的访问到了 fourhoi CDN） |

**实测中发现并修掉的 3 个问题**：

1. **安卓上探测不到代理** — `read_system_proxy()` 原来只读 Windows 注册表，非 Windows 分支只读环境变量。Android 的系统代理存在 `settings global http_proxy`，两边都读不到，导致所有请求走直连然后超时。已加 `android_http_proxy()` 分支 + 探测 `10.0.2.2`（模拟器里的宿主机地址）。
2. **`/api/connectivity` 映射错误** — 之前偷懒映射到 `cmd_lan`（只返回 IP），前端拿不到 `site`/`cdn` 字段，状态栏永远显示不可达。已补 `cmd_connectivity` / `cmd_proxy_status` / `cmd_proxy_detect` 三个命令。
3. **`/api/proxy/status` 和 `/api/proxy` 没映射** — 点代理状态会抛「该功能在手机上暂不支持」。已补。

**测试中踩到的环境坑（与 App 无关，但会影响你复现）**：

- SteamTools 代理只监听 `127.0.0.1`，模拟器访问不到。要用 `adb reverse tcp:26531 tcp:26531` 把它映射进去，再把模拟器代理设成 `127.0.0.1:26531`。
- 模拟器网络状态是 `partialConnectivity`（不是 VALIDATED），外网访问受限。`example.com` 能通，`missav.live` 不能——和宿主机表现一致。

**仍未验证的**：真机安装、下载全流程（需要能连 missav 的代理）、在线播放（需要能取到 m3u8）。

### 3. 安卓版当前只编了 arm64

`arm64-v8a` 覆盖绝大多数现代手机。老的 32 位设备需要另编 `armv7`。

---

## 构建方法（复现用）

环境要求（本机已装好）：
- Rust 1.99.1 + `aarch64-linux-android` target
- VS 2022（`E:\c\c`）+ `vcvars64.bat`
- JDK 21、Android SDK（`D:\android-sdk`）、NDK 27

### 桌面版

```powershell
cd D:\missav-tauri
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cmd /c "`"E:\c\c\VC\Auxiliary\Build\vcvars64.bat`" >nul 2>&1 && cargo tauri build --no-bundle"
# 产物: src-tauri\target\release\missav.exe
```

### 安卓版

Windows 上构建 Android 会遇到三个坑，都已绕过：

**坑 1：Gradle 下载超时**
境外源慢，用腾讯镜像：
```powershell
Invoke-WebRequest "https://mirrors.cloud.tencent.com/gradle/gradle-9.6.1-bin.zip" -OutFile "$env:TEMP\g.zip"
# 解压到 %USERPROFILE%\.gradle\wrapper\dists\gradle-9.6.1-bin\<hash>\
```

**坑 2：路径含中文**
`D:\工作区` 被 Android Gradle 插件拒绝。解决：
```powershell
cmd /c "mklink /J D:\missav-tauri `"D:\工作区\missav-tauri`""
Add-Content D:\missav-tauri\src-tauri\gen\android\gradle.properties "android.overridePathCheck=true"
```

**坑 3：符号链接权限**
Tauri 用 symlink 把 `.so` 放进 jniLibs，Windows 非开发者模式没权限。绕过方式：
```powershell
# 1) 先编译 Rust（这一步 Tauri 能做成功）
cd D:\missav-tauri
cargo tauri android build --target aarch64 --apk   # 会在 symlink 处失败，但 .so 已生成

# 2) 手动放 .so
$src = "src-tauri\target\aarch64-linux-android\release\libmissav_lib.so"
$dst = "src-tauri\gen\android\app\src\main\jniLibs\arm64-v8a\libmissav_lib.so"
Copy-Item $src $dst -Force

# 3) 跳过 rust 构建直接打包
cd src-tauri\gen\android
.\gradlew.bat assembleArm64Release --no-daemon -x rustBuildArm64Release

# 4) 签名（自己生成一份，密码自己定，不要提交 keystore）
keytool -genkeypair -keystore missav.keystore -alias missav -keyalg RSA `
  -keysize 2048 -validity 10000 -storepass <你的密码> -keypass <你的密码> `
  -dname "CN=MissAV, OU=Dev, O=MissAV, L=CN, ST=CN, C=CN"
D:\android-sdk\build-tools\34.0.0\apksigner.bat sign `
  --ks missav.keystore --ks-pass pass:<你的密码> --key-pass pass:<你的密码> `
  --out MissAV下载器-arm64.apk app\build\outputs\apk\arm64\release\app-arm64-release-unsigned.apk
```

> `.keystore` 是签名私钥，泄露等于别人能伪造你的 App 更新。本仓库的 `.gitignore` 已排除它。

---

## 测试

```powershell
cd D:\missav-tauri\src-tauri
cmd /c "`"E:\c\c\VC\Auxiliary\Build\vcvars64.bat`" >nul 2>&1 && cargo test --lib"
```

**28 个单元测试全部通过**，覆盖：

- HLS：master 排序、清晰度选择、相对/协议相对地址、加密与 IV 检测、音频流过滤
- 站点：番号规范化、有码判定、标题清洗（不能吃掉番号）、packer 解包、base62 解析、播放源分级
- 任务：文件名清洗、长度限制、哈希稳定性
- API：m3u8 地址改写（含 EXT-X-KEY）、URL 参数解析
- 抓取：UTF-8/非法字节解码、代理 URL 规范化

诊断测试（需真实代理）：
```powershell
cargo test --lib diag_ -- --ignored --nocapture
```

---

## 与原 Python 版的关系

| | Python 版 | Tauri 2 版 |
| --- | --- | --- |
| 位置 | `D:\工作区\missav-dl` | `D:\工作区\missav-tauri` |
| 状态 | **原封未动**，可继续用 | 新增，独立 |
| 体积 | 58 MB | 6.1 MB |
| 内存 | ~100 MB | ~31 MB |
| 手机 | 浏览器扫码（PWA） | 原生 APK |
| 成熟度 | 长期验证，功能齐全 | 后端逻辑等价，安卓前端待适配 |

两套互不干扰，Python 版继续可用。

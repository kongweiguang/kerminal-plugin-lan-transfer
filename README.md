<!-- @author kongweiguang -->

# 局域网文件传输 / LAN File Transfer

Kerminal 外部插件（Rust Sidecar）：在本机选择一个共享目录，用同一局域网内的浏览器浏览、上传与下载文件。控制面是 Kerminal 原生面板，数据面是插件进程内的本地 HTTP 服务。

## 能力

- 面板内选择共享目录与本机监听地址，启动后由系统分配端口，生成带高熵密钥的访问链接与二维码。
- 浏览器打开链接后，用 URL 片段里的密钥配对，换取短期 Bearer 令牌；片段随即从历史记录中移除。
- 目录浏览、单文件上传（同名自动改名，不覆盖）、单文件下载（短期票据 URL，浏览器原生下载与进度）。
- 面板显示运行状态、监听地址、可用本机地址、分享链接、二维码、上传/下载计数与失败原因；停止服务会撤销全部会话与票据。
- 「复制链接」通过宿主 `clipboard.write` 写入系统剪贴板；写入失败会作为动作错误上报，不会只在界面显示「已复制」。

## 安全边界（必须了解）

- **HTTP 明文传输**：链路不加密，同一局域网内的人可能看到文件内容与会话数据。只在可信网络使用。
- 服务只监听用户选择的本机地址，端口由系统分配；只服务所选目录及其安全子目录，不跟随链接与 Windows 重解析点，隐藏项与内部暂存文件不可见、不可写。名字无法通过路径段规则表达的条目（例如 Linux 上的 `a:b.txt`、尾空格）会被整条隐藏，列表不会为它们生成空 id。
- 所有响应带 `Content-Security-Policy: frame-ancestors 'none'`、`X-Frame-Options: DENY`、`nosniff`、`no-referrer`、`no-store`。
- 所有非 `GET`/`HEAD` 请求由同一层中间件要求与本次请求的 `Host` 精确同源（`Host: 127.0.0.1:P` 配 `Origin: http://localhost:P` 视为跨源而拒绝）；`Host` 必须精确匹配当前监听地址（含端口），IPv6 只接受 `[ip]:port`，`localhost` 按大小写不敏感比较。`GET`/`HEAD` 允许缺少 `Origin`（浏览器原生下载不带），但存在时同样校验。
- 文件名只来自查询串，按同一套路径段规则校验，绝不参与拼接绝对路径；`CON`、`a/b`、`.hidden`、暂存前缀等一律拒绝。
- 上传先写内部暂存文件，刷盘后才以「不覆盖」方式改名提交；提交失败不谎称成功，也不自动重试。
- 下载票据绑定单个文件与当前服务代次，过期或服务重启即失效，且不能用于其他接口。下载响应声明 `Accept-Ranges: bytes` 并支持标准单区间 `Range`（`bytes=start-end`、`bytes=start-`、`bytes=-suffix`，返回 206 与 `Content-Range`；起点越界返回 416 与 `bytes */size`；语法非法或多区间按 RFC 忽略并整份返回 200）。只承诺单次请求内的浏览器续传兼容，不承诺跨会话断点续传；票据失效后需重新获取票据从头下载。
- 服务端返回「已发送」不等于浏览器「已保存」：网络中断、浏览器取消或磁盘写满都可能让接收方没有完整文件。

## 构建

需要 Kerminal 随附的 `kerminal-plugin` CLI（`build`、`check`、`package`、`dev`）与 Rust 工具链。安装插件的用户不需要 Rust。

1. 把 Kerminal SDK 仓库检出到本项目的 `.kerminal-sdk/`，切换到作者确认的完整 40 位提交 SHA（目录必须包含 `sdk/cli/Cargo.toml`、`sdk/protocol/Cargo.toml` 与 `sdk/rust/Cargo.toml`）。本仓库核对时可公开获取的固定提交是 `ea099eb33a562422cb1bfd6678187a007a7d2a41`（核对时位于 `kongweiguang/Kerminal` 的 `main`）；发布工作流应把 `KERMINAL_SDK_REF` 设为同一完整 SHA。CLI 与 SDK 必须来自同一提交，不能用漂移的默认分支代替。
   插件的 `Cargo.toml` 在 `[workspace]` 里显式 `exclude = [".kerminal-sdk"]`：SDK 检出自带 `[workspace]`、`workspace.package` 与 `workspace.dependencies`，`sdk/rust` 依赖 `edition.workspace` 等继承字段。不排除时 Cargo 会把插件清单当作 SDK 的外层工作区根，继承落到插件的空 `[workspace]` 上，依赖解析直接失败。排除后 SDK 仍按自身清单解析，插件工作区只含本包；开发机的 junction 与 CI 的真实目录布局都适用，不需要复制或改写 SDK 的工作区配置。
2. 在项目根执行：

```powershell
kerminal-plugin build
kerminal-plugin check .kerminal/build
kerminal-plugin package
```

`resources/**` 会原样复制到包根，因此视图 `views/panel.json`、文案 `l10n/*.json` 与图标 `icon.svg` 都以 `resources/` 为源。Sidecar 二进制名固定为 `kerminal-plugin-sidecar`（Windows 为 `.exe`），清单 `runtime.main` 的平台路径与打包后布局一致。

Sidecar 可额外运行 `kerminal-plugin conformance <exe>` 检查握手、身份、分帧、心跳与关闭；该检查只证明这五项，取消与背压未用 fixture-mode 验证，也不覆盖真实界面、崩溃恢复与多平台验收。

`cargo test` 覆盖的入口契约：`tests/backend/rpc_handshake.rs` 用内存双向流驱动与生产相同的入口，验证握手、`view.snapshot`/`view.open`、未知方法、协议保留方法不可注册，以及 `shutdown` 与输入 EOF 后服务确实回到空闲。生命周期部分在 `src/service.rs` 的单元测试里用可控通道任务覆盖「停止等待中被取消后仍能再次确认回收」「超时不越过共享预算且不丢句柄」「重新启动回收旧句柄时取消不丢句柄」「取消登记前留下的已中止句柄由后续停止确认而不是伪报成功」，不依赖真实端口或固定 sleep；这些用例已在 2026-11-16 的 Windows 11 本地 `cargo test --locked` 中实际运行通过。真实子进程退出码、宿主收到 `shutdown` 回复后的强杀宽限期，以及标准输入以外的管道故障仍未被这些单元测试覆盖；真实子进程的握手、HTTP 传输与 `shutdown` 退出由 `tests/sidecar_http_e2e.py --sidecar` 覆盖。

## 目录

| 路径 | 职责 |
| --- | --- |
| `src/` | Sidecar：配置、路径规则、平台文件系统、HTTP、会话、票据、视图 RPC |
| `resources/views/` | 原生声明式视图 |
| `resources/l10n/` | 面板文案（`en.json`、`zh-CN.json`） |
| `tests/backend/` | 后端契约回归（安全头、Host/Origin、鉴权、路径拒绝、上传不覆盖、票据边界、HEAD/Range、停止后拒绝、视图绑定、协议握手与关闭回收） |
| `web/` | 浏览器页面（独立执行者维护） |

## 本地验证

隔离目标目录与配置根，避免污染开发机上的其他构建：

```powershell
$env:CARGO_TARGET_DIR = 'C:/tmp/klt-target'
$env:KERMINAL_CONFIG_ROOT = 'C:/tmp/klt-cfg'
cargo test
```

`tests/backend/` 直接驱动真实路由与真实文件系统（临时目录），覆盖安全头、Host/Origin、鉴权、路径拒绝、上传不覆盖、票据边界、HEAD/Range 语义、停止后的拒绝、视图绑定与文案键一致性；其中 `rpc_handshake.rs` 会启动真实的本地监听（绑定回环的随机端口）来验证启动与回收。平台相关的文件系统用例（Windows 变长文件名与多批次枚举、单字符提交、克隆句柄游标隔离、junction 根、Unix 符号链接）只在对应平台编译与运行，未在对应平台跑过时不能视为通过。这些测试不替代真实浏览器、真实局域网、真实宿主进程与安装包验收。

### 本地打包与验收（需要 Rust 与已固定的 SDK）

以下命令会执行真实构建、打包和 Sidecar 联调，必须在 `.kerminal-sdk` 已固定到上面的公开提交后运行：

```powershell
$env:CARGO_TARGET_DIR = 'C:/tmp/klt-target'
$env:KERMINAL_CONFIG_ROOT = 'C:/tmp/klt-cfg'
cargo test --locked
kerminal-plugin build --platform windows-x86_64
kerminal-plugin check .kerminal/build --platform windows-x86_64
kerminal-plugin package --platform windows-x86_64
$version = (Get-Content -LiteralPath 'kerminal-plugin.json' -Raw | ConvertFrom-Json).version
kerminal-plugin check ".kerminal/packages/kongweiguang.lan-transfer-$version-windows-x86_64.kplugin" --platform windows-x86_64
kerminal-plugin conformance .kerminal/build/bin/kerminal-plugin-sidecar.exe
python -I tests/sidecar_http_e2e.py --sidecar .kerminal/build/bin/kerminal-plugin-sidecar.exe
```

2026-11-16 在 Windows 11（`x86_64-pc-windows-msvc`，Rust 1.98.0）上按上述顺序实际执行并通过：`cargo test --locked` 共 117 个测试（70 个库测试、29 个 HTTP 契约、6 个 Origin 契约、6 个 RPC 握手、6 个视图数据）全部通过，0 失败、0 忽略；Sidecar release 构建成功；`check`、`package`、归档 `check` 与 `conformance` 均返回 0；`sidecar_http_e2e.py --sidecar` 6/6 通过，其中 5 项是脚本内的 `HelperSelfCheck`（链接解析、百分号编码、脱敏与清单身份等辅助校验），只有 1 项是真实 Sidecar 进程的完整流程（配对、列目录、上传不覆盖、下载、Range、停止后旧令牌/票据失效、重启与 `shutdown` 回收），不是 6 个独立的真实场景。本轮构建使用工作区既有的 `.kerminal-sdk` junction（其 HEAD 为固定提交 `ea099eb33a562422cb1bfd6678187a007a7d2a41`），没有重新检出或切换 SDK；`conformance` 只证明握手、身份、分帧、心跳与关闭，取消与背压未用 fixture-mode 验证。上述构建与打包产物对应本轮源码；本轮新增「单字符目标名提交」回归、重写「克隆句柄枚举游标隔离」回归，并把改名结构的 union 初始化为等价的 `Flags: 0`；单字符回归先复现了 22 字节被内核以 `ERROR_BAD_LENGTH`（24）拒绝，修复为长度下限取 `size_of::<FILE_RENAME_INFORMATION>()` 后复跑通过。

`cargo test --locked` 依赖已提交的 `Cargo.lock`。真实浏览器、真实局域网、宿主安装以及 macOS/Linux 原生运行仍需在对应环境单独验收。

## 发布

在[插件源码仓库](https://github.com/kongweiguang/kerminal-plugin-lan-transfer)推送与清单版本匹配的 `v*` 标签后，GitHub Actions 使用 `KERMINAL_SDK_REF` 固定的 SDK 提交构建 Windows x64、Linux x64、macOS Intel 和 macOS Apple Silicon 的 `.kplugin`。四个平台的包全部通过校验、上传并回读比对后，工作流才公开 Release。SDK 的配置方法见上面的构建说明。

所有版本（含正式版）对 macOS Sidecar 仅做本地 ad-hoc 签名和严格完整性验证，不使用 Apple Developer ID 证书，也不提交 Apple 公证。ad-hoc 签名不证明发布者身份；下载后仍受系统 Gatekeeper 策略约束，可能无法启动。CI 构建和协议检查不代替 macOS 桌面上的真实安装验收。

## 已知限制

- 面板使用 `qrcode` 原生节点。相关源码已随 Sidecar 完成 release 编译，并通过真实 Sidecar 的 `view.snapshot`/`view.open`/`view.action` 黑盒联调；原生窗口渲染与宿主安装仍未验收，在完成这些验证之前不声称桌面端可用。
- 清单声明 `api: ">=1.1.0 <2.0.0"`，当前宿主权威常量 `HOST_API_VERSION` 为 `1.1.0`（`crates/application/src/plugins.rs`），因此该范围与当前宿主兼容；宿主在安装、启动与贡献注册三处都用同一套 npm 范围语义校验 `manifest.api`，更旧的 1.0 宿主仍会拒绝加载。此结论来自宿主源码与测试，不是本轮桌面安装验收。
- 真实浏览器反嵌入、真实局域网与 macOS/Linux 原生运行未验收；Windows 无目录 fsync，不对崩溃后的目录项持久性作承诺。
- 共享目录当前需要创建权限：只读 ACL（缺少创建文件/子目录权限）的目录无法作为可读共享目录打开；本轮不修改权限模型。
- 超长文件名没有空间追加自动改名后缀时，同名上传可能以 409 失败（仍不覆盖）；本轮不修改名字策略。
- 目录条目超过 `MAX_DIR_ENTRIES`（20000）时列举会返回 `too_large` 而不是截断；恰好上限仍接受、超过一条即失败这一边界由纯函数回归固定，但尚未用真实两万条目目录做端到端验证。
- Windows 上「共享根本身是 junction」由内核 `OBJ_DONT_REPARSE` 拒绝，代码另有属性复核；该场景只在能创建 junction 的 NTFS 环境里由测试实际执行，其他环境会跳过。目录枚举按 `FileNameLength` 读取变长文件名（`FILE_ID_BOTH_DIR_INFO.FileName` 在 windows-sys 中只是 `[u16; 1]` 占位），并有长 ASCII、中文、代理对文件名与多批次目录的回归；这些用例已在 2026-11-16 的 Windows 11 本地测试中实际运行通过。macOS/Linux 原生运行仍未验收。
- 提交（改名）不再按暂存名定位源文件：Windows 直接对创建时的写句柄执行改名，Linux 以匿名 `O_TMPFILE` 暂存并通过 `/proc/self/fd/<fd>` 链接到目标名，因此同目录的并发本地写入者替换同名暂存文件不能改变提交内容。目标名冲突仍由内核以「已存在」失败，不覆盖语义不变。Linux 的暂存清理同样做身份判断：只有当目录项仍指向本次写入的 inode 时才删除，避免误删并发写入者替换进去的文件；`statat` 与 `unlinkat` 之间仍存在尽力而为的残留窗口，但它只影响隐藏暂存名，不影响提交内容与不覆盖语义。macOS 没有 `O_TMPFILE` 与 fd 链接入口，只能按名 `renameatx_np(RENAME_EXCL)`：不覆盖仍成立，但「提交的就是本次写入的字节」在该平台无法证明，属于未消除的限制；相关代码与依赖 API 已按锁定源码核对，未做 macOS 原生或安装验收。

许可证：GPL-3.0-or-later，见 `LICENSE`。

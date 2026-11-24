# Haltd 架构与设计文档

> 代码注释中的 `DESIGN §x` 引用均指向本文档对应章节。

---

## 目录

1. [定位与目标](#1-定位与目标)
2. [设计输入与目标工程假设](#2-设计输入与目标工程假设)
3. [功能范围（与 Ozone 对照）](#3-功能范围与-ozone-对照)
4. [总体架构](#4-总体架构)
5. [模块详细设计](#5-模块详细设计)
6. [对外协议](#6-对外协议)
7. [配置文件](#7-配置文件)
8. [VS Code 扩展](#8-vs-code-扩展)
9. [其他编辑器集成](#9-其他编辑器集成)
10. [固件侧约定](#10-固件侧约定)
11. [性能目标与预算](#11-性能目标与预算)
12. [仓库结构](#12-仓库结构)
13. [测试策略](#13-测试策略)
14. [附录](#14-附录)

---

## 1. 定位与目标

**Haltd** 是面向嵌入式 MCU 的开源常驻调试后端、VS Code 扩展与命令行工具，采用「常驻守护进程（daemon）+ DAP + WebSocket 数据通道」架构，旨在将大工程快速加载、`-O2` 优化代码源码级调试以及不停机实时观测（Live Watch、1 kHz 高速波形 Timeline、结构化 RTT 解码）统一放进编辑器与 CLI 工作流中。

当前版本主力面向 **ARM Cortex-M / STM32 + SEGGER J-Link（重点实测 J-Link V9 PLUS）**，同时在架构上将探针访问隔离在抽象层 `probe-api` 之后，为后续接入更多探针后端保持边界清晰。

### 1.1 解决的核心痛点

| 痛点 | 根因 |
|---|---|
| 编辑器与调试器割裂 | 改代码编译后需在外部 GUI 调试器中重新加载 ELF 与配置 |
| 大型 C++ 固件加载缓慢 | 百 MB 级 DWARF（模板密集）每次启动都会全量解析 |
| `-O2` 构建单步与变量体验差 | 行内多地址、大量内联函数、无帧指针与局部变量寄存器化 |
| 实时观测与调试难兼顾 | 希望在不暂停控制循环的前提下同时看变量、波形与结构化 RTT |

### 1.2 核心目标

- **G1**：在 VS Code 及兼容 DAP 的编辑器中完成源码级调试（断点、条件/数据断点、`-O2` 单步、调用栈、局部变量、寄存器、SVD 外设寄存器、HardFault 诊断、FreeRTOS 任务感知）。
- **G2**：提供 **Live Watch**（0–5 Hz 不停机读写）与 **Timeline**（HSS 1 kHz 采样 / 自动轮询降级 / RTT 自动与配置解码统一时间轴）。
- **G3**：支持 Build & Reload（`F6`）增量烧录并热重载符号，不重连探针、自动重绑断点与观测项。
- **G4**：在 **RelWithDebInfo（`-O2 -g`）** 优化构建下保持完整可用的调试与表达式求值体验。

### 1.3 不做的部分

- 反汇编视图与指令级单步（需要时可使用其他工具或并连 Ozone）。
- ETM 指令跟踪、代码覆盖与功耗曲线（硬件依赖 J-Trace / 专有探针）。
- SWO（ITM / PC 采样）：目标板 SWO 引脚可能被外设复用，`probe-api` 仅预留 `SwoApi` 扩展接口。

---

## 2. 设计输入与目标工程假设

架构设计基于以下典型嵌入式工程特征：

| 维度 | 假设与特征 | 对架构的影响 |
|---|---|---|
| MCU / 内核 | ARM Cortex-M3 / M4F / M7（如 STM32F1 / G4 / H7，540 MHz 主频下 32 位 `DWT_CYCCNT` 约 7.95 s 回绕） | 默认按典型 8 个 FPB 断点比较器、4 个 DWT 比较器规划资源；时间同步与周期计数必须处理 32 位回绕 |
| 构建与优化 | C/C++ RelWithDebInfo（`-O2 -g`、`-ffunction-sections`、常见 `-fno-rtti -fno-exceptions`） | 需专门处理行内多地址、内联虚拟栈帧、无帧指针与 `<optimized out>`（§5.4、§5.5.3） |
| 调试信息规模 | DWARF 5（含 `.debug_loclists`、`.debug_rnglists`、`.debug_line_str`、`.debug_frame`），ELF 体积可达百 MB 级且可能无 `.debug_names` | 符号引擎必须采用 `mmap` + 内容哈希落盘缓存 + `rayon` 并行建索引 + 按需懒解析（§5.3） |
| RTOS | 裸机或 FreeRTOS（`ARM_CM3` / `ARM_CM4F` / `ARM_CM7` port），无需额外改写固件源码 | 任务状态与寄存器上下文布局全部由 DWARF 结构体信息动态推导（§5.5.6） |
| SWO 引脚 | 假设首块目标板无可用 SWO 引脚（如被 SPI 复用） | 不依赖 SWO 进行数据采样，实时数据统一走 AHB-AP / HSS 与 RTT |
| 调试探针 | 当前通过动态加载官方 J-Link 库（`libjlinkarm.so` / `JLink_x64.dll`）通信 | 单线程 Actor 串行化全部库调用并按优先级调度（§5.2.3） |

---

## 3. 功能范围（与 Ozone 对照）

| # | Ozone 功能 | Haltd 的实现方式 | 所在模块 |
|---|---|---|---|
| F1 | 源码断点、函数断点 | DAP `setBreakpoints` / `setFunctionBreakpoints`；优先使用 FPB，超出后按配置使用 Flash 断点 | `engine/breakpoints` |
| F2 | 运行控制、源码单步（over / into / out） | 适配 `-O2` 优化代码与内联调用的混合单步算法（§5.5.3） | `engine/step` |
| F3 | 调用栈（含异常帧与内联帧） | 基于 `.debug_frame` CFI 回溯，识别 `EXC_RETURN` 插入 `<exception N>` 帧并展开内联虚拟帧 | `symbols/unwind` |
| F4 | Local Data | DWARF location list 按当前 PC 求值，寄存器变量分配合成地址，不可求值时显示 `<optimized out>` | `symbols/loc` |
| F5 | Global / Watched Data（不停机刷新） | Live Watch：AHB-AP 后台读取与写回，每条目独立 0–5 Hz 刷新 | `sampler/watch` |
| F6 | Data Sampling + Timeline | 优先 1 kHz HSS 分块采样，超预算自动降级轮询；WebUI uPlot 多轨道同步示波器 | `sampler`, `tsdb`, `webui` |
| F7 | Terminal（RTT） | J-Link RTT 引擎；通道 0 作为双向交互终端 | `rtt` |
| F8 | 下载与 ELF 热重载 | Build & Reload（`F6`）：增量擦写变化扇区，并行重建符号索引并重绑断点/监视项 | `probe-jlink/flash`, `engine` |
| F9 | RTT 数据解析入 Timeline | 支持零配置 ELF 结构体 / `key=value` 自动解码，以及 `rtt.yaml` 声明的文本/正则/结构体/Lua/SystemView | `rtt/decode` |
| F10 | 条件断点、命中计数、日志点 | 命中暂停后由后端浅层回溯快速求值，不满足条件则自动恢复运行 | `engine/conditions` |
| F11 | 数据断点（Watchpoint） | 直接写 CoreSight DWT 比较器，对应 DAP `dataBreakpoints` | `engine/breakpoints`, `probe-jlink` |
| F12 | CPU 寄存器（含 FPU） | Variables 视图提供 Registers 作用域（r0–r15、xPSR、MSP/PSP、PRIMASK/BASEPRI/FAULTMASK/CONTROL、FPSCR、s0–s31） | `engine/session`, `server/dap` |
| F13 | 外设寄存器（CMSIS-SVD） | `svd` crate 解析 SVD 文件（含继承、`derivedFrom`、`dim`、`cluster`），自动保护读副作用寄存器 | `svd`, `engine`, `vscode-ext` |
| F14 | Memory 窗口 | 实现 DAP `readMemory` / `writeMemory` | `server/dap`, `engine` |
| F15 | 故障分析（Fault） | `DEMCR` 向量捕获在入口暂停，译码 `CFSR`/`HFSR`/`MMFAR`/`BFAR`/`AFSR` 并跨异常帧定位源码行 | `engine/fault` |
| F16 | 周期计数 | 每次暂停读取 `DWT_CYCCNT` 差值并换算微秒（含 32 位回绕检测） | `engine/session` |
| F17 | RTOS 任务感知（FreeRTOS） | 识别任务状态、优先级、栈高水位（`0xA5` 扫描）与每任务独立调用栈 | `engine/rtos` |
| F18 | 表达式求值、类型美化 | 优先级爬升 C/C++ 表达式求值器 + Lua 5.4 `visualizer` 规则 | `symbols/expr`, `visualizer` |
| F19 | 录制与导出 | 二进制 `.hlrec`（向后兼容读取 `.jlrec`）流式录制、可见窗口 CSV 导出、PNG 截图 | `tsdb/record`, `webui` |
| F20 | 离线状态快照 | 暂未实现 | — |
| F21 | 类 `.jdebug` 自动化脚本 | 暂未实现；项目级参数通过 `.haltd/project.yaml` 配置 | `engine/project` |
| F22 | 反汇编、指令级单步 | 不做 | — |
| F23 | ETM 指令跟踪、代码覆盖 | 不做 | — |
| F24 | 功率曲线 | 不做 | — |
| F25 | SWO（ITM / DWT 跟踪） | 当前不做；`probe-api` 预留 `SwoApi` trait | `probe-api` |

---

## 4. 总体架构

```
┌────────────────── VS Code 扩展 (TS) ─────────────────┐    ┌────── 其他 DAP 客户端 / 浏览器 ──────┐
│ DebugAdapterDescriptorFactory │ Live Watch TreeView  │    │ nvim-dap / 通用 DAP (stdio)         │
│ Peripherals & FreeRTOS Views  │ Device Assist        │    │ 浏览器打开 /timeline 与 /rtt          │
│ Timeline & RTT Webview (iframe + 主题同步)            │    └───────────┬──────────────────┬─────────┘
└──────┬──────────────────────────────┬────────────────┘                │ DAP (stdio)    │ WS/HTTP
       │ DAP (`haltd dap` stdio↔TCP)  │ WebSocket / HTTP                │                │
       ▼                              ▼                                 ▼                ▼
┌───────────────────────────────── haltd 后台常驻进程 (Rust) ─────────────────────────────────┐
│  server:    DAP 服务 (TCP 47111) │ HTTP 静态页面 & WebSocket JSON-RPC/二进制推送 (TCP 47110)   │
│  ────────────────────────────────────────────────────────────────────────────────────────── │
│  engine:    会话状态机 │ 断点/DWT │ -O2 单步 │ Fault 诊断 │ FreeRTOS 感知 │ SVD 外设         │
│  sampler:   Live Watch 轮询 │ HSS 块规划与轮询降级 │ 探针-主机时钟同步                         │
│  rtt:       RTT 发现与轮询 │ 解码器 (auto-struct / auto-text / text / regex / struct / lua) │
│  tsdb:      环形缓冲 │ 9 级 min/max 金字塔 LOD │ 事件轨道 │ .hlrec 录制与 CSV 导出             │
│  symbols:   ELF mmap + blake3 缓存 │ 并行名字索引 │ DWARF 5 懒解析 │ CFI/EXC 回溯 │ 表达式   │
│  visualizer: Lua 5.4 运行时 + 内置规则 (units / Eigen / std / etl)                           │
│  ────────────────────────────────────────────────────────────────────────────────────────── │
│  probe-api (Probe / HssApi / RttApi / SwoApi)  ←──  probe-jlink (ProbeActor 5 级优先队列)   │
│                                                  └──  probe-mock  (内存仿真 & 会话录制回放)   │
└──────────────────────────────────────────────────┬──────────────────────────────────────────┘
                                                   │ libloading (libjlinkarm.so / JLink_x64.dll)
                                                   ▼
                                         J-Link 探针 ── SWD/JTAG ── Cortex-M MCU
```

### 4.1 关键设计决定

| 决定 | 理由 |
|---|---|
| 后端**常驻运行**，不随单次 DAP 会话启停 | 跨调试会话复用探针连接与已加载的 `SymbolDb`，Reload / 二次启动免重连与重解析 |
| **控制流走 DAP，高频数据流走 WebSocket** | 编辑器原生支持 DAP 做断点/单步/栈帧；1 kHz 波形、RTT 与实时 Watch 不受 DAP 请求-响应队列阻塞 |
| **WebUI 内嵌于二进制中**（`rust-embed` + `axum`） | VS Code Webview iframe 与外部浏览器共享同一份 `webui/` 构建产物 |
| **单线程 `ProbeActor` 独占探针调用** | J-Link DLL 未声明线程安全；集中在一个线程并通过 5 级优先队列调度可避免高频采样阻塞单步 |
| **ELF 索引按内容 `blake3` 哈希缓存** | 首次并行建索引后落盘缓存，后续加载跳过全量 DWARF 扫描 |
| **所有时序数据共享同一 `Tsdb` 时间原点** | HSS、轮询、RTT 遥测与 Halt/Resumed/Fault/Reload 事件统一采用自后端启动以来的纳秒时间戳，前端可直接对齐绘制 |
| **本地回环与 Origin 隔离** | DAP（47111）与 HTTP/WS（47110）仅绑定 `127.0.0.1`；WS 握手校验 `Origin` 仅允许 `127.0.0.1` / `localhost` 或无 Origin 的本地原生客户端 |

### 4.2 进程与生命周期

- **`haltd serve`**（默认 HTTP/WS 端口 `47110`，DAP 端口 `47111 = DEFAULT_PORT + 1`）：后台守护进程，持有 `Session`、`Sampler`、`Rtt`、`ControlBus` 与 `Tsdb`。
- **`haltd dap`**（`crates/server/src/bridge.rs`）：编辑器直接启动的轻量 stdio ↔ TCP 47111 转发桥。若连接 47111 失败，自动以脱离父进程的方式拉起 `haltd --log info serve --dap-port <port>` 并把日志写入缓存目录下的 `serve.log`。在 Linux 上启动前会检查 `/proc` 中路径不同的旧构建后台并提前停止，防止升级二进制后仍连向旧进程。
- **单控制端互斥与数据客户端共存**：同一时刻仅允许一个 DAP 连接作为控制端（第二个 DAP 连接在首个请求上被拒绝），而多个 WebSocket 数据客户端可同时订阅和查询波形，并通过 `ControlBus` 向 `Dap` 主循环投递运行控制命令（`target.halt` / `target.go` / `target.reset` / `flash.reload`）。
- **空闲生命周期（`IdleTracker`）**：默认 `--idle-exit-secs 30`（设为 `0` 可长期常驻）。DAP 控制端断开超过限时后自动释放探针（`release_probe`），此时若仍有 WebSocket 页面打开，进程继续保留 `Tsdb` 历史数据供浏览；当连接器与 WS 客户端均空闲超时后自动退出。断开 DAP 会话时（`disconnect`），默认不强行复位或暂停目标（除非参数 `terminateDebuggee: true`）。

---

## 5. 模块详细设计

### 5.1 `probe-api`：探针抽象

定义与硬件探针厂商解耦的公共接口与目标特征类型（`crates/probe-api/src/lib.rs`）：

- **`Probe` trait**（`Send`）：
  - 连接与信息：`info() -> ProbeInfo`、`connect(&ConnectCfg) -> Result<TargetInfo>`、`disconnect()`
  - 运行控制：`halt()`、`go()`、`step_instr()`、`reset(ResetKind::{Normal, Core, Pin}, halt_after)`、`is_halted()`
  - 寄存器与内存：`read_regs(&[Reg], &mut [u32])`、`write_reg(Reg, u32)`、`read_mem(addr, buf, AccessWidth)`、`write_mem(addr, data, AccessWidth)`
  - 断点与监视点：`set_bp(addr, BpKind::{Hw, Flash, Any}) -> BpHandle`、`clear_bp`、`set_wp(&WatchpointSpec) -> WpHandle`、`clear_wp`
  - 烧录：`download(elf_or_bin, &dyn Fn(Progress)) -> DownloadStats`
  - 可选子能力：`hss() -> Option<&mut dyn HssApi>`、`rtt() -> Option<&mut dyn RttApi>`、`swo() -> Option<&mut dyn SwoApi>`
  - 扩展与诊断：`exec(cmd)`、`set_speed(khz)`、`call_stats() -> CallStats`
- **`ProbeExt`**：为所有 `Probe` 自动提供 `read_u32`、`write_u32`、`modify_u32` 读改写辅助。
- **`DeviceProfile`**（`crates/probe-api/src/device.rs`）：内置解析 `devices/{stm32f1,stm32g4,stm32h7,stm32h74}.yaml`，封装内存区、Flash 扇区大小、`postConnect` 初始化写寄存器与 `freezeOnHalt` 位映射。

### 5.2 `probe-jlink`：J-Link DLL 封装

#### 5.2.1 加载方式

通过 `libloading` 在运行时动态加载 J-Link 共享库（无需编译期链接专有 SDK）：
- 查找优先级：显式 `--dll` / `launch.json` `dll` / `HALTD_DLL` 环境变量 → 平台默认路径：
  - Linux：`/opt/SEGGER/JLink/libjlinkarm.so` → 系统库路径 `libjlinkarm.so`
  - Windows：读取注册表 `HKCU` / `HKLM\Software\SEGGER\J-Link\InstallPath` 下的 `JLink_x64.dll` → 系统搜索路径 `JLink_x64.dll`
- 所有 FFI 符号逐一解析为 `Option<fn(...)>`，缺失非关键符号时在 `ProbeInfo::missing_symbols` 中报告，对应能力优雅降级而不阻塞主流程。
- Windows 下调用 `JLINK_DownloadFile` 等路径接口前，会主动把路径转为系统当前 ANSI 代码页（`CP_ACP`），遇到无法表示的字符显式报错拒绝，绕过 J-Link Windows DLL 不支持 UTF-8 路径导致的 `-268` 错误。

#### 5.2.2 使用的 J-Link API

- **连接与器件枚举**：`JLINKARM_OpenEx`、`JLINKARM_Close`、`JLINKARM_ExecCommand`、`JLINKARM_TIF_Select`、`JLINKARM_SetSpeed`、`JLINKARM_Connect`、`JLINKARM_IsConnected`、`JLINKARM_GetSN`、`JLINKARM_GetFirmwareString`、`JLINKARM_GetHardwareVersion`、`JLINKARM_GetDLLVersion`、`JLINKARM_GetEmuCaps`、`JLINKARM_GetEmuCapsEx`、`JLINKARM_DEVICE_GetInfo`、`JLINKARM_Core2CoreName`。
- **运行控制与读写**：`JLINKARM_Halt`、`JLINKARM_IsHalted`、`JLINKARM_Go`、`JLINKARM_Step`、`JLINKARM_Reset`、`JLINKARM_SetResetType`、`JLINKARM_ReadReg`、`JLINKARM_ReadRegs`、`JLINKARM_WriteReg`、`JLINKARM_ReadMemEx`、`JLINKARM_WriteMemEx`。
- **断点与烧录**：`JLINKARM_SetBPEx`、`JLINKARM_ClrBPEx`、`JLINKARM_GetNumBPUnits`、`JLINK_SetFlashProgProgressCallback`、`JLINK_DownloadFile`（配合 `SetSkipProgOnCRCMatch = 1`）。
  - *注*：由于实测调用 `JLINKARM_SetWP` 后 J-Link DLL 会在 `go` / 复位时清除 `DEMCR` 的异常向量捕获位，`probe-jlink` 不使用 `JLINKARM_SetWP`，改为通过内存访问直接读写 CoreSight DWT 比较器寄存器（`DWT_COMP` / `DWT_MASK` / `DWT_FUNCTION`）。
- **HSS 与 RTT**：`JLINK_HSS_GetCaps`、`JLINK_HSS_Start`、`JLINK_HSS_Read`、`JLINK_HSS_Stop`、`JLINK_RTTERMINAL_Control`（含 `START`、`STOP`、`SETADDR`、`GETNUMBUF`、`GETDESC`、`GETSTAT`）、`JLINK_RTTERMINAL_Read`、`JLINK_RTTERMINAL_Write`。

#### 5.2.3 线程模型和调度

`ProbeActor` 是全进程唯一允许访问底层 `Probe` 的专用线程，通过 `ProbeHandle` 接收异步阻塞调用（`call_blocking`）或周期任务（`add_periodic`），内部维护 5 级优先队列：

| 优先级 | 用途 |
|---|---|
| `P0` | 运行控制（halt/go/step/reset）、断点设置、调试核心命令 |
| `P1` | HSS 缓冲读取（10 ms 周期）、运行暂停轮询 |
| `P2` | RTT 通道读取（5 ms 周期） |
| `P3` | Live Watch 与轮询采样 |
| `P4` | 后台任务（SVD 刷新等） |

调度策略：每次循环先彻底清空 `P0`，再依次给予 `P1`–`P4` 至多 `TIME_SLICE = 2 ms` 的时间片，一旦有新的 `P0` 到来立即抢占回 `P0`。在执行单步或控制流程期间，调用 `suspend_background()` 临时挂起 `P2`–`P4`（`P0`/`P1` 除外），避免后台读取争抢 USB 带宽拖慢单步。每次 DLL 调用耗时都会记录直方图 `Histogram`（通过 `session.stats` 查看）。

#### 5.2.4 连接后初始化

`probe_api::apply_post_connect` 在连接目标后按顺序执行：

| 操作 | 寄存器 | 目的 |
|---|---|---|
| 开启调试与跟踪时钟 | `DEMCR`（`0xE000_EDFC`）bit 24 `TRCENA` | 使能 DWT 与观察点硬件单元 |
| 使能周期计数器 | `DWT_CTRL`（`0xE000_1000`）bit 0 `CYCCNTENA` | 供周期计数与 RTT 时间戳使用 |
| 配置 Fault 向量捕获 | `DEMCR` 的 `VC_HARDERR` / `VC_BUSERR` / `VC_MMERR` / `VC_STATERR` / `VC_CHKERR` / `VC_NOCPERR` / `VC_INTERR` | 进入异常即停，实现 HardFault 拦截（结束调试时恢复清除） |
| 器件配置写寄存器 | 来自 `devices/<family>.yaml` 的 `postConnect` 与 `freeze` | 低功耗模式下保持调试连接（`DBGMCU_CR`），并按 `project.yaml` 的 `freezeOnHalt` 冻结看门狗/定时器 |

### 5.3 `symbols`：ELF 与 DWARF 处理

基于 `object`、`gimli`（DWARF 4/5）、`memmap2`、`rayon`、`bincode`、`blake3` 实现。

#### 5.3.1 加载流程

1. 使用 `memmap2` 只读映射 ELF 文件并计算 `blake3` 内容哈希。
2. 查询 `$HALTD_CACHE_DIR/index/<hash>.idx`（默认位置：Linux `$XDG_CACHE_HOME/haltd` 或 `~/.cache/haltd`，Windows 系统「文档」目录 `haltd`）。若存在且版本/魔数校验通过则直接 `bincode` 反序列化加载。
3. 缓存未命中时，使用 `rayon` **并行遍历全部 DWARF 编译单元（CU）**，仅扫描构建紧凑的名字与区间索引（全局变量、函数地址区间、规范化源文件到 CU 的映射、类型名称到 DIE 位置），然后通过临时文件 + 原子重命名写入缓存（Windows 下覆盖兼容）。
4. 具体的行号表、完整类型树、作用域树与 location list 在首次查询时按 CU 懒解析。

#### 5.3.2 能力清单

| 能力 | 实现要点 |
|---|---|
| 地址与行号互转 | 识别 `is_stmt` 语句行；路径统一归一化为 `/`（Windows 小写盘符），支持精确匹配与组件后缀匹配 |
| 类型系统 | 支持基元类型、指针、引用、多维数组、结构体/类/联合体、枚举、位域、`typedef`、cv 修饰符与 C++ 模板名解析 |
| 变量位置求值 | 解析 `DW_OP_addr`、`fbreg`、`breg*`、`reg*`、`piece`、`stack_value`、`implicit_value`、`entry_value`、`call_frame_cfa` 与 DWARF 5 `loclists`；寄存器变量由 `FrameMemory` 映射至合成地址区（`0xFFF0_0000+`）统一读写与拒写 |
| 内联作用域 | 通过 `DW_TAG_inlined_subroutine`（`abstract_origin`、`call_file`/`call_line`、`ranges`）构造由外到内的内联调用链 |
| CFI 栈回溯 | 使用 `gimli::UnwindSection` 解析 `.debug_frame`，支持 `EXC_RETURN` 异常边界穿透（§5.5.4） |
| 表达式引擎 | 优先级爬升解析 C/C++ 子集（`ns::name`、`.`、`->`、`[]`、`*`、`&`、标量强转、算术/位运算/比较/短路逻辑），内部采用 `i128` 运算避免隐式无符号截断 |

#### 5.3.3 ELF 热重载

`Session::reload` 与烧录并行构建新的 `SymbolDb`，构建完成后原子替换旧实例，随后重新检测 FreeRTOS，并依据表达式字符串与 `file:line` 重新绑定所有断点、数据断点、Live Watch 与 Timeline 信号。

### 5.4 针对 RelWithDebInfo（`-O2 -g`）的处理

| 优化代码场景 | Haltd 的处理策略 |
|---|---|
| 变量被优化掉或仅存在于部分 PC 区间 | 严格按当前 PC 求值 location list，无有效位置时如实报告 `<optimized out>`，不返回过期值 |
| 大量内联函数 | 调用栈生成虚拟内联帧；F11 位于内联入口时不推进硬件 PC 而直接逐层步入虚拟帧（§5.5.3） |
| 单行编译为多个分散地址 | 断点绑定到该行全部 `is_stmt` 起始地址，超出 FPB 上限时自动切换 Flash 断点 |
| 目标行被优化无代码 | 在同一函数内向后查找最多 10 行的最近有效 `is_stmt` 行并回传实际生效行号 |
| 省略帧指针（`-fomit-frame-pointer`） | 完全依托 `.debug_frame` CFI 规则恢复 CFA 与寄存器 |
| 指令乱序导致的单步来回跳 | 仅在 `is_stmt` 行号发生变化且处于同层或更外层 CFA 时停止 |
| 模板类型可读性差 | 通过 `visualizer` Lua 规则美化 `units`、`Eigen`、`std`、`etl` 等模板类型（§5.7） |

### 5.5 `engine`：调试引擎

#### 5.5.1 会话状态机

`Session` 的运行状态为 `RunState::{Running, Halted}`。DAP 主循环以 `POLL_INTERVAL = 20 ms` 周期调用 `Session::poll()` 读取 `DHCSR.S_HALT`；检测到停机后读取并写 1 清除 `DFSR` 以区分停机原因：

| `DFSR` 位 | 对应停机原因 |
|---|---|
| `VCATCH` | Fault 向量捕获 |
| `DWTTRAP` | DWT 数据断点命中 |
| `BKPT` | 硬件/软件/Flash 断点或单步临时断点 |
| `EXTERNAL` | 外部调试请求 |
| 其他（如 `HALTED`） | 手动暂停或指令单步 |

停机期间所有表达式求值与变量读取共享 `HaltedMemory`（按 `64 B` 分块缓存读取结果），大幅减少一次停机内展开多个变量的 USB 往返。

#### 5.5.2 断点管理

- **FPB 与 Flash 断点分配**：`BreakpointTable` 优先分配 FPB 硬件比较器，并固定为单步逃逸保留 `RESERVED_FOR_STEPPING = 2` 个比较器；超出部分根据 `flashBreakpoints`（`auto | never | always`）自动使用 Flash 断点（需要探针支持）。`setBreakpoints` 采用全量声明语义，未改变位置的行保留原硬件槽位不重复擦写 Flash。
- **条件断点、命中计数与日志点**：硬件命中停机后在 `filter_stop` 中求值。为控制延迟，判定只执行 1 帧浅层栈回溯（`shallow_unwind`）且运行态任务直接使用 CPU 当前寄存器；单次 `poll()` 设 `AUTO_RESUME_BURST = 15 ms` 突发窗口，超时交还事件循环以防止高频日志点饿死 DAP 请求。条件表达式解析/求值错误时会明确停下报错，避免静默漏停。
- **数据断点**：直写 DWT `COMP`/`MASK`/`FUNCTION` 寄存器，支持读/写/读写触发与条件表达式，要求长度满足 2 的幂且自身对齐；命中后读取并缓存清除 `DWT_FUNCTION.MATCHED`。

#### 5.5.3 单步算法（适配优化代码）

1. **内联步入（Step Into）**：若当前 PC 命中尚未展开的虚拟内联链首层，隐藏/展开计数器直接调整而不执行任何目标指令。
2. **指令循环与子函数跨越**：逐条指令单步（单步期间默认设置 `DHCSR.C_MASKINTS` 屏蔽中断），使用 CFI 追踪 `CFA` 与 `PC`。进入子函数时（`CFA` 变深或 `PC` 落在另一函数入口，含尾调用），Step Over 会用临时硬件断点运行到返回地址（校验 CFA、任务与异常上下文一致）；Step Into 在目标函数有行号信息时停下，无行号信息则自动步出。
3. **长循环与脱离事件循环（`DetachedRun`）**：当同一行指令步数达到 `SINGLE_STEPS_BEFORE_ESCAPE = 64` 时，改在函数内其余行及返回地址上放置临时 FPB 断点加速逃逸（超过 `SINGLE_STEPS_BEFORE_FLASH_ESCAPE = 2048` 才允许使用 Flash 临时断点，硬上限 `MAX_SINGLE_STEPS = 20_000`，从而避免普通单步产生 Flash 磨损）。若自由运行等待超过 `DETACH_AFTER = 500 ms`（如永不返回的函数），`run_to` 转为 `DetachedRun` 并返回 `StopReason::StillRunning`，交回 DAP 事件循环，后续由 `poll()` 捕获完成或允许用户随时 `pause`。

#### 5.5.4 栈回溯（Cortex-M）

- 使用 `r0–r15`、`xPSR`、`MSP`、`PSP`、`CONTROL`、`FPSCR`、`s0–s31` 与 `.debug_frame` CFI 逐帧回溯（最大深度 `MAX_FRAMES = 64`）。
- 遇到返回地址落在 `0xFFFF_FF00..0xFFFF_FFFF`（`EXC_RETURN`）时，依据 bit 2 选择 `MSP`/`PSP`，依据 bit 4 判断基本帧（8 字）或浮点扩展帧（+18 字），并检查硬件压栈 `xPSR` bit 9 的 4 字节栈对齐填充，恢复被中断现场后向调用栈插入 `<exception N>` 分隔帧继续回溯。

#### 5.5.5 故障分析（F15）

触发向量捕获停机后，`engine/fault` 读取 `CFSR`、`HFSR`、`MMFAR`、`BFAR`、`AFSR`，将状态位译码为可读诊断文本（仅在 `MMARVALID` / `BFARVALID` 置位时报告地址，对 `IMPRECISERR` 附加不精确位置警告），跨过 `<exception N>` 定位出错源码行并将处理函数帧标为 `deemphasize`，同时下发 `haltd/fault` 事件。

#### 5.5.6 FreeRTOS 任务感知（F17）

- 通过 ELF 符号 `pxCurrentTCB` 与 `pxReadyTasksLists` 自动启用，所有 `TCB`、`List_t` 字段偏移从 DWARF 读取，遍历 Ready / Delayed1 / Delayed2 / PendingReady / Suspended / WaitingTermination 链表。
- 按 `ARM_CM3`（无 FPU）或 `ARM_CM4F`/`ARM_CM7`（带 FPU）上下文布局从 `pxTopOfStack` 恢复非运行态任务寄存器供单独查看调用栈，并从 `pxStack` 扫描连续 `0xA5` 填充字节计算剩余栈空间。运行中请求任务列表时返回最近一次稳态快照。

### 5.6 `sampler`：Live Watch 与 Timeline 采样

#### 5.6.1 添加信号与模式选择

用户输入或点选 C/C++ 表达式后，通过 `symbols` 与 `visualizer` 解析为叶子数值标量（结构体与数组自动递归展开至上限）：
- `auto`：优先调度进 HSS，超出带宽或块数预算的信号自动降级至轮询。
- `hss` / `poll`：强制指定通道。

#### 5.6.2 HSS 调度与时钟同步

- **块合并与带宽预算**：按地址排序并将间隔 `≤ HSS_MERGE_GAP (32 B)` 的信号合并为同一个 `HssBlock`；受限于 `max_blocks`（J-Link V9 为 10）与单记录字节预算 `Σ(bytes + 8) ≤ BUDGET`（4 MHz SWD / 1 kHz 下基准为 `150 B`，随 `speed_khz` 与周期按比例缩放）。超预算时按 `(priority, 添加顺序)` 剔除信号。
- **周期读取与时钟同步（`timesync::ProbeClock`）**：以 `10 ms` 周期（`P1`）调用 `hss_read`。由于探针微秒计数器在核心暂停时冻结、与主机存在速率漂移（V9 实测约 `+0.37%`）且 USB 投递延迟在 `0–11 ms` 间抖动，`ProbeClock` 采用：
  - 连续 `QUIET_READS = 10`（100 ms）空读视为目标暂停，下一批数据重新锚定时间基线；
  - 滑动窗口（`32 → 256` 批）取观测最小偏移分离投递抖动，并自适应估计探针与主机的时钟比率；
  - 扣除实测投递延迟常量 `HSS_DELIVERY_LATENCY_NS = 3 ms`；
  - 在 `SETTLING_RECORDS = 32` 条记录学习真实周期后，将间距 `≥ 1.75` 个周期记为丢点。

#### 5.6.3 轮询采样与 Live Watch

- **轮询采样**：将非 HSS 信号按 `POLL_MERGE_GAP = 64 B` 合并读取区间，在 `P3` 周期读取（上限 `MAX_POLL_HZ = 100 Hz`），取读前读后主机单调时间中点作为样本时间戳。
- **Live Watch**：独立线程按 `TICK = 25 ms` 调度每个根监视项（支持 `0–5 Hz` 独立设置，`0` 为仅停机刷新），按树展开状态仅读取可见子成员；通过 `watch.write` 支持标量直接写入及位段读改写。

### 5.7 `visualizer`：类型美化显示

内嵌 Lua 5.4（`mlua` vendored），启动时加载 `prelude.lua` 与 `builtin.lua`（并可追加加载 `.haltd/visualizers.lua`），全局入口函数为 `__haltd_apply(type_name, handle, resolving)`。Lua 脚本仅通过整数句柄向 Rust 侧请求成员、数组元素、内存标量与指针解引用，无法直接越权访问进程或硬件；内置规则覆盖 `units::unit_t`、`Eigen::Matrix`/`Quaternion`、`std::{array,optional,variant,span,string_view,tuple}`、`etl::{vector,ivector,array,string}`。规则同时作用于 DAP Variables 面板、Live Watch 展开与表达式虚拟成员路径解析（`VirtualMembers`）。

### 5.8 `rtt`：RTT 引擎

- **控制块发现与复位处理**：当 ELF 包含 `_SEGGER_RTT` 符号或工作区存在 `.haltd/rtt.yaml` 时启用 RTT（无信号与配置时不扫描 RAM，防止占用 SWD 带宽饿死 HSS）。针对 J-Link 在目标复位后可能返回旧通道数且忽略重复 `START` 的特性，复位时会擦除 RAM 中的旧 `"SEGGER RTT"` 魔数并使用 `SETADDR` 显式重设控制块地址。
- **读取与解码分离**：探针侧 `P2` 每 `5 ms` 读取上行通道并送入独立解码线程，避免解码阻塞探针。支持 5 种显式解码模式与 2 种零配置自动解码（`rtt.autoDecode` / `rtt.autoTimeline`）：
  - 显式配置（`.haltd/rtt.yaml`）：`text`、`text-regex`、`struct`（按 ELF DWARF 布局对齐同步字 + 可选时间戳字段对齐，结构体改动免改配置）、`lua`（`decode(bytes, state)` 状态机）、`systemview`（原始数据直写 `.SVDat`）。
  - 零配置自动解码：通道名匹配 `Type` / `struct:Type` / `Type@0xSync` 且存在 ELF 结构体定义时走 `auto-struct`；其余未配置文本通道走 `auto-text`（保留完整终端输出同时提取 `key=value` 数值，每通道最多追踪 32 个去重信号名）。

### 5.9 `tsdb`：时间序列存储

- **环形缓冲与 LOD 金字塔**（`crates/tsdb/src/ring.rs`）：每路信号默认容量 `DEFAULT_CAPACITY = 600_000` 点（1 kHz 下 10 分钟），同时维护 `9` 级 4 倍粗化（`shift = 2, 4, ..., 18`）的 min/max 金字塔。前端按请求像素宽度 `widthPx` 直接在对应层级查询每像素列的最值，保证任意时间跨度返回点数 `≤ 2 × widthPx`。
- **事件轨道**：记录 `Halted`、`Resumed`、`Reset`、`Reload`、`SamplingChanged`、`Fault` 六类事件，与波形共享纳秒时间轴。
- **录制与导出**（`crates/tsdb/src/record.rs`）：后台写线程每 `200 ms` 批量将新元数据、样本块与事件流式写入 `.hlrec` 文件，不阻塞采样线程；支持按时间范围导出多信号对齐的 CSV。

### 5.10 `flash`：烧录与热重载

- **增量烧录**（`crates/probe-jlink/src/flash.rs`）：烧录前先暂停核心，启用 `SetSkipProgOnCRCMatch = 1` 并调用 `JLINK_DownloadFile` 仅擦写内容发生变化的 Flash 扇区，通过全局回调函数透传烧录进度。
- **Build & Reload 流程**（`Session::reload`）：
  1. 暂停目标并清理 `DetachedRun` 状态；
  2. 先移除全部活动断点（避免残留软件断点指令干扰 Flash 扇区内容比对）；
  3. 使用 `std::thread::scope` **并行**执行探针烧录与重建 `SymbolDb`；
  4. 烧录完成后复位核心并重新检测 FreeRTOS；
  5. 重新绑定源码/函数/表达式数据断点、更新客户端断点状态，发送 `haltd/reloaded` 事件并插入 `Reload` 事件，随后恢复运行（或按 `runToMain` 停在 `main`）并重新绑定 `Sampler` / `Rtt`。

---

## 6. 对外协议

### 6.1 DAP 支持范围

基于自定义 serde 消息实现常用 DAP 子集（`crates/server/src/dap.rs`）：

- **生命周期与配置**：`initialize`、`launch`、`attach`、`configurationDone`、`restart`、`terminate`、`disconnect`
- **断点**：`setBreakpoints`（条件、命中次数、日志点）、`setFunctionBreakpoints`、`setExceptionBreakpoints`（`hardfault` / `busfault` / `memmanage` / `usagefault`）、`dataBreakpointInfo`、`setDataBreakpoints`、`exceptionInfo`
- **执行控制**：`continue`、`pause`、`next`、`stepIn`、`stepOut`
- **状态与内存**：`threads`（FreeRTOS 任务映射）、`stackTrace`、`scopes`（Locals / Registers / Globals）、`variables`、`setVariable`、`evaluate`、`readMemory`、`writeMemory`
- **不支持**：`disassemble`、指令粒度 `steppingGranularity`

**自定义请求**：`haltd/reload`、`haltd/faultInfo`、`haltd/rtosTasks`、`haltd/cycleCounter`、`haltd/peripherals`、`haltd/peripheralRead`、`haltd/peripheralWrite`。

**自定义事件**：`haltd/bound`、`haltd/reloaded`、`haltd/fault`、`haltd/cycles`。

### 6.2 数据通道（HTTP / WebSocket `127.0.0.1:47110`）

- **HTTP 页面与静态资源**（`crates/server/src/http.rs`）：`/` 与 `/timeline` 提供 Timeline 页面，`/rtt` 提供 RTT 面板，`/ws` 升级 WebSocket。
- **JSON-RPC 2.0 控制方法**（文本帧，`crates/server/src/rpc.rs`）：
  - 会话：`session.status`、`session.stats`
  - Live Watch：`watch.add`、`watch.remove`、`watch.list`、`watch.write`、`watch.expand`、`watch.collapse`、`watch.setRate`
  - Timeline：`timeline.add`（支持 `expr` 或 `rttSignal`）、`timeline.remove`、`timeline.list`、`timeline.update`、`timeline.rate`、`timeline.setRate`、`timeline.query`（返回 `kind=4` 二进制帧）、`timeline.subscribe`、`timeline.unsubscribe`、`events.query`
  - RTT：`rtt.status`、`rtt.channels`、`rtt.signals`、`rtt.subscribe`、`rtt.unsubscribe`、`rtt.write`
  - 录制与导出：`record.start`（默认输出 `<cache>/recordings/<ts>.hlrec`）、`record.stop`、`record.export`（CSV）
  - 运行控制：`target.halt`、`target.go`、`target.reset`、`flash.reload`
- **二进制帧格式**（小端序，`crates/server/src/ws.rs` / `rpc.rs`）：
  - 通用 4 字节头：`[kind: u8][version: u8 = 1][count_or_reserved: u16]`
  - `kind=1`（实时采样推送，每 33 ms）：重复 `count` 个块，每块 `[seriesId: u32][n: u32][t0_ns: i64][dt_ns: u32 = 0][ts: i64 * n][vals: f64 * n]`
  - `kind=2`（RTT 终端字节流）：`[ch: u8][len: u32][data: u8 * len]`
  - `kind=3`（事件通知）：`[t_ns: i64][event_kind: u16][len: u16][detail_json: u8 * len]`
  - `kind=4`（`timeline.query` 降采样返回）：`[requestId: u32][widthPx: u32][reserved: u32][t0_ns: i64][t1_ns: i64]` 后跟每路 `[seriesId: u32][reserved: u32][min: f64 * widthPx][max: f64 * widthPx]`

---

## 7. 配置文件

### 7.1 VS Code `launch.json`

```jsonc
{
  "type": "haltd",
  "request": "launch",                 // 或 "attach"
  "name": "Debug (J-Link)",
  "device": "STM32H723VG",
  "interface": "SWD",                  // SWD | JTAG
  "speed": 4000,                       // kHz 或 "auto"
  "elf": "${workspaceFolder}/build/firmware.elf",
  "flash": "changed",                  // always | changed | never
  "runToMain": false,
  "stopOnEntry": false,
  "preLaunchTask": "build",
  "project": "${workspaceFolder}/.haltd/project.yaml",
  "svd": "path/to/device.svd",         // 省略时尝试从已安装 Ozone 目录匹配
  "dll": "",                           // 可选覆盖 J-Link 动态库路径
  "serial": null,                      // 多探针序列号
  "rtt": {
    "autoDecode": true,
    "autoTimeline": true
  }
}
```

### 7.2 `.haltd/project.yaml`

```yaml
target:
  family: stm32h7                      # 可选，缺省由 device 推导（stm32f1/stm32g4/stm32h7/stm32h74）

debug:
  maskInterruptsOnStep: true
  vectorCatch: true
  freezeOnHalt: [iwdg1, wwdg1]         # 寄存器位名定义在 devices/<family>.yaml
  flashBreakpoints: auto               # auto | never | always

sampling:
  hss: { periodUs: 1000 }
  watchRateHz: 5

svd: path/to/device.svd
visualizers:
  - .haltd/visualizers.lua
rtt:
  config: .haltd/rtt.yaml
  autoDecode: true
  autoTimeline: true
```

### 7.3 `.haltd/rtt.yaml`

```yaml
controlBlock: auto                     # auto 取 ELF 中 _SEGGER_RTT 地址，或写十六进制地址
channels:
  0: { name: log, type: text, ansi: true }
  1: { name: sysview, type: systemview, file: build/trace.SVDat }
  2:
    name: telemetry
    type: struct
    sync: 0xA55A
    struct: haltd::TelemetryFrame      # ELF 中定义的结构体/类/联合体类型名
    timestamp: { field: cyccnt, clockHz: 540000000 }
    timeline:
      - jointPos[0..5]
      - { expr: loopUs, group: timing }
  3: { name: motor, type: lua, script: .haltd/decode_motor.lua }
  4: { name: metrics, type: text-regex, pattern: 'err=(?P<err>-?\d+)' }
```

---

## 8. VS Code 扩展

位于 `vscode-ext/`（TypeScript + esbuild）：

| 组件 | 实现内容 |
|---|---|
| 调试适配器注册 | 注册 `type: "haltd"` 的 `DebugAdapterDescriptorFactory`（调用 `haltd dap`）与默认 `.haltd/project.yaml` 注入 |
| 器件选型辅助（`deviceAssist.ts`） | 在 `launch.json` 中提供 J-Link 器件补全、内核/Flash/RAM 与内置 profile 悬停信息、未知型号警告与 `Haltd: Select Device`（优先调用 `haltd devices --json`，离线回退内置 `devices-stm32.json`） |
| Live Watch（`liveWatch.ts`） | 调试侧栏 `haltd.liveWatch` TreeView，支持折叠/展开、改值、独立刷新率（0–5 Hz）与 `.haltd/state.json` 持久化；会话期间接管原生 Add to Watch 并提供 `Ctrl+G` / `Ctrl+H` 等快捷键 |
| Timeline 与 RTT 面板 | `BackendPanel` 通过 Webview iframe 嵌入 `127.0.0.1:47110/timeline` 与 `/rtt`，通过 `postMessage`（`haltd:vscode-theme` / `haltd:request-theme` / `haltd:host-theme`）同步 VS Code 主题配色与 CSS 变量；RTT 通道 0 同时提供 VS Code `Pseudoterminal` |
| Peripherals & FreeRTOS 视图 | `debugViews.ts` 通过 `haltd/peripherals`、`haltd/peripheralRead`、`haltd/peripheralWrite` 与 `haltd/rtosTasks` 渲染视图，停机自动刷新并对读副作用寄存器按需读取 |
| 状态栏与 Build & Reload | 显示探针连接、SWD 速率、HSS 采样率与丢点计数、`DWT_CYCCNT` 周期差；`F6` 触发构建并调用 `haltd/reload` |

---

## 9. 其他编辑器集成

由于后端把 DAP 服务独立暴露在 stdio 桥接命令 `haltd dap` 上，且 Timeline 与 RTT 界面本身通过内置 HTTP/WebSocket 服务器提供，任何支持标准 Debug Adapter Protocol 的编辑器（例如 Neovim 的 `nvim-dap`）均可直接配置执行 `haltd dap` 接入核心源码级调试能力，并在外部浏览器打开 `http://127.0.0.1:47110/timeline` 与 `http://127.0.0.1:47110/rtt` 使用波形与 RTT 面板。

---

## 10. 固件侧约定

Haltd 尽量做到对固件零侵入（FreeRTOS 感知与符号解析均只读取 ELF DWARF），仅在启用部分可选能力时存在如下轻量约定：

1. **RTT 阻塞模式**：用于高速二进制遥测或高频日志的上行通道建议使用 `SEGGER_RTT_MODE_NO_BLOCK_SKIP`（无同步字按结构体定长切分时必须使用该模式，避免截断半帧与阻塞控制循环）。
2. **零配置结构体通道命名**：调用 `SEGGER_RTT_ConfigUpBuffer` 时将通道名设为 `<Type>`、`struct:<Type>` 或 `<Type>@0x<SyncHex>`（例如 `TelemetryFrame@0xA55A`），且保证该结构体类型被引用并保留在 DWARF 中（例如使用 `[[gnu::used]]` 定义锚点变量）。
3. **D-Cache 与 RTT 缓冲区**：若 MCU 开启 D-Cache，建议通过 MPU 将 RTT 缓冲区所在内存区配置为非缓存（Uncacheable），以免调试器读取到旧缓存行。
4. **HSS / Live Watch 变量范围**：AHB-AP 与 HSS 直接读取目标 RAM 地址，因此仅支持全局变量、静态变量或可通过全局变量指针解引用的对象，纯寄存器局部变量无法在运行期采样。

---

## 11. 性能目标与预算

| 指标 | 设计目标 / 基准 | 说明 |
|---|---|---|
| 符号首次加载（百 MB 级 ELF） | ≤ 3 s（24 线程典型实测约 180 ms） | 并行构建名称索引 + 懒解析（§5.3.1） |
| 符号命中缓存加载 | ≤ 300 ms（典型实测约 20 ms） | `blake3` 索引缓存 |
| 断点检测到跳转响应 | ≤ 100 ms | `20 ms` 暂停轮询 + 缓存读与栈回溯 |
| 4 MHz SWD 单帧 HSS 预算 | `字节数 + 8 × 块数 ≤ 150 B`（1 kHz） | 由 `jlbench` 实测拟合，随 SWD 速率与采样周期缩放 |
| HSS 持续采样 | 1 kHz 零丢点 | 超预算信号自动降级至 ≤ 100 Hz 轮询 |
| Live Watch | 0–5 Hz 独立可调 | 仅读取展开后的可见节点 |
| Build & Reload（少量改动） | ≤ 5 s（不含编译） | 增量扇区擦写 + 并行符号重建 |
| Timeline queries | 60 fps 流畅缩放（60 万点 × 多通道） | TSDB 9 级 min/max 金字塔降采样 + uPlot 渲染 |

硬件基准测量由仓库内置的 `bins/jlbench` 工具负责（支持只读安全测速与带 `--allow-halt` / `--allow-flash` 的完整探针测量，详见 [J-Link 实测特性与注意事项](jlink-notes.md)）。

---

## 12. 仓库结构

```
.
├─ Cargo.toml                     # Rust workspace（MIT）
├─ bins/
│  ├─ haltd/                      # 主程序（info | flash | read | tasks | serve | dap | symbols | devices）
│  └─ jlbench/                    # J-Link 与目标硬件基准测量工具
├─ crates/
│  ├─ probe-api/                  # Probe / HssApi / RttApi / SwoApi trait 与器件 profile 加载
│  ├─ probe-jlink/                # J-Link DLL 动态加载、ProbeActor 优先队列、Flash/HSS/RTT 实现
│  ├─ probe-mock/                 # 内存仿真探针与会话录制/回放
│  ├─ symbols/                    # ELF mmap、blake3 缓存、DWARF 5 懒解析、CFI 回溯、表达式求值
│  ├─ visualizer/                 # Lua 5.4 运行时与内置 C++ 类型美化规则
│  ├─ engine/                     # Session、断点/DWT、-O2 单步、Fault 译码、FreeRTOS 感知
│  ├─ sampler/                    # HSS 块规划、轮询合并、ProbeClock 时钟同步、Live Watch
│  ├─ rtt/                        # RTT 发现与解码（auto-struct/auto-text/text/regex/struct/lua/sysview）
│  ├─ tsdb/                       # 环形缓冲、min/max 金字塔 LOD、事件轨道、.hlrec 录制与 CSV 导出
│  ├─ svd/                        # CMSIS-SVD 解析与读副作用寄存器屏蔽
│  └─ server/                     # DAP 服务、stdio↔TCP 桥接、ControlBus、HTTP/WS 与 JSON-RPC
├─ devices/                       # 内置 MCU 配置（stm32f1 / stm32g4 / stm32h7 / stm32h74.yaml）
├─ webui/                         # Vite + TypeScript + uPlot（timeline/ 模块化轨道 UI 与 rtt.html）
├─ vscode-ext/                    # VS Code 扩展（DAP 客户端、deviceAssist、Live Watch、Views、Webview）
├─ scripts/                       # 打包脚本（release-windows.ps1）
├─ assets/                        # icon.svg、logo.svg
└─ docs/                          # DESIGN.md、usage.md、jlink-notes.md
```

---

## 13. 测试策略

| 层级 | 覆盖范围与方式 |
|---|---|
| Rust 单元测试与集成测试 | `cargo test --workspace`：基于 `probe-mock`（含内存镜像仿真与真实会话录制回放 `RecordingProbe` / `ReplayProbe`）覆盖引擎、采样规划、时钟同步、TSDB、`.hlrec`/`.jlrec` 编解码、SVD、RTT 与服务器协议 |
| 真实 ARM GCC DWARF 夹具测试 | `crates/symbols/tests/fixture.rs`、`crates/engine/tests/freertos.rs`、`crates/rtt/tests/auto_elf.rs`：检测到 `arm-none-eabi-gcc`/`g++` 时在 `build.rs` 中以 `-O2 -g` 编译真实 ELF 验证符号、CFI 异常回溯、FreeRTOS 布局与 RTT 结构体自动解码（无工具链时跳过） |
| 大型真实 ELF 压测（按需） | 通过环境变量 `HALTD_TEST_ELF` / `HALTD_TEST_NAMES` / `HALTD_TEST_EXPR` 运行 `symbols` 与 `visualizer` 的 `#[ignore]` 测试 |
| 前端单元测试与类型检查 | `webui/` 下执行 `tsc --noEmit && vite build` 与 `vitest run`（覆盖 Timeline 坐标换算与快捷键映射）；`vscode-ext/` 执行 `tsc --noEmit` 与 `esbuild` 打包 |

---

## 14. 附录

### 14.1 Cortex-M 调试寄存器速查

| 寄存器 | 地址 | 用途 |
|---|---|---|
| `DHCSR` | `0xE000_EDF0` | `C_DEBUGEN`、`C_HALT`、`C_MASKINTS`、`S_HALT` |
| `DEMCR` | `0xE000_EDFC` | `TRCENA`（bit 24）、Fault 向量捕获 `VC_*` |
| `DFSR` | `0xE000_ED30` | 停机原因（`BKPT` / `DWTTRAP` / `VCATCH` / `EXTERNAL` / `HALTED`），写 1 清除 |
| `CFSR` / `HFSR` | `0xE000_ED28` / `2C` | Fault 状态寄存器 |
| `MMFAR` / `BFAR` / `AFSR` | `0xE000_ED34` / `38` / `3C` | Fault 触发地址与辅助状态 |
| `DWT_CTRL` / `DWT_CYCCNT` | `0xE000_1000` / `04` | DWT 控制与 32 位周期计数器 |
| `DWT_COMPn` / `MASKn` / `FUNCTIONn` | `0xE000_1020 + 16n` | DWT 数据断点比较器 |

### 14.2 `.hlrec` 二进制录制格式

`.hlrec` 为追加写入的流式二进制格式（`crates/tsdb/src/record.rs`）：

- **8 字节文件头魔数**：`HLREC\x001\n`（读取侧同时兼容旧版魔数 `JLREC\x001\n`）。
- **变长帧序列**：每帧为 `[tag: u8][length: u32 LE][payload: u8 * length]`，未知 `tag` 自动跳过以保留前向兼容性：
  - `TAG_SERIES (1)`：JSON 序列化的 `SeriesMeta`（`id`、`name`、`group`、`color`）。
  - `TAG_SAMPLES (2)`：`[series_id: u32 LE][count: u32 LE][t_ns: i64 LE * count][values: f64 LE * count]`。
  - `TAG_EVENT (3)`：JSON 序列化的 `Event`（`seq`、`t`、`kind`、`detail`）。

### 14.3 名词表

| 名词 | 含义 |
|---|---|
| **HSS** | High Speed Sampling，由 J-Link 固件按固定周期主动采集目标内存块并批量回传主机 |
| **RTT** | Real Time Transfer，目标 RAM 中的环形缓冲区通信机制 |
| **FPB** | Flash Patch and Breakpoint，Cortex-M 硬件断点比较器单元 |
| **DWT** | Data Watchpoint and Trace，提供数据断点比较器与 `CYCCNT` 周期计数器 |
| **LOD** | Level of Detail，按屏幕像素列密度返回 min/max 金字塔降采样结果 |
| **CFA** | Canonical Frame Address，DWARF CFI 栈回溯中的帧基准地址 |

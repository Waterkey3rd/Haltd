<p align="center">
  <img src="assets/logo.svg" alt="jlinkd" width="460" />
</p>

<p align="center">
  <strong>中文</strong> | <a href="README.en.md">English</a>
</p>

# jlinkd

面向 STM32（ARM Cortex-M）的 J-Link 调试后端、VS Code 插件与命令行工具，采用「常驻后台进程 + DAP + WebSocket 数据通道」架构。

## 写在前面

这个项目是作为 **Ozone 的一种开源替代** 开发的，采用「常驻后台进程 + DAP + WebSocket 数据通道」架构，把大工程符号缓存、`-O2` 单步、HardFault 定位、FreeRTOS 感知、1 kHz 高速波形与 RTT 结构化解码统一放进 VS Code 与命令行工作流中，主要为了解决平时调试 STM32 时遇到的一些痛点：

- 工程开启 `-O2` 优化后，常规调试器经常单步乱跳、局部变量看不准或内联栈帧断掉；
- 调试控制算法/电机/实时系统时，既想打断点、看 HardFault 和 FreeRTOS 任务栈，又不想停机，希望能直接在 VS Code / 浏览器里一边跑一边看 Live Watch、1 kHz 高速波形（Timeline）和 RTT 日志；
- 大一点的 ELF 每次启动调试都要重新加载半天，来回改代码烧录（Build & Reload）很折磨；
- 在 Linux 和 Windows 两边切换开发时，希望能有一套一致、轻量、开源且可控的工具链。

目前日常高频使用的核心调试能力都已经打通，日常开发可以作为主力调试环境，也可以按需与 Ozone 搭配使用。项目以开源交流与分享为主（非商业软件）；受限于个人手头的硬件环境，没有的芯片和探针型号没办法逐一实测——欢迎有兴趣的朋友一起测试、反馈问题或提 PR 补充器件配置与支持！

---

## 硬件与平台验证现状

| 项目 | 当前状态 | 说明 |
|---|---|---|
| **操作系统** | Linux x64 / Windows x64 | 均已支持并实测；Linux 下继续保留零开销路径与 mmap，Windows 走独立条件编译适配 |
| **J-Link 探针** | J-Link V9 实测通过 | 其余型号理论上兼容，欢迎实测反馈 |
| **STM32F1（F103C8）** | 已实测通过 | 调试、HSS、RTT、Fault、FreeRTOS 全流程验证 |
| **STM32G4 系列** | 已实测通过 | 内置 [devices/stm32g4.yaml](devices/stm32g4.yaml) |
| **STM32H72x / H73x（H723）** | 已实测通过 | 内置 [devices/stm32h7.yaml](devices/stm32h7.yaml) |
| **STM32H74x / H75x** | 配置已内置，待充分实测 | 已根据手册编写 [devices/stm32h74.yaml](devices/stm32h74.yaml)，缺少对应实机全面验证 |
| **其他 Cortex-M / STM32** | 可基础连接 | 可正常连探针与调试，暂未内置 `DBGMCU` 低功耗调试与 `freezeOnHalt` 外设冻结配置 |

> 如果你想验证自己手头的板子或探针，仓库里自带基准工具 `jlbench`（见 [J-Link 实测特性与注意事项](docs/jlink-notes.md)），也欢迎提 PR 补充 `devices/*.yaml`。

---

## 核心功能

- **源码级调试（DAP）**：针对 `-O2` 优化代码做了专门的单步与内联虚拟栈帧处理；支持条件断点、命中次数、日志点、DWT 硬件数据断点，以及 `F6` 一键 Build & Reload（仅擦写变化扇区并自动重绑断点）。
- **HardFault 自动拦截与译码**：发生异常时在入口第一时间暂停，自动解析 `CFSR` / `HFSR` / `MMFAR` / `BFAR` / `AFSR`，并在调用栈中跨过 `<exception N>` 直接定位到出错代码行。
- **FreeRTOS 感知**：直接从 DWARF 信息读取 TCB 与任务链表布局（支持 CM3 / CM4F），无需硬编码偏移，可查看任务状态、优先级、剩余栈高水位及各任务独立调用栈。
- **不停机变量与波形采样**：
  - **Live Watch**：通过 AHB-AP 不停机实时读写全局变量、结构体与位段（0~5 Hz 可调）。
  - **Timeline（HSS / 轮询）**：最高 1 kHz 高速采样（超出探针 HSS 带宽预算的信号自动降级为轮询），内嵌浏览器 / VS Code 波形面板，支持游标测量、分组子图、`.jlrec` 录制与 CSV/PNG 导出。
- **RTT 终端与结构化解码**：支持文本终端、正则提取、按 ELF DWARF 结构体自动解析的二进制帧、Lua 自定义解码脚本与 SystemView 透传。
- **CMSIS-SVD 外设寄存器视图**：支持自动屏蔽带读副作用的寄存器（如 `DR` / `RDR` / `RXDR`），防止打开视图就把串口/SPI 数据读丢，并支持按位段读改写。
- **C++ 类型美化（Visualizer）**：内置支持 `units`、`Eigen::Matrix` / `Quaternion`、`std::array` / `std::tuple` / `std::optional` / `std::variant` / `std::span` / `std::string_view` 等类型，支持通过 Lua 自定义项目规则。

---

## 快速安装与使用

### 方式一：直接使用发布包（推荐）

从 Release 获取对应平台的包：
- **Linux x64**：`jlinkd-<版本>-linux-x64.vsix` + `jlinkd` 二进制
- **Windows x64**：`jlinkd-<版本>-win32-x64.vsix` + `jlinkd.exe` 二进制

前置只需安装对应系统的 **SEGGER J-Link 驱动** 与 **VS Code（≥ 1.90）**，在 VS Code 中安装 `.vsix`（插件已内置后端可执行文件），无需本机装 Rust 或 Node.js。

详细配置、快捷键、`launch.json` 与 `.jlinkd/*.yaml` 说明见 **[使用手册](docs/usage.md)**。

### 方式二：从源码构建

构建依赖：Rust 工具链（`rust-version = 1.85`）、Node.js / npm（`webui/` 内嵌前端与 `vscode-ext/` 需要），运行夹具单元测试还需 `arm-none-eabi-gcc`（没有时相关测试自动跳过）。

**Linux x64**：

```bash
# 1. 先构建 Web UI（server 会在编译期把 webui/dist 内嵌进二进制）
cd webui && npm ci && npm run build && cd ..

# 2. 构建并安装 jlinkd 到 ~/.cargo/bin
cargo install --path bins/jlinkd --locked

# 3. 构建 VS Code 扩展（可选）
cd vscode-ext && npm ci && npm run build && cd ..
```

**Windows x64（PowerShell）**：

```powershell
# 需要 Rust (MSVC) 与 Node.js，一键打包到 release/
powershell -ExecutionPolicy Bypass -File scripts\release-windows.ps1
```

---

## 文档索引

- **[使用手册](docs/usage.md)**：安装、VS Code 调试配置、Live Watch / Timeline、RTT 解码、FreeRTOS / SVD、CLI 命令与常见问题（覆盖 Linux 与 Windows）。
- **[J-Link 实测特性与注意事项](docs/jlink-notes.md)**：开发过程中实测出来的 J-Link 行为与坑（SWD 分频、HSS 超预算降频、`JLINKARM_SetWP` 清空 `DEMCR`、RTT 复位残留、Windows 代码页等），以及 `jlbench` 实测方法。
- **[开源许可证](LICENSE)**：项目本身采用 MIT License；运行时通过动态加载调用用户本机安装的 SEGGER J-Link 驱动库（`libjlinkarm.so` / `JLink_x64.dll`），使用 J-Link 驱动需遵守 SEGGER 对应的许可协议。

---

## 开发与测试

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

绝大多数单元测试基于内置的 `probe-mock` 运行，无需连接真实硬件即可验证。

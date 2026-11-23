<p align="center">
  <img src="assets/logo.svg" alt="jlinkd" width="460" />
</p>

# jlinkd

面向 STM32（ARM Cortex-M）的 J-Link 调试后端与前端工具集，采用「常驻后台进程 + DAP + WebSocket 数据通道」架构，兼顾常规源码调试、不停机实时观测与高频采样。

> 架构设计与实现细节见 [DESIGN.md](DESIGN.md) 与 [docs/technical-implementation.md](docs/technical-implementation.md)。

## 核心功能

- **源码级调试（DAP）**：支持 `-O2` 优化代码单步、内联栈帧展开、条件断点 / 命中次数 / 日志点、DWT 硬件数据断点、Build & Reload（`F6`）。
- **HardFault 自动定位与译码**：异常发生时在入口处拦截，自动读取并解释 `CFSR` / `HFSR` / `MMFAR` / `BFAR` / `AFSR`，调用栈直接回溯跨过 `<exception N>` 指向出错代码行。
- **FreeRTOS 感知**：通过 DWARF 自动解析 TCB 与任务链表布局（支持 CM3 / CM4F），查看任务状态、优先级、剩余栈空间及各任务独立调用栈。
- **不停机变量与波形采样**：
  - **Live Watch**：通过 AHB-AP 实时读写全局变量 / 结构体成员（支持 0~5 Hz）。
  - **Timeline（HSS / 轮询）**：最高 1 kHz 高速采样（HSS 超预算自动转轮询），内嵌浏览器波形界面（`http://127.0.0.1:47110/timeline`），支持 `.jlrec` 录制与 CSV 导出。
- **RTT 终端与结构化解码**：支持文本、正则提取、DWARF 结构体二进制帧、Lua 解码脚本与 SystemView 透传。
- **CMSIS-SVD 外设寄存器视图**：支持自动屏蔽带读副作用的寄存器（如 `DR` / `RDR` / `RXDR`），并支持按字段读改写。
- **C++ 类型美化（Visualizer）**：内置支持 `units`、`Eigen::Matrix` 等类型显示，并可通过 Lua 自定义规则。

---

## 环境要求

- **操作系统**：Linux x64 / Windows x64（Windows 使用说明见 [docs/windows-usage.md](docs/windows-usage.md)）
- **SEGGER J-Link 驱动**：提供 `libjlinkarm.so`（Linux 默认查找 `/opt/SEGGER/JLink/libjlinkarm.so`）或 `JLink_x64.dll`（Windows 默认从注册表查找），也可通过 `--dll`、`JLINKD_DLL` 或 `launch.json` 指定
- **源码构建依赖**：
  - Rust 工具链（`rust-version = 1.85`）
  - Node.js / npm（构建 `webui/` 与 `vscode-ext/` 时需要）
  - `arm-none-eabi-gcc`（可选，仅运行 DWARF / FreeRTOS 夹具测试时需要）

---

## 构建与安装

### 1. 构建 Web UI 与 Rust 二进制

`server` 会在编译期把 `webui/dist` 内嵌进 `jlinkd`（若未先构建前端，会嵌入一个提示占位页）：

```bash
# 1. 构建内嵌 Web UI（Timeline / RTT 页面）
cd webui && npm install && npm run build && cd ..

# 2. 编译并安装 jlinkd 到 ~/.cargo/bin
cargo install --path bins/jlinkd
```

### 2. 构建 VS Code 扩展（可选）

```bash
cd vscode-ext
npm install
npm run build
```

> 若使用 `release/` 目录下的预打包版本，将 `jlinkd` 放到 `PATH`（或 `~/.cargo/bin/jlinkd`），在 VS Code 中安装 `.vsix` 即可。

---

## CLI 使用方法

`jlinkd` 提供一次性调试命令和后台服务命令：

### 通用连接参数与环境变量

`info`、`flash`、`read`、`tasks` 共用以下选项：

| 选项 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `--device <DEVICE>` | `JLINKD_DEVICE` | `STM32H723VG` | J-Link 器件名称（如 `STM32F103C8`、`STM32G474RE`） |
| `--interface <swd\|jtag>` | — | `swd` | 调试接口 |
| `--speed <KHZ>` | — | `4000` | 接口时钟（kHz） |
| `--serial <SERIAL>` | — | 无 | 多台 J-Link 连接时指定序列号 |
| `--dll <PATH>` | `JLINKD_DLL` | `/opt/SEGGER/JLink/libjlinkarm.so` | `libjlinkarm.so` 路径 |
| `--log <FILTER>` | `JLINKD_LOG` | `warn` | 全局日志级别（如 `info`、`jlink=trace`） |

### 常用子命令

```bash
# 1. 离线检查 ELF 符号与 FreeRTOS 信息（无需连接探针，首次建缓存后可毫秒级加载）
jlinkd symbols build/firmware.elf --lookup g_state --lookup main

# 2. 查看探针与目标连接状态
jlinkd info --device STM32H723VG
jlinkd info --no-target                    # 仅检测 J-Link 探针

# 3. 增量烧录 ELF 或 BIN（仅擦写变化的扇区）
jlinkd flash build/firmware.elf --device STM32H723VG
jlinkd flash build/firmware.bin --device STM32H723VG --no-run   # 烧录复位后保持暂停

# 4. 不停机读取内存或 C/C++ 变量表达式
jlinkd read 0x20000000 --count 4 --width 4 --device STM32H723VG
jlinkd read "g_state.items[2].gain" --elf build/firmware.elf --device STM32H723VG

# 5. 查看 FreeRTOS 任务列表、剩余栈与调用栈
jlinkd tasks --elf build/firmware.elf --device STM32H723VG
jlinkd tasks --elf build/firmware.elf --halt --backtrace
jlinkd tasks --elf build/firmware.elf --break-at Error_Handler --backtrace

# 6. 启动/桥接后台服务
jlinkd serve --port 47110 --dap-port 47111 --idle-exit-secs 30
jlinkd dap --dap-port 47111                # stdio <-> DAP TCP 桥接（按需自动拉起 serve）
```

> **注意（探针独占）**：同一时刻只能有一个进程打开 J-Link。如果 `jlinkd serve` 正在持有探针，直接执行 `info` / `flash` / `read` / `tasks` 会失败，需等待空闲释放（默认调试结束 30 秒后释放）或退出后台进程。

---

## VS Code 插件使用

### 1. `launch.json` 配置示例

在项目 `.vscode/launch.json` 中使用 `"type": "jlinkd"`：

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "jlinkd",
      "request": "launch",
      "name": "Debug (J-Link)",
      "device": "STM32H723VG",
      "interface": "SWD",
      "speed": 4000,
      "elf": "${workspaceFolder}/build/firmware.elf",
      "flash": "changed",
      "runToMain": false,
      "stopOnEntry": false,
      "dll": "/path/to/libjlinkarm.so",
      "svd": "${workspaceFolder}/STM32H723.svd",
      "project": "${workspaceFolder}/.jlinkd/project.yaml",
      "preLaunchTask": "build"
    },
    {
      "type": "jlinkd",
      "request": "attach",
      "name": "Attach (J-Link)",
      "device": "STM32H723VG",
      "elf": "${workspaceFolder}/build/firmware.elf"
    }
  ]
}
```

### 2. `launch.json` 属性说明

- **必填**：`device`、`elf`
- **连接选项（`launch` / `attach` 通用）**：
  - `interface`：`"SWD"`（默认）或 `"JTAG"`
  - `speed`：数字（kHz，默认 `4000`）或 `"auto"`
  - `serial`：J-Link 序列号（数字或字符串）
  - `dll`：`libjlinkarm.so` 的完整路径（不在 `/opt/SEGGER/JLink/libjlinkarm.so` 或系统库路径时使用；也可写 `"${env:JLINKD_DLL}"`）
  - `project`：`.jlinkd/project.yaml` 路径（缺省时插件自动填入 `${workspaceFolder}/.jlinkd/project.yaml`）
  - `svd`：CMSIS-SVD 文件路径（缺省时尝试从已安装的 Ozone 目录匹配）
- **`launch` 专属选项**：
  - `flash`：`"changed"`（默认）、`"always"` 或 `"never"`（`"never"` 跳过烧录）
  - `runToMain`：`true` 时复位运行到 `main` 并停住
  - `stopOnEntry`：`true` 时复位后停在入口指令（优先级高于 `runToMain`）
- **插件设置（`settings.json`）**：
  - `jlinkd.path`：自定义 `jlinkd` 可执行文件路径（默认依次查找扩展内置目录、`~/.cargo/bin/jlinkd`、`PATH`）
  - `jlinkd.replaceWatchView`：调试期间是否接管原生 Watch 视图到 Live Watch（默认 `true`）

### 3. 界面功能与快捷键

- **`F6`（`jlinkd: Build & Reload`）**：先执行 `preLaunchTask` 编译，再无感重烧 ELF 并自动恢复断点与采样。
- **调试侧栏视图**：`Live Watch`（支持在线改值、0~5 Hz 调节）、`FreeRTOS Tasks`、`Peripherals`。
- **Web 面板**：调试期间可在 VS Code 内或浏览器打开 `http://127.0.0.1:47110/timeline`（波形）与 `http://127.0.0.1:47110/rtt`（RTT）。

---

## 项目配置文件（`.jlinkd/`）

### `.jlinkd/project.yaml`

```yaml
target:
  family: stm32h7          # 可选；STM32F1/G4/H7/H74 系列可自动识别

debug:
  maskInterruptsOnStep: true
  vectorCatch: true
  flashBreakpoints: auto   # auto | never | always
  freezeOnHalt:            # 暂停时冻结的外设（见 devices/*.yaml）
    - iwdg1
    - tim1

sampling:
  hss:
    periodUs: 1000         # HSS 采样周期（微秒）
  watchRateHz: 5

rtt:
  config: .jlinkd/rtt.yaml

svd: path/to/STM32H723.svd
visualizers:
  - .jlinkd/custom_types.lua
```

### `.jlinkd/rtt.yaml`

`jlinkd` 仅在提供 RTT 配置时才启动 RTT（避免无意义扫描 RAM 影响 HSS 带宽）：

```yaml
controlBlock: auto         # auto（按 ELF 中 _SEGGER_RTT 或扫描）或显式地址如 0x20001000
channels:
  0:
    name: log
    type: text
    ansi: true
  1:
    name: metrics
    type: text-regex
    pattern: 'err=(?P<err>-?\d+(\.\d+)?)'
  2:
    name: telemetry
    type: struct
    sync: 0xA55A
    struct: TelemetryFrame
    timestamp:
      field: cyccnt
      clockHz: 540000000
    timeline:
      - pos[0..2]
      - { expr: loopUs, group: timing }
```

> 支持的通道类型：`text`、`text-regex`、`struct`、`lua`、`systemview`。

---

## 支持的内置器件配置

`devices/` 目录下提供以下器件族配置（自动开启 `DBGMCU_CR` 低功耗调试时钟并定义 RAM/Flash 区间与 `freezeOnHalt` 位）：

- `stm32f1`（[devices/stm32f1.yaml](devices/stm32f1.yaml)）
- `stm32g4`（[devices/stm32g4.yaml](devices/stm32g4.yaml)）
- `stm32h7`（[devices/stm32h7.yaml](devices/stm32h7.yaml)）
- `stm32h74`（[devices/stm32h74.yaml](devices/stm32h74.yaml)）

---

## 开发与测试

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

单元测试基于 `probe-mock` 运行，无需连接真实硬件；带 `#[ignore]` 的在板回归测试需要真实目标板。

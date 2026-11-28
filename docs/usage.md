# Haltd 用户使用指南与全量自定义手册

`Haltd` 是面向 STM32（ARM Cortex-M）的开源 J-Link 调试后端、VS Code 扩展与命令行工具，采用「常驻后台进程 + DAP + WebSocket 数据通道」架构。除断点、`-O2` 单步、HardFault 译码与 FreeRTOS 任务感知外，还提供**一键工程配置**、**源码免框选智能链式+命名空间取词**、**官方 CMSIS-SVD 自动拉取缓存**、**Ozone 风格 Live Watch**、**1 kHz 高速波形 Timeline** 与**结构化 RTT 解码**。

## 目录

- [1. 环境准备与安装](#1-环境准备与安装)
- [2. 快速开始与一键配置项目](#2-快速开始与一键配置项目)
- [3. 源码级调试与免框选智能取词](#3-源码级调试与免框选智能取词)
  - [免框选链式取词与外层 Namespace 自动补齐](#免框选链式取词与外层-namespace-自动补齐)
  - [断点、数据观察点与 Fault 捕获](#断点数据观察点与-fault-捕获)
- [4. 不停机实时观测（Live Watch & Timeline）](#4-不停机实时观测live-watch--timeline)
  - [Live Watch](#live-watch)
  - [Timeline 示波器](#timeline-示波器)
- [5. FreeRTOS、双调试器同步与自动 SVD 外设视图](#5-freertos双调试器同步与自动-svd-外设视图)
- [6. 全量自定义参考手册（Customization Reference）](#6-全量自定义参考手册customization-reference)
  - [6.1 VS Code 扩展设置（`settings.json`）](#61-vs-code-扩展设置settingsjson)
  - [6.2 快捷键全表与自定义改绑（`keybindings.json`）](#62-快捷键全表与自定义改绑keybindingsjson)
  - [6.3 启动与附加配置（`.vscode/launch.json`）](#63-启动与附加配置vscodelaunchjson)
  - [6.4 项目引擎配置（`.haltd/project.yaml`）与 `freezeOnHalt` 外设表](#64-项目引擎配置haltdprojectyaml与-freezeonhalt-外设表)
  - [6.5 自定义 C++ 类型美化规则（`.haltd/visualizers.lua`）](#65-自定义-c-类型美化规则haltdvisualizerslua)
  - [6.6 自定义 RTT 结构化解码（`.haltd/rtt.yaml` 与 Lua 解码器）](#66-自定义-rtt-结构化解码haltdrttyaml-与-lua-解码器)
  - [6.7 工作区预设与持久化状态文件（`.haltd/timeline-presets.json` / `state.json`）](#67-工作区预设与持久化状态文件haltdtimeline-presetsjson--statejson)
  - [6.8 芯片家族 Profile（`devices/*.yaml`）与环境变量/缓存目录](#68-芯片家族-profiledevicesyaml与环境变量缓存目录)
- [7. 完整实战案例：七轴机械臂重力补偿数据采集](#7-完整实战案例七轴机械臂重力补偿数据采集)
- [8. 命令行（CLI）、常见问题与迁移](#8-命令行cli常见问题与迁移)

---

## 1. 环境准备与安装

### 前置依赖

- **操作系统**：Linux x64 或 Windows 10 / 11 x64
- **SEGGER J-Link 软件包**：提供运行时动态加载的 J-Link 驱动与动态库
  - Linux：默认查找 `/opt/SEGGER/JLink/libjlinkarm.so` 或系统库路径
  - Windows：默认从注册表 `Software\SEGGER\J-Link` 查找 `JLink_x64.dll` 或系统搜索路径
- **VS Code**：1.90 或更新版本

使用预打包的发布包时，不需要在本机安装 Rust、Node.js 或 VC 运行库。

### 安装 VS Code 插件

发布包中的 `.vsix` 已内置对应平台的 `haltd`（或 `haltd.exe`）可执行文件：

```bash
# Linux x64
code --install-extension haltd-<版本>-linux-x64.vsix

# Windows x64
code --install-extension haltd-<版本>-win32-x64.vsix
```

也可以在 VS Code 扩展面板右上角的 `...` 菜单中选择「从 VSIX 安装」。如果使用了 VS Code 配置文件（Profile），可在命令后加上 `--profile <名称>`。

> 如需单独在终端使用 `haltd` CLI，可将压缩包中的 `haltd` / `haltd.exe` 放入 `PATH`（扩展默认也会查找自身内置目录与 `~/.cargo/bin/haltd(.exe)`）。

---

## 2. 快速开始与一键配置项目

1. 编译固件时保留 DWARF 调试信息（GCC / Clang 开启 `-g`，支持 `-O2` 优化）。
2. 在 VS Code 打开工程目录，按 `Ctrl+Shift+P` 运行 **`Haltd: Configure Project`**（或在 Run and Debug 视图点击「create a launch.json file」选择 **J-Link (Haltd)**）：
   - **第 1 步（选芯片）**：扩展自动扫描工程中的 `*.ioc`、`CMakeLists.txt`、`Makefile`、`*_FLASH.ld` 提取芯片线索并置顶提示，你只需确认目标芯片型号（例如 `STM32H723VG`、`STM32G474RE`、`STM32F103C8`）；
   - **第 2 步（自动搜索 ELF）**：自动扫描工程中的 ARM 32-bit ELF（过滤宿主机产物）。命中唯一文件时直接选中；存在多个 `.elf`（如 `Debug` / `Release`）时弹出列表交由你选择；尚未编译时根据 `CMakePresets.json` / `CMakeLists.txt` / `Makefile` 推导预期路径供确认；
   - **第 3 步（构建任务与双配置生成）**：自动关联 `.vscode/tasks.json` 中的默认构建任务（若检测到 CMake / Make / Ninja 且尚未配置任务，自动生成默认 `build` 任务），然后**一次性向 `.vscode/launch.json` 合并写入两套配置**（不破坏已有注释与其他调试器配置）：
     - **`Haltd: Build & Debug`**：`request: "launch"`，默认 **`runToMain: true`**，增量烧写变化的扇区，支持 `F6` Build & Reload；
     - **`Haltd: Attach`**：`request: "attach"`，不复位、不烧录，直接附着正在运行的板子。
   - **第 4 步（后台预缓存 SVD）**：立即在后台从官方仓库拉取对应型号的 CMSIS-SVD 文件存入全局缓存。
3. 接好 J-Link 探针与目标板，按 **`F5`** 开始调试。

---

## 3. 源码级调试与免框选智能取词

| 基础调试操作 | 默认快捷键 | 说明 |
|---|---|---|
| 开始 / 继续 | `F5` | 按所选配置启动或恢复运行 |
| 单步跳过 / 进入 / 跳出 | `F10` / `F11` / `Shift+F11` | 支持 `-O2` 内联函数虚拟栈帧展开与 `<optimized out>` 提示 |
| **编译并重新加载（Build & Reload）** | `F6` | 先执行 `preLaunchTask` 编译，成功后再增量烧写并重绑断点与监视 |
| 重启 | `Ctrl+Shift+F5` | 复位并重新进入启动流程 |
| **加入 Live Watch** | **`Ctrl+W`**（macOS `Cmd+W`） | 仅在 Haltd 调试中且焦点在代码编辑区生效（支持免框选智能取词） |
| **加入 Timeline (HSS)** | **`Ctrl+G`**（macOS `Cmd+G`） | 代码编辑区与 Live Watch 表内均可按此键直接送入 1 kHz 采样 |

### 免框选链式取词与外层 Namespace 自动补齐

在 C/C++ 源码中把光标放到变量名上直接按 **`Ctrl+W`**（加入 Live Watch）或 **`Ctrl+G`**（加入 Timeline）时，无需提前拖选整段文本：

1. **有框选优先**：若当前存在选中文本，100% 原样采用所选文本（方便手动框选算术表达式如 `g_adc * 3.3f / 4096.0f` 或解引用 `*g_ptr`）。
2. **左侧链条轻量扩展**：未框选时，从光标所在位置沿左侧的 **`.`、`->`、`::`、`[i]`**（含嵌套下标、模板实参 `Type<T>::member` 与多行断行）自动溯源，且**只向左扩展、不向右吞并**：
   - 光标在 `g_ctrl.pid[2].gain` 的 `pid` 上按 `Ctrl+W` → 提取 `g_ctrl.pid`；
   - 光标在 `gain` 上按 `Ctrl+W` → 提取 `g_ctrl.pid[2].gain`；
   - **下标内向外吸附**：光标放在数组下标内（无论是数字 `g_arr[2]` 的 `2`、变量 `g_arr[idx]` 的 `idx`，还是嵌套 `a[b[i]]`）按快捷键时，统一向外吸附到最外层 `]`，直接取出完整元素 `g_arr[idx]` / `a[b[i]]`；
   - **严守边界**：不会向左吞入一元运算符（如 `*p->next` 光标在 `next` 取 `p->next`），也不会错误截断函数调用返回值链（如 `get().field`）。
3. **自动补齐外层 Namespace / 类作用域**：自动调用 VS Code 内置 `vscode.executeDocumentSymbolProvider` 获取光标所在外层 `namespace`、`class`/`struct`（含 `.cpp` 中的类外成员函数定义 `ns::Cls::fn`）及函数作用域，过滤匿名命名空间后生成由内向外的候选列表：
   - 调试连接中通过后端 `symbols.resolve` 无副作用精确消歧：**优先命中当前最内层命名空间变量**，如果当前命名空间内写的是全局 C 变量（如 `uwTick`、`hadc1`），会自动跳过不存在的前缀精确落回全局符号，**绝不误伤全局 C 变量**；
   - 若固件中某处唯一变量（如类静态成员 `Widget::s_count`）被短名引用，也会自动规范化补全为完整限定名；
   - 若你显式写了前导全局限定 `::g_var`，则会严格保留全局匹配语义。

> **关于 `Ctrl+W` 与原生关闭标签页**：`Ctrl+W` 绑定限定了 `inDebugMode && debugType == 'haltd' && editorTextFocus`。未在 Haltd 调试中，或焦点在终端 / 侧边栏 / Live Watch 面板时，`Ctrl+W` 仍执行原生关闭标签页。若希望在调试中继续用 `Ctrl+W` 关标签页，可在 [6.2 节](#62-快捷键全表与自定义改绑keybindingsjson) 查看改绑方式。

### 断点、数据观察点与 Fault 捕获

在行号左侧点击设置断点，右键「Edit Breakpoint」可配置条件、命中次数或日志点（可自由组合）：

| 类型 | 示例 | 效果 |
|---|---|---|
| 条件 | `count > 10 && g_state.items[2].gain != 0` | 表达式为真才停 |
| 命中次数 | `5`、`>5`、`>=5`、`<5`、`<=5`、`%10` | 恰好第 5 次 / 大于 / 小于 / 每 10 次 |
| 日志点 | `tick={uwTick} mode={g_state.mode}` | 打印到调试控制台，不暂停目标 |

**表达式与使用注意点**：
- 支持算术、比较、逻辑、位运算、标量类型转换、成员、下标、取址（`&`）和解引用（`*`），不支持调用固件函数。
- 表达式内部整数按有符号高精度计算，无符号减法不会自动回绕（如 `count` 为 `0` 时 `count - 1` 得 `-1`）；如需 C 无符号语义请显式强转 `(uint32_t)(count - 1)`。
- 条件写错（如符号不存在）时会停下并给出原因，避免静默漏停。
- 条件断点由主机端判断，每次命中都会短暂停一下再恢复（单次约数毫秒），请勿放在高频中断里；高频场景建议使用 Timeline 或数据断点。
- 在行内单步经过日志点时不会输出日志。
- **硬件断点用完时**会自动改用 Flash 断点（需要探针具备 FlashBP 许可）；若不想写 Flash，可在 `.haltd/project.yaml` 中将 `flashBreakpoints` 设为 `never`。

**数据断点（Watchpoint）**：在 Variables 或 Live Watch 中右键变量即可设置读、写、读写触发的 DWT 硬件观察点（支持附加条件）。监视长度必须为 1/2/4 字节且按自身地址对齐。

**Fault 自动捕获与译码**：HardFault、BusFault、MemManage、UsageFault 默认开启捕获，可在 Breakpoints 视图开关。触发后会弹出 `CFSR` / `HFSR` / `MMFAR` / `BFAR` / `AFSR` 译码说明，并将调用栈直接定位到出错代码行（注：不精确总线错误 `IMPRECISERR` 无法精确指向触发指令）。

**周期计数**：每次停下时，状态栏会显示距离上一次停下经过的 `DWT_CYCCNT` 周期数与换算时间（暂停期间挂起的中断在恢复后会执行并计入其中）。

---

## 4. 不停机实时观测（Live Watch & Timeline）

以下功能基于 AHB-AP / HSS，不需要暂停目标。

### Live Watch

默认停靠于底部 **Panel**（`LIVE WATCH` 面板标签页，调试启动时自动切出；也可点击左下状态栏 `Live Watch` 按钮或执行 `Haltd: Open Live Watch` 打开），并可点击视图右上角 **Pop out** 按钮弹出到编辑区独立标签页（与源码左右分栏或拖到副屏，两处共享实时数据流，点 **Dock** 即可收回到面板）。调试期间原生 Watch 视图默认隐藏并将「Add to Watch」接管到 Live Watch。

- **多列树形表**：分列展示 `Expression`（变量名）、`Value`（当前值，行内紧随 `auto`/`dec`/`hex` 等可点击格式标签，切换零延迟）、`Type`、`Address`（含位段信息）与 `Rate`（刷新率）。可通过表头右键或 `haltd.liveWatch.columns` 开关列，拖拽分隔线调整列宽。
- **值变动新鲜度着色**：运行态数值跳变时立即高亮红字并平滑冷却（默认 `2500 ms`）；断点停机或单步后，本轮改变的变量保持静态红字提示直至下次运行。悬停数值单元格可查看「上次值 → 当前值」。
- **底部快速输入与 ELF 自动补全**：底部常驻 `+ Add expression…` 输入行，输入 2 个字符起自动联想 ELF 全局/静态符号、类型与地址；支持 `0–20 Hz` 独立调频（默认 `5 Hz`，`0` 表示仅停机时刷新）。
- **原位写回与多进制格式**：支持行内切换 `Default` / `Dec` / `Uint` / `Hex` / `Bin` / `Float` / `Char`；双击可写标量原位改值，支持 `0x`、`0b`、`_` 分隔符、字符字面量 `'A'` 与 C/C++ 枚举常量名（如 `STATE_RUN`）。
- **动态寻址与算术表达式**：运行期自动重解析 `*ptr`、`ptr->field`、`arr[idx]`（指针初始化前提示错误，非 NULL 后自动恢复）；支持 `g_adc * 3.3 / 4096.0`、`(reg >> 4) & 1`、`&g_motor` 等只读表达式监视。
- **观测到调试闭环**：Live Watch 行内右键支持下发 DWT 硬件观察点（Break on Write / Access / Read）、在内存视图打开对应地址，或直接按 **`Ctrl+G`** 送入 Timeline。

### Timeline 示波器

采用参考 SEGGER Ozone Timeline / Data Graph 的多轨道同步示波器布局：每条轨道左侧为独立图例与控制栏（含来源标识 `HSS` / `POLL` / `RTT` 与数值读数），右侧为波形画布，底部共用时间轴与可拖拽游标手柄（`S` / `A` / `B`）。点击状态栏 `Timeline` 按钮、按 **`Ctrl+Shift+T`** 或执行 `Haltd: Open Timeline` 打开，也可在浏览器访问 <http://127.0.0.1:47110/timeline>。

- **添加信号与工作区预设（Presets）**：
  - 在源码或 Live Watch 按 **`Ctrl+G`**（或右键「Add to Timeline (HSS)」）、在顶部输入表达式回车（支持 2 字符以上 ELF 补全），或从左下角 `RTT` 抽屉拖拽信号到轨道/空白区新建轨道；结构体与数组自动展开（默认上限 64 路）。
  - 通过顶部 **Presets** 下拉框与保存按钮（或 VS Code 命令 `Haltd: Load Timeline Preset`）将信号、采样率、轨道分组、颜色与 Y 轴范围保存到工作区 `.haltd/timeline-presets.json`，支持**覆盖加载（Replace）**与 **`+` 追加（Append）**。
  - **在工具栏 `Sample` 下拉框选择采样率（`20 Hz` ~ `1 kHz`）后开始采集 HSS/Poll**（`Off` 时暂停采样；RTT 信号随固件写入到达）。

| 采样模式 | 说明 |
|---|---|
| `auto` | 优先走 HSS，超出探针带宽预算的部分自动转轮询 |
| `hss` | J-Link 高速采样（J-Link V9 最高 1 kHz，受块数与字节预算限制） |
| `poll` | 主机轮询读取，最高 100 Hz |
| `rtt` | 来自 RTT 解码通道的信号 |

**常用键鼠交互**：
- **滚轮**：横向滚动时间窗；`Shift + 滚轮`：缩放鼠标所在轨道 Y 轴；`Ctrl + 滚轮`：以鼠标位置为中心缩放时间轴。
- **拖动**：左键/中键拖动平移；`Shift + 左键拖动`：仅上下平移当前轨道 Y 轴；`Ctrl + 左键拖动`：框选放大时间区间。
- **游标与标签**：单击放置临时采样游标 `S`；`A` / `B` 键在悬停处放置测量游标（底栏显示 `Δt`、`1/Δt`、`ΔV`、`dV/dt`）；右键支持添加固定标签 `M1`, `M2`…；`Esc` 清除游标；`Space` 切换跟随/暂停；`Home`/`End` 跳首尾；`Ctrl+0` Fit 全部历史。
- **录制与导出**：点击 `Record` 实时显示录制时长与采样点数，停止后顶部横幅可一键查看或**将 `.hlrec` 无损导出为 CSV**（即使超出 10 分钟内存环形缓存也不丢点）；点击 **`Export CSV`** 优先匹配 `Cursor A..B` 区间，支持仅可见信号、**Zero-Order Hold（零阶保持对齐）**、从 `0.000 s` 计时及原生「另存为」对话框。

---

## 5. FreeRTOS、双调试器同步与自动 SVD 外设视图

- **FreeRTOS Tasks 视图**：自动从 ELF DWARF 读取 TCB 与任务链表布局（支持 CM3 / CM4F / CM7），显示任务状态、优先级、剩余栈高水位及各任务独立调用栈。默认设置 `haltd.rtos.threadView` 为 `currentOnly`（Call Stack 仅聚焦当前线程/中断，所有任务在 `FreeRTOS Tasks` 视图查看）。
- **与 SEGGER Ozone 同时 attach**：支持同时连接同一开发板；当 Ozone 恢复运行、暂停或单步时，Haltd 自动同步真实状态与调用栈，并提醒硬件比较器占用冲突。
- **CMSIS-SVD 自动拉取缓存与 Peripherals 视图**：
  - 查找与获取优先级：**① 手工指定的 `svd` 路径**（`launch.json` / `.haltd/project.yaml`） → **② 全局本地缓存 `<cache_root>/svd/files/`** → **③ 本机已安装的 Ozone / STM32CubeCLT / STM32CubeIDE 目录** → **④ 后台自动从官方仓库拉取并缓存**（首选 ST 官方 `Open-CMSIS-Pack/STM32<Family>xx_DFP`，多厂商与二级回退 `cmsis-svd/cmsis-svd-data`）；
  - 启动 `F5` 时若本地尚未缓存对应芯片 SVD，后台异步下载校验通过后直接通过 `haltd/reloadSvd` 热挂载到当前调试会话并刷新 `Peripherals` 视图，**零阻塞启动速度**；也可随时执行命令 **`Haltd: Fetch / Refresh Device SVD`** 手动拉取或更新。
  - **读副作用保护**：自动屏蔽 `USART/UART/LPUART/SPI/I2S/I2C/ADC/SDIO/SDMMC/DCMI/SAI` 等外设的 `DR` / `RDR` / `RXDR` / `RXDATA` / `FIFO` 等寄存器以及带 `readAction` 的寄存器，显示 `<not read: reading changes it>`，避免打开外设视图就把数据读丢，需要时可右键手动读取，并支持按位段读改写。

---

## 6. 全量自定义参考手册（Customization Reference）

以下完整列出当前版本 Haltd 所有可定制配置、脚本接口、预设格式与环境变量。

### 6.1 VS Code 扩展设置（`settings.json`）

可在用户设置或工作区 `.vscode/settings.json` 中配置：

| 设置项 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `haltd.path` | `string` | `"haltd"` | `haltd` 可执行文件路径（默认依次查找插件内置 `bin/`、`~/.cargo/bin/` 与 `PATH`） |
| `haltd.replaceWatchView` | `boolean` | `true` | Haltd 调试期间隐藏 VS Code 原生 Watch 视图并将 Add to Watch 接管至 Live Watch |
| `haltd.rtos.threadView` | `"currentOnly" \| "currentFirst"` | `"currentOnly"` | Call Stack 呈现策略：`currentOnly` 仅显示当前运行核心/任务（Ozone 风格）；`currentFirst` 展开全部 FreeRTOS 任务 |
| `haltd.liveWatch.columns` | `string[]` | `["type", "address", "rate"]` | Live Watch 可选显示列（`Expression` 与 `Value` 强制常驻） |
| `haltd.liveWatch.freshnessDurationMs` | `number` (`0..10000`) | `2500` | 运行态数值变动后红色冷却持续毫秒数；设为 `0` 关闭时间渐变（仍保留停机变动标红） |
| `haltd.liveWatch.openOnDebugStart` | `boolean` | `true` | 启动 Haltd 调试会话时自动展开底部 Live Watch 面板 |
| `haltd.svd.autoDownload` | `boolean` | `true` | 未手工指定 `svd` 且本地无缓存时，自动从官方仓库拉取并缓存对应芯片 CMSIS-SVD |
| `haltd.svd.mirrorPrefix` | `string` | `""` | 自定义 GitHub Raw URL 加速前缀（内网代理或镜像场景使用，例如 `https://mirror.example.com/`） |

### 6.2 快捷键全表与自定义改绑（`keybindings.json`）

#### 全局与编辑器快捷键

| 快捷键 | macOS | 作用域（`when`） | 命令 ID | 功能 |
|---|---|---|---|---|
| `F6` | `F6` | `debugType == haltd` | `haltd.buildAndReload` | 编译 `preLaunchTask` 并增量重载 |
| `Ctrl+W` | `Cmd+W` | `inDebugMode && debugType == 'haltd' && editorTextFocus` | `haltd.addSelectionToWatch` | 免框选链式+作用域补全加入 Live Watch |
| `Ctrl+G` | `Cmd+G` | `inDebugMode && debugType == 'haltd' && editorTextFocus` | `haltd.addSelectionToTimeline` | 免框选链式+作用域补全加入 Timeline (HSS) |
| `Ctrl+Shift+T` | `Cmd+Shift+T` | `debugType == 'haltd'` | `haltd.openTimeline` | 打开 Timeline 面板 |

#### Live Watch 树表内快捷键（焦点在 Live Watch 表格内生效）

| 快捷键 | 功能 |
|---|---|
| `↑` / `↓` | 移动焦点行（`Shift+↑/↓` 连续多选，`Ctrl/Cmd+Click` 跳选） |
| `→` / `←` | 展开 / 折叠结构体（或在子成员按 `←` 跳回父节点） |
| `F2` / `Enter` | 可写标量进入原位改值；不可写但可展开行按 `Enter` 展开/折叠；`F2` 根节点表达式列可就地改表达式 |
| `Del` / `Backspace` | 删除选中监视项 |
| `Ctrl+G` / `Cmd+G` | 将选中行（支持多选）送入 Timeline (HSS) |
| `Ctrl+R` / `Cmd+R` | 为选中行快速调整刷新率（`0–20 Hz`） |
| `Ctrl+C` / `Cmd+C` | 复制选中项为 `expr = value` |
| `Ctrl+F` / `Cmd+F` | 唤醒表内过滤框 |
| `/` 或 `+` | 聚焦底部 `+ Add expression…` 快速添加行 |

#### 自定义改绑示例（`.vscode` 或用户 `keybindings.json`）

如果习惯在调试期间继续用 `Ctrl+W` 关闭编辑器标签页，可将其释放并改为 `Alt+W`：

```json
[
  {
    "key": "ctrl+w",
    "command": "-haltd.addSelectionToWatch",
    "when": "inDebugMode && debugType == 'haltd' && editorTextFocus"
  },
  {
    "key": "alt+w",
    "command": "haltd.addSelectionToWatch",
    "when": "inDebugMode && debugType == 'haltd' && editorTextFocus"
  }
]
```

### 6.3 启动与附加配置（`.vscode/launch.json`）

通过 `Haltd: Configure Project` 自动生成的典型配置包含以下全部可调字段：

| 字段 | 适用请求 | 类型 / 可选值 | 默认值 / 说明 |
|---|---|---|---|
| `type` | `launch` & `attach` | `"haltd"` | 必填 |
| `request` | `launch` & `attach` | `"launch" \| "attach"` | `launch` 烧录/复位；`attach` 无侵入直接附着运行中目标 |
| `device` | `launch` & `attach` | `string` | 必填，J-Link 器件全名，如 `STM32H723VG`（支持补全、悬停与警告提示） |
| `elf` | `launch` & `attach` | `string` | 必填，带 DWARF 调试信息的固件 ELF 路径 |
| `interface` | `launch` & `attach` | `"SWD" \| "JTAG"` | 默认 `"SWD"` |
| `speed` | `launch` & `attach` | `number \| "auto"` | 接口时钟 kHz，默认 `4000`（满速 HSS 场景建议升至 `8000`–`12000`） |
| `flash` | `launch` | `"changed" \| "always" \| "never"` | 默认 `"changed"`（仅擦写变化扇区） |
| `runToMain` | `launch` | `boolean` | 默认 `true`（启动或重载后自动停在 `main`） |
| `stopOnEntry` | `launch` | `boolean` | 默认 `false`（停在复位向量第一条指令，优先级高于 `runToMain`） |
| `preLaunchTask` | `launch` | `string` | 调试前运行及 `F6` 触发的 `.vscode/tasks.json` 任务 `label` |
| `project` | `launch` & `attach` | `string` | 默认 `${workspaceFolder}/.haltd/project.yaml` |
| `svd` | `launch` & `attach` | `string` | 显式 CMSIS-SVD 文件路径；缺省时自动走缓存/工具链/官方仓库自动拉取 |
| `dll` | `launch` & `attach` | `string` | 显式指定 `libjlinkarm.so` 或 `JLink_x64.dll` 路径 |
| `serial` | `launch` & `attach` | `number \| string` | 多台 J-Link 并存时指定探针序列号 |
| `rtt.autoDecode` | `launch` & `attach` | `boolean` | 默认 `true`（自动解码未在 `rtt.yaml` 中配置的通道） |
| `rtt.autoTimeline` | `launch` & `attach` | `boolean` | 默认 `true`（自动把自动解码的 RTT 数值信号绘制到 Timeline） |

### 6.4 项目引擎配置（`.haltd/project.yaml`）与 `freezeOnHalt` 外设表

放置于工程根目录 `.haltd/project.yaml`（全部字段可选）：

```yaml
target:
  family: stm32h7          # 可选；内置 stm32f1 / stm32g4 / stm32h7 / stm32h74，默认按 device 自动识别

debug:
  freezeOnHalt: [iwdg1, wwdg1, tim1]   # 暂停时冻结的外设计数器，防止看门狗复位或 PWM 输出卡死
  flashBreakpoints: auto               # auto（HW用完切FlashBP） | never | always
  maskInterruptsOnStep: true           # 单步期间屏蔽外部中断，避免步进掉进 SysTick/通信中断
  vectorCatch: true                    # 捕获 HardFault / BusFault / MemManage / UsageFault

sampling:
  hss:
    periodUs: 1000                     # HSS 采样周期微秒数（1000 = 1 kHz，2000 = 500 Hz）
  watchRateHz: 5                       # Live Watch 默认刷新率（0..20 Hz）

svd: path/to/custom.svd                # 可选，覆盖自动查找/缓存的本地 SVD 文件（相对工程根目录）
visualizers:                           # 可选，按顺序加载自定义 C++ 类型美化 Lua 脚本
  - .haltd/visualizers.lua

rtt:
  config: .haltd/rtt.yaml              # 默认即读 .haltd/rtt.yaml
  autoDecode: true
  autoTimeline: true
```

#### 内置家族 `debug.freezeOnHalt` 可用外设名称速查表

> 对应仓内 `devices/*.yaml`（已内置编入 `haltd` 二进制）：

| Profile (`target.family`) | 适用器件前缀 | 可填写在 `freezeOnHalt` 中的外设名称 |
|---|---|---|
| `stm32f1` | `STM32F1*` | `iwdg`, `wwdg`, `tim1`, `tim2`, `tim3`, `tim4` |
| `stm32g4` | `STM32G4*` | `iwdg`, `wwdg`, `rtc`, `lptim1`, `i2c1`, `i2c2`, `i2c3`, `i2c4`, `tim1`, `tim2`, `tim3`, `tim4`, `tim5`, `tim6`, `tim7`, `tim8`, `tim15`, `tim16`, `tim17`, `tim20`, `hrtim1` |
| `stm32h7` | `STM32H7*`（除 H74/H75，如 `STM32H723`） | `iwdg1`, `wwdg1`, `rtc`, `lptim1`, `lptim2`, `lptim3`, `lptim4`, `lptim5`, `i2c1`, `i2c2`, `i2c3`, `i2c4`, `i2c5`, `tim1`, `tim2`, `tim3`, `tim4`, `tim5`, `tim6`, `tim7`, `tim8`, `tim12`, `tim13`, `tim14`, `tim15`, `tim16`, `tim17`, `tim23`, `tim24` |
| `stm32h74` | `STM32H74*` / `STM32H75*` | `iwdg1`, `wwdg1`, `rtc`, `lptim1`, `lptim2`, `lptim3`, `lptim4`, `lptim5`, `i2c1`, `i2c2`, `i2c3`, `i2c4`, `fdcan`, `hrtim`, `tim1`, `tim2`, `tim3`, `tim4`, `tim5`, `tim6`, `tim7`, `tim8`, `tim12`, `tim13`, `tim14`, `tim15`, `tim16`, `tim17` |

### 6.5 自定义 C++ 类型美化规则（`.haltd/visualizers.lua`）

Haltd 已内置对以下类型的开箱即用美化：`units::unit_t`、`Eigen::Matrix` / `Quaternion`、`std::array` / `std::tuple` / `std::optional` / `std::variant` / `std::span` / `std::basic_string_view<char>`、`etl::vector` / `ivector` / `array` / `string`。

通过 `.haltd/project.yaml` 的 `visualizers` 挂载项目级 Lua 文件后，后注册的 `visualizer { ... }` 规则**优先级高于内置规则**。

#### 规则接口与全部宿主方法

```lua
visualizer {
  -- Lua 模式，匹配 DWARF 的全限定类型名（经 strip typedef/cv 后的真实类型名）
  match = "^MyNs::RingBuffer<(.+), (%d+)>$",

  -- 可选：返回列表摘要字符串（captures 为 match 捕获组数组）
  summary = function(v, captures)
    local cap = tonumber(captures[2])
    local cnt = v:field("count_"):num()
    return string.format("size=%d/%d", math.floor(cnt), cap)
  end,

  -- 可选：返回子成员列表。
  -- resolving == true 表示正在为 Live Watch / Timeline 静态解析子表达式地址（如 buf[0]），
  -- 此时即使容器当前为空/未就绪，也建议返回潜在子成员以允许预先绑定。
  children = function(v, captures, resolving)
    local elems = v:field("storage_"):elements()
    local cnt = resolving and #elems or math.min(#elems, math.floor(v:field("count_"):num()))
    local out = {}
    for i = 1, cnt do
      out[#out + 1] = { name = "[" .. (i - 1) .. "]", value = elems[i] }
    end
    return out
  end,
}
```

**值句柄 `v` 可用方法全表**：

| 方法 | 返回值 / 说明 |
|---|---|
| `v:field(name)` | 返回结构体成员句柄（支持继承基类成员）；不存在则抛错并回退默认渲染 |
| `v:has(name)` | 返回 `boolean`，判断结构体是否拥有成员 `name` |
| `v:num()` | 将整数、枚举、布尔、浮点或指针读取为 Lua 数值 |
| `v:str()` | 复用 Rust 默认渲染器格式化该子成员摘要字符串 |
| `v:type()` | 返回该句柄的 C/C++ 类型名 |
| `v:deref()` | 解引用指针或引用（等同 `v:at(0)`） |
| `v:at(index)` | 按指针目标步长偏移 `index` 个元素并返回句柄 |
| `v:pointee_size()` | 返回指针/引用目标类型的字节大小 |
| `v:cstring(len)` | 将 `char*` 指针指向的最多 `len` 字节读取为字符串 |
| `v:elements()` | 展开数组或静态容器的所有直接子元素句柄列表（1-based Lua 数组） |
| `v:tuple()` | 按元素索引提取 `std::tuple` 成员句柄列表 |

> **虚拟子成员直达表达式（`VirtualMembers`）**：规则返回的 `{ name = "value", value = ... }` 或 `[0]` 会直接成为合法路径段。例如自定义 `std::optional<T>` 或你自己的封装类暴露出 `value` 后，可以直接在 Live Watch / Timeline 中输入 `g_opt.value` 持续采样。

### 6.6 自定义 RTT 结构化解码（`.haltd/rtt.yaml` 与 Lua 解码器）

只要 ELF 中包含 `_SEGGER_RTT` 符号或存在 `.haltd/rtt.yaml`，即自动启用 RTT。**未配置 `rtt.yaml` 时启用零配置自动解码**：
1. **`auto-struct`**：固件调用 `SEGGER_RTT_ConfigUpBuffer` 时将通道名命名为 `Type`、`struct:ns::Type` 或 `Type@0xA55A`（带 `@0x...` 时启用同步字对齐与丢帧重同步；不带 `@0x...` 且无 `sync` 字段时要求固件使用 `SEGGER_RTT_MODE_NO_BLOCK_SKIP` 模式），自动按 DWARF 布局解码结构体全部数值字段为 `<通道名>.<字段>`。
2. **`auto-text`**：其余上行通道保留完整终端文本输出，同时逐行自动提取 `name=value` / `name: value` 数值（忽略 `0x` 十六进制与 `1.2.3` 版本号，每通道最多追踪 32 个信号）。

#### 显式配置 `.haltd/rtt.yaml` 全格式示例

```yaml
controlBlock: auto                 # auto（读 ELF _SEGGER_RTT 地址）或明确十六进制如 0x24000000
channels:
  0:
    name: log
    type: text
    ansi: true
  1:
    name: sysview
    type: systemview
    file: build/trace.SVDat
  2:
    name: telemetry
    type: struct
    sync: 0xA55A                   # 同步字；结构体中可选含 seq 字段用于统计丢帧
    struct: TelemetryFrame         # ELF 中结构体/类全限定名
    timestamp:                     # 可选：借助硬件周期计数器消除 USB 批量抖动
      field: cyccnt
      clockHz: 550000000
    timeline:                      # 可选：筛选要推入 Timeline 的字段（默认全部数值字段）
      - jointPos[0..6]             # 支持 [a..b] 闭区间展开
      - { expr: loopUs, group: timing }
  3:
    name: metrics
    type: text-regex
    group: control                 # 可选默认分组轨道
    pattern: 'err=(?P<err>-?\d+(?:\.\d+)?)\s+iq=(?P<iq>-?\d+(?:\.\d+)?)'
  4:
    name: motor
    type: lua
    script: .haltd/decode_motor.lua
```

#### 自定义二进制协议 Lua 解码器（`.haltd/decode_*.lua`）

当 `type: lua` 时，脚本需定义全局函数 `decode(bytes, state)`：
- `bytes`：本次从 RTT 通道新读到的原始二进制字节串；
- `state`：持久化 Lua 表，由 Haltd 跨多次调用自动保留，可用于拼接跨包残帧 `state.buf`；
- 返回值：Lua 数组，每项为 `{ 信号名(string), 相对时间戳秒数(number 或 nil), 数值(number) }`。

```lua
-- .haltd/decode_motor.lua：解析 [0xAA, 0x55, iq_i16_le, id_i16_le] 6 字节小端定长帧
function decode(bytes, state)
  local buf = (state.rest or "") .. bytes
  local out = {}
  local pos = 1
  while pos + 5 <= #buf do
    local b0, b1 = buf:byte(pos, pos + 1)
    if b0 == 0xAA and b1 == 0x55 then
      local iq = string.unpack("<i2", buf, pos + 2) * 0.01
      local id = string.unpack("<i2", buf, pos + 4) * 0.01
      out[#out + 1] = { "motor.iq_A", nil, iq }
      out[#out + 1] = { "motor.id_A", nil, id }
      pos = pos + 6
    else
      pos = pos + 1
    end
  end
  state.rest = buf:sub(pos)
  return out
end
```

### 6.7 工作区预设与持久化状态文件（`.haltd/timeline-presets.json` / `state.json`）

- **`.haltd/timeline-presets.json`**：由 Timeline 工具栏 `Presets` 管理，记录 `activePreset` 与各组预设的 `rateHz`、`division`、`splitLanes`、`showBands`、`signals`（含 `kind`/`expr`/`mode`/`group`/`color`/`hidden`）与 `tracks`（Y 轴范围与折叠高度）。
- **`.haltd/state.json`**：由 Live Watch 自动持久化保存 `watches`（`expr`、`rateHz`、`format`）与 `liveWatch`（`visibleColumns`、`columnWidths`、`formats`）。
- **Git 版本控制建议**：建议将 `.haltd/project.yaml`、`rtt.yaml`、`visualizers.lua`、`timeline-presets.json` 提交进团队仓库共享；建议将 `.haltd/state.json` 加入 `.gitignore` 供个人自由调整。

### 6.8 芯片家族 Profile（`devices/*.yaml`）与环境变量/缓存目录

若从源码新增 STM32 系列支持，可在 `devices/<family>.yaml` 定义 `memory`、`flashSectorSize`、`postConnect`（上电写 `DBGMCU_CR` 保证低功耗 `WFI`/`Stop` 不断连）与 `freeze`（各外设对应的 `DBGMCU_*FZ` 寄存器地址和位偏移），并在 `crates/probe-api/src/device.rs` 与 `crates/engine/src/project.rs` 注册。

**全局缓存与环境变量**：

| 环境变量 | 默认值 / 作用 |
|---|---|
| `HALTD_CACHE_DIR` | 覆盖全局缓存根目录（Linux 默认 `$XDG_CACHE_HOME/haltd` 或 `~/.cache/haltd`，Windows 默认系统「文档」目录下的 `haltd`） |
| `HALTD_DLL` | 显式指定 `libjlinkarm.so` / `JLink_x64.dll` 路径 |
| `HALTD_DEVICE` | 覆盖命令行工具默认芯片型号（默认 `STM32H723VG`） |
| `HALTD_LOG` | CLI 日志级别过滤（如 `info`、`jlink=trace`；后台 `serve.log` 固定输出 `info`） |

缓存根目录内部划分：`index/`（ELF `blake3` 符号索引缓存）、`svd/`（`files/` 存放官方 `.svd`，`index/` 存放仓库清单缓存）、`recordings/`（默认 `.hlrec` 与 CSV 目录）、`serve.log`（常驻后台服务日志）。

---

## 7. 完整实战案例：七轴机械臂重力补偿数据采集

本节以一个真实的 **STM32H723VG + C++20 + SheriffOS/FreeRTOS 七轴双臂机器人固件** 为例，演示如何从零接入 Haltd、在 `App/src/thread.cpp` 中一步提取 `ConceptArm::leftArm` 监视其动力学补偿量，再挑选关键关节变量送入 Timeline 做 1 kHz 同步采集与 CSV 导出。

### 背景与工程结构

该工程使用 CMake + `arm-none-eabi-g++`（C++20 `-O2 -g -gdwarf-5`），核心任务与数据结构如下：
- `App/RoboArm/roboArm.hpp`：在 `namespace ConceptArm` 下声明全局双臂核心对象 `extern LeftArm leftArm; extern RightArm rightArm;`；
- `App/RoboArm/ConceptArmCore.hpp` & `RoboArmType.h`：`ConceptArmCore` 内部持有：
  - `frame_.jointState.angle`（`std::array<units::angle::radian_t, 7>`，7 轴物理关节角）
  - `frame_.jointState.measuredTorque`（`std::array<units::torque::newton_meter_t, 7>`，标定后的实测力矩）
  - `dynamicsData_.modelTorque`（`std::array<units::torque::newton_meter_t, 7>`，RNEA 公共重力/科氏+摩擦前馈力矩）
  - `externalForceState_.residual.value`（扣除了动力学模型后的外力残差力矩）
- `App/src/thread.cpp`：高优先级实时线程 `RoboArmThread` 每 1 ms 推进一次双臂闭环：
  ```cpp
  if (!std::holds_alternative<std::monostate>(ConceptArm::leftArm.getStatus())) {
      ...
  } else ConceptArm::leftArm.tick();
  ```

### 第 1 步：一键配置项目与 SVD 自动拉取

1. 用 VS Code 打开机械臂固件工程，按 `Ctrl+Shift+P` 执行 **`Haltd: Configure Project`**；
2. 扩展识别工程中 `CMakeLists.txt` 的 MCU 定义，在选型列表中选定 **`STM32H723VG`**；
3. 自动扫描出工作区下编译好的 ARM 固件 `build/eng27Arm.elf` 并关联已有构建任务 `CMake: build`，一次性向 `.vscode/launch.json` 生成两条配置（`Build & Debug` 默认 `runToMain: true`）：
   ```jsonc
   {
     "version": "0.2.0",
     "configurations": [
       {
         "name": "Haltd: Build & Debug",
         "type": "haltd",
         "request": "launch",
         "device": "STM32H723VG",
         "interface": "SWD",
         "speed": 4000,
         "elf": "${workspaceFolder}/build/eng27Arm.elf",
         "flash": "changed",
         "runToMain": true,
         "preLaunchTask": "CMake: build"
       },
       {
         "name": "Haltd: Attach",
         "type": "haltd",
         "request": "attach",
         "device": "STM32H723VG",
         "interface": "SWD",
         "speed": 4000,
         "elf": "${workspaceFolder}/build/eng27Arm.elf"
       }
     ]
   }
   ```
4. 选定芯片同时，后台静默从 `Open-CMSIS-Pack/STM32H7xx_DFP` 拉取 `STM32H723.svd` 缓存到本地 `<cache_root>/svd/files/`，无需手工配置 `svd` 路径。

> **带宽小提示**：如果后续需要在 Timeline 同步以 **1 kHz** 采多路关节数据，建议把 `speed` 从 `4000` 提升至 **`8000`** 或 **`12000`**（依据板子排线信号完整性选择），可将每帧 HSS 带宽预算从 `150 B` 成倍提升至 `300–450 B`。

### 第 2 步：按需定制 `.haltd/project.yaml` 安全保护

在工程根目录新建 `.haltd/project.yaml`，确保打断点暂停调试时看门狗不会复位芯片，同时保留默认 1 kHz HSS 采样与 5 Hz Live Watch：

```yaml
target:
  family: stm32h7

debug:
  freezeOnHalt: [iwdg1, wwdg1]
  flashBreakpoints: auto
  maskInterruptsOnStep: true
  vectorCatch: true

sampling:
  hss:
    periodUs: 1000
  watchRateHz: 5
```

### 第 3 步：在 `App/src/thread.cpp` 免框选一键 Watch `ConceptArm::leftArm`

1. 若板子已经在上电自检或示教运行，在 Run and Debug 选择 **`Haltd: Attach`** 按 **`F5`** 无复位附着（或选择 **`Haltd: Build & Debug`** 烧录并从 `main` 开始运行）；
2. 打开 `App/src/thread.cpp`，找到 `RoboArmThread` 中的 `ConceptArm::leftArm.tick();` 一行；
3. **不要拖动框选**，直接把编辑器光标放在 `leftArm` 单词上按下 **`Ctrl+W`**：
   - 插件顺着左侧 `::` 链条与 `DocumentSymbol` 自动识别为 **`ConceptArm::leftArm`**；
   - 底部 **Live Watch** 面板自动亮出，出现根节点 `ConceptArm::leftArm`（也可点击右上角 **Pop out** 弹出到右侧分栏与源码并排观察）。
4. 在 Live Watch 中依次点击展开箭头 `▸`：
   - 展开 `ConceptArm::leftArm` → 即使 `frame_` 与 `dynamicsData_` 在 C++ 中是 `private` 成员，DWARF 依然完整展开；
   - 展开 `frame_` → `jointState` → `angle` 与 `measuredTorque`；
   - 展开 `dynamicsData_` → `modelTorque`；
   - 展开 `externalForceState_` → `residual` → `value`；
   - 内置 `std::array` 与 `units::unit_t` 美化规则自动生效，每一轴实时以 `0.5234 rad`、`1.842 N·m` 等物理单位刷新（数字跳变时以鲜红渐变高亮最新变化的关节）。

### 第 4 步：从 Live Watch 挑选重力补偿变量进 Timeline 采集与 CSV 导出

1. **按需挑选核心变量送入 Timeline**：
   由于重力补偿标定重点关注特定承重关节（如肩关节 `1` 号、肘关节 `3` 号）的姿态角、实测力矩与模型补偿力矩对照，在 Live Watch 中按住 `Ctrl` 点击选中以下节点（例如第 1、3 关节）：
   - `ConceptArm::leftArm.frame_.jointState.angle[1]`、`[3]`
   - `ConceptArm::leftArm.frame_.jointState.measuredTorque[1]`、`[3]`
   - `ConceptArm::leftArm.dynamicsData_.modelTorque[1]`、`[3]`
   - `ConceptArm::leftArm.externalForceState_.residual.value[1]`、`[3]`
   然后直接按 **`Ctrl+G`**，全部 8 路变量（仅占用 `32 B` 数据，4 MHz SWD 下即可全部稳跑在 1 kHz HSS，0 降级！）立即打开 **Timeline** 并自动挂载。
   - **分组整理**：在 Timeline 左侧把 `angle[1]/[3]` 拖入 `Joint Angles (rad)` 轨道，把 `measuredTorque`、`modelTorque` 与 `residual` 拖入同一 `Torque Compare (N·m)` 轨道直观对比贴合度；
   - 点击工具栏 **Presets** → **Save as New Preset…** 保存为 **`左臂重力补偿标定`**（写入 `.haltd/timeline-presets.json`，下次调试直接一键加载）。
2. **开启 1 kHz 采集并录制实验数据**：
   - 将工具栏 `Sample` 下拉框从 `Off` 切换为 **`1 kHz`**；
   - 点击工具栏 **`Record`** 按钮，手动拖动左臂缓慢遍历各典型姿态并短暂停留（此时可实时看到 `measuredTorque` 与 `modelTorque` 两条曲线几乎重合，残差波动范围清晰可见）；
   - 实验结束后点击停止录制，在弹出的完成横幅点击 **`Export CSV…`**，保持勾选 **Zero-Order Hold（零阶保持）** 与 `0.000 s` 相对时间起点，通过原生保存对话框保存为 `leftarm_gravity_comp.csv` 用于 Python/MATLAB 动力学参数辨识。
3. **快速迭代调参（`F6`）**：
   - 若发现某轴静态重力残差偏大，在 `App/RoboArm/UrdfRneaProfiles.hpp` 修改连杆质心/质量参数后直接按 **`F6`**（Build & Reload）：Haltd 编译新 ELF 并仅擦写变化的扇区，Live Watch 中的 `ConceptArm::leftArm` 展开树与 Timeline 的 8 路采样信号**自动重绑恢复**，立刻开始下一轮对比采样！

---

## 8. 命令行（CLI）、常见问题与迁移

### 命令行（CLI）

`haltd` 二进制可脱离 VS Code 独立使用。未指定 `--device` 时默认值为 `STM32H723VG`（可通过 `--device` 或 `HALTD_DEVICE` 覆盖）：

```bash
haltd info --device STM32G474RE                          # 查看探针与目标状态（--no-target 仅查探针，--dump-stack <FILE> 导出现场栈与寄存器快照）
haltd flash firmware.elf --device STM32G474RE            # 增量烧录（--no-run 烧录后保持暂停）
haltd read uwTick --elf firmware.elf --device STM32G474RE # 不暂停读取变量或表达式
haltd read 0x20000000 --count 16 --device STM32G474RE    # 读取内存
haltd tasks --elf firmware.elf --backtrace --device STM32G474RE # 查看 FreeRTOS 任务与调用栈（支持 --dump-stack <FILE>）
haltd symbols firmware.elf --lookup main                 # 离线查看 ELF 符号与缓存耗时（无需连接探针）
haltd devices --filter STM32H7 --json                    # 查询 J-Link 支持器件列表、内核与 Flash/RAM 大小（仅需 J-Link 库）
```

常用全局选项：`--interface <swd|jtag>`、`--speed <khz>`、`--serial <sn>`、`--dll <path>`、`--log <filter>`，完整参数请执行 `haltd --help`。

> 调试时会拉起一个后台 `haltd` 服务进程持有探针（监听本机 `127.0.0.1:47110` 和 `127.0.0.1:47111`），调试结束且无网页连接约 30 秒后自动退出。同一时间只允许一个调试会话控制目标；升级插件或修改环境变量后，请等待旧后台进程退出或重载 VS Code 窗口再重新调试。

### 常见问题

| 现象 | 处理建议 |
|---|---|
| 提示找不到 J-Link 库 | 确认已安装 SEGGER J-Link 驱动；绿色版或自定义路径请在 `launch.json` 设置 `dll` 或传 `--dll` / `HALTD_DLL` |
| 连不上目标板 | 先运行 `haltd info --device <器件>` 排查；检查 SWD 接线与供电，或适当降低 `speed` |
| `Ctrl+W` 没有反应或报错找不到外层符号 | 确认处于 Haltd 调试中且安装了 C/C++ 语言扩展（如 `clangd`）提供符号树；确认光标未放在函数返回值调用链（如 `fn().x`）上 |
| Peripherals 视图空或缺少 SVD | 执行 `Haltd: Fetch / Refresh Device SVD`；内网受限可设置 `haltd.svd.mirrorPrefix` 或在 `launch.json` 手动配置 `svd` |
| 断点显示灰色未绑定 | 确认 ELF 是当前源码编译且带 `-g`，或该行在优化后没有生成对应机器指令 |
| Timeline 没有波形数据 | 确认 `Sample` 采样率不是 `Off`、目标处于运行态，且变量为全局或静态变量 |
| 信号提示放不下 HSS | HSS 带宽与块数受探针硬件上限限制，可精选关键变量、提高 SWD `speed`、降低采样率，或改用 `auto`/`poll` |
| RTT 没有输出 | 确认固件启用了 RTT 且 ELF 中存在 `_SEGGER_RTT`（高速二进制建议开启 `SEGGER_RTT_MODE_NO_BLOCK_SKIP`） |
| Windows 烧录路径报错 | Windows 下 J-Link DLL 只接受系统当前代码页（`CP_ACP`）路径，避免路径包含当前代码页无法表示的字符 |
| 其他异常定位 | 执行命令面板 `Haltd: Collect Diagnostics` 复制脱敏诊断包，并查看输出面板 `Haltd`、缓存目录 `serve.log` 与调试控制台 |

### 已知限制与硬件验证情况

- **实测范围**：已在 **STM32F103C8**、**STM32G4 系列**、**STM32H723** + J-Link V9 下完成实际调试与验证；**STM32H74x/H75x** 配置已内置（参考 `RM0433`），但受限于手头硬件，尚未完成充分实机验证。其他 STM32 型号也能连接，但可能缺少内置 `DBGMCU` / 外设冻结配置。
- 芯片读保护解锁或 J-Link 固件升级时是否会弹出阻塞对话框尚未验证（后台模式下建议保持探针固件正常）。
- 如果使用 `runToMain: true` 但固件在进入 `main` 前就死循环，需要手动点暂停或停止来中断等待。
- `.hlrec` 录制文件暂时不能在网页界面中直接导入回放（但可在停止录制后于 Timeline 横幅/Export 弹窗中一键无损导出为 CSV）。

### 从 jlinkd 迁移

项目自 `0.7.0` 起由 `jlinkd` 更名为 `Haltd`，迁移时请注意：
1. 将 `.jlinkd/` 重命名为 `.haltd/`（含 `project.yaml`、`rtt.yaml`、`state.json`、`visualizers.lua`）；
2. 将 `launch.json` 中的 `"type": "jlinkd"` 改为 `"type": "haltd"`，用户设置 `jlinkd.*` 改为 `haltd.*`；
3. 环境变量 `JLINKD_*` 统一更名为 `HALTD_*`（`HALTD_CACHE_DIR` / `HALTD_DLL` / `HALTD_DEVICE` / `HALTD_LOG`）；
4. 卸载旧版 `jlinkd.jlinkd` 扩展再安装新版 `WaterKeyqwq.haltd-embedded`，旧 `.jlrec` 录制文件仍可直接兼容读取。

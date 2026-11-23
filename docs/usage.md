# jlinkd 使用手册

`jlinkd` 是面向 STM32（ARM Cortex-M）的 J-Link 调试后端、VS Code 扩展与命令行工具。除了常规的断点与单步，它还支持在目标不停机的情况下实时读写变量（Live Watch）、绘制高速波形（Timeline），以及结构化解码 RTT 数据。

---

## 1. 环境准备与安装

### 前置依赖

- **操作系统**：Linux x64 或 Windows 10 / 11 x64
- **SEGGER J-Link 软件包**：提供运行时动态加载的 J-Link 库与 USB 驱动
  - Linux：默认查找 `/opt/SEGGER/JLink/libjlinkarm.so` 或系统库路径
  - Windows：默认从注册表 `Software\SEGGER\J-Link` 查找 `JLink_x64.dll` 或系统搜索路径
- **VS Code**：1.90 或更新版本

使用预打包的发布包时，不需要在本机安装 Rust、Node.js 或 VC 运行库。

### 安装 VS Code 插件

发布包中的 `.vsix` 已内置对应平台的 `jlinkd`（或 `jlinkd.exe`）可执行文件：

```bash
# Linux x64
code --install-extension jlinkd-<版本>-linux-x64.vsix

# Windows x64
code --install-extension jlinkd-<版本>-win32-x64.vsix
```

也可以在 VS Code 扩展面板右上角的 `...` 菜单中选择「从 VSIX 安装」。如果使用了 VS Code 配置文件（Profile），可在命令后加上 `--profile <名称>`。

> 如需把 `jlinkd` 单独放到命令行使用，可将压缩包中的 `jlinkd` / `jlinkd.exe` 放到 `PATH` 中（扩展默认也会查找自身内置目录与 `~/.cargo/bin/jlinkd(.exe)`）。

---

## 2. 快速开始

1. 编译固件时保留 DWARF 调试信息（gcc / clang 开启 `-g`，`-O2` 优化同样可用）。
2. 用 VS Code 打开工程目录，在 `.vscode/launch.json` 中添加：

   ```json
   {
     "version": "0.2.0",
     "configurations": [
       {
         "name": "J-Link (jlinkd)",
         "type": "jlinkd",
         "request": "launch",
         "device": "STM32H723VG",
         "interface": "SWD",
         "speed": 4000,
         "elf": "${workspaceFolder}/build/firmware.elf",
         "flash": "changed",
         "runToMain": true,
         "preLaunchTask": "build"
       }
     ]
   }
   ```

3. 接好 J-Link 探针与目标板，按 `F5` 开始调试。

### `launch.json` 字段说明

| 字段 | 必填 | 说明 |
|---|---|---|
| `device` | 是 | J-Link 器件名称，如 `STM32F103C8`、`STM32G474RE`、`STM32H723VG` |
| `elf` | 是 | 带调试信息的 ELF 固件路径 |
| `interface` | | `SWD`（默认）或 `JTAG` |
| `speed` | | 接口时钟（kHz），默认 `4000`；也可写 `"auto"` |
| `flash` | | `changed`（默认，只擦写变化的扇区）、`always`、`never` |
| `runToMain` | | 启动后运行到 `main` 停下 |
| `stopOnEntry` | | 启动后停在入口指令（优先级高于 `runToMain`） |
| `preLaunchTask` | | 调试前运行的构建任务，需与 `tasks.json` 的 `label` 一致；`F6` 也使用它 |
| `svd` | | CMSIS-SVD 文件路径，用于 Peripherals 视图（缺省时尝试从已安装的 Ozone 目录匹配） |
| `project` | | 项目配置文件路径，默认 `${workspaceFolder}/.jlinkd/project.yaml`，没有也可以 |
| `dll` | | 显式指定 `libjlinkarm.so` 或 `JLink_x64.dll` 的路径（多版本共存或绿色版时使用） |
| `serial` | | 连接多台探针时指定序列号（数字或字符串） |

**附加到正在运行的固件（Attach）**：把 `request` 改成 `"attach"`，不会复位也不会烧录。可用字段为 `device`、`elf`、`interface`、`speed`、`project`、`svd`、`dll`、`serial`。

---

## 3. 基本调试

| 操作 | 快捷键 |
|---|---|
| 开始 / 继续 | `F5` |
| 单步跳过 / 进入 / 跳出 | `F10` / `F11` / `Shift+F11` |
| **编译并重新加载（Build & Reload）** | `F6` |
| 重启 | `Ctrl+Shift+F5` |

- **`F6`（Build & Reload）**：先执行 `preLaunchTask` 编译，成功后再只烧写变化的扇区，断点和 Live Watch 会按新 ELF 自动重新绑定。编译失败时不会改动目标。
- **变量与寄存器**：Variables 视图、悬停和 Watch 均可查看和修改变量值；Variables 里同时包含 Registers 作用域；Memory 视图可读写内存（不写 Flash）。
- **`-O2` 优化代码单步**：支持内联函数虚拟栈帧展开，按行单步时会自动越过或进入调用并恢复到语句边界。

### 断点与 Fault 捕获

在行号左侧点击设置断点，右键「Edit Breakpoint」可配置条件、命中次数或日志点（可组合）：

| 类型 | 示例 | 效果 |
|---|---|---|
| 条件 | `count > 10 && g_state.items[2].gain != 0` | 表达式为真才停 |
| 命中次数 | `5`、`>5`、`>=5`、`<5`、`<=5`、`%10` | 恰好第 5 次 / 大于 / 小于 / 每 10 次 |
| 日志点 | `tick={uwTick} mode={g_state.mode}` | 打印到调试控制台，不暂停目标 |

**表达式与使用注意点**：
- 支持算术、比较、逻辑、位运算、标量类型转换、成员、下标和解引用，不支持调用固件函数。
- 表达式内部整数按有符号高精度计算，无符号减法不会自动回绕（如 `count` 为 0 时 `count - 1` 得 `-1`）；如需 C 无符号语义请显式强转 `(uint32_t)(count - 1)`。
- 条件写错（如符号不存在）时会停下并给出原因，避免静默漏停。
- 条件断点由主机端判断，每次命中都会短暂停一下再恢复（单次约数毫秒），请勿放在高频中断里，高频场景建议使用 Timeline 或数据断点。
- 在行内单步经过日志点时不会输出日志。
- **硬件断点用完时**会自动改用 Flash 断点（需要探针具备 FlashBP 许可）；若不想写 Flash，可在 `.jlinkd/project.yaml` 中关闭。

**数据断点（Watchpoint）**：在 Variables 里右键变量选择「Break on Value Change」，支持读、写、读写触发并可附带条件。注意监视长度必须是 2 的幂且按自身地址对齐。

**Fault 自动捕获与译码**：HardFault、BusFault、MemManage、UsageFault 默认开启捕获，可在 Breakpoints 视图开关。触发后会弹出 `CFSR` / `HFSR` / `MMFAR` / `BFAR` / `AFSR` 译码说明，并将调用栈直接定位到出错代码行（注：不精确总线错误 `IMPRECISERR` 无法精确指向触发指令）。

**周期计数**：每次停下时，状态栏会显示距离上一次停下经过的 `DWT_CYCCNT` 周期数与换算时间（暂停期间挂起的中断在恢复后会执行并计入其中）。

---

## 4. 不停机观察（Live Watch & Timeline）

以下功能基于 AHB-AP / HSS，不需要暂停目标。

### Live Watch

位于调试侧栏。在源码或 Variables 中右键「Add to Live Watch」，或点击视图标题栏 `+` 添加表达式。调试期间原生 Watch 视图会隐藏并将「Add to Watch」接管到 Live Watch（可通过设置项 `jlinkd.replaceWatchView: false` 关闭）。

- 刷新率支持 `0–5 Hz` 每行独立设置（`0` 表示仅在目标暂停时刷新）。
- 结构体、数组与指针可逐层展开，展开后的成员可直接右键加入 Timeline。
- 支持右键「Set Value」直接修改目标内存值。
- 指针表达式只在添加或 Reload 时解析一次地址，运行期间指针自身变化不会自动重新寻址。

### Timeline

像示波器一样绘制变量随时间变化的波形。点击状态栏 Timeline 按钮或执行命令 `jlinkd: Open Timeline` 在 VS Code 内打开，也可使用 `jlinkd: Open Timeline in Browser` 在浏览器访问 <http://127.0.0.1:47110/timeline>。

**添加信号与采样率**：
- 在源码或 Live Watch 右键「Add to Timeline (HSS)」，或在网页顶部输入表达式后回车；结构体与数组会自动展开为多路（默认上限 64 路）。请使用全局或静态变量，局部变量无法持续采样。
- **选好信号后，需在工具栏 `Sample` 下拉框选择采样率（`20 Hz` ~ `1 kHz`）才会开始采集**；选 `Off` 时停止采集。

| 采样模式 | 说明 |
|---|---|
| `auto` | 优先走 HSS，超出探针带宽预算的部分自动转轮询 |
| `hss` | J-Link 高速采样（J-Link V9 最高 1 kHz，受块数与字节预算限制） |
| `poll` | 主机轮询读取，最高 100 Hz |
| `rtt` | 来自 RTT 解码通道的信号 |

**常用交互与快捷键**：

| 操作 | 效果 |
|---|---|
| 滚轮 | 以鼠标位置为中心缩放时间轴 |
| `Shift` + 滚轮 | 缩放当前子图的 Y 轴 |
| 左键拖动 | 平移时间并暂停自动跟随 |
| `Shift` + 拖动 | 平移当前子图的 Y 轴 |
| `Ctrl` + 拖动 | 框选时间区间放大 |
| 单击 | 依次放置游标 A、B，侧栏显示 `Δt` 与各信号差值 |
| 双击 | 当前子图 Y 轴恢复自动缩放 |
| `Space` | 切换跟随 / 暂停 |
| `←` / `→` | 前后移动时间窗 |
| `-` / `+` | 缩放时间窗 |
| `A` / `B` | 在鼠标当前位置放置游标 A / B |
| `Esc` | 清除游标 |
| `/` | 聚焦表达式输入框 |
| `?` | 显示快捷键帮助 |

**其他面板功能**：
- 顶部概览条可点击或拖动跳转历史波形。
- 工具栏可直接对目标执行暂停、继续、复位。
- 侧栏支持显示/隐藏、独显（`S`）、改色与修改 `group`（同组信号画在同一张子图，`Grouped` 按钮可切换平铺或分组）。
- 支持 `Min/Max` 包络显示、状态栏 `Events` 事件跳转、导出可见窗口 CSV、保存 PNG 截图、主题切换（默认跟随 VS Code）。
- 支持录制 `.jlrec` 文件（`Record` 按钮或命令 `jlinkd: Start/Stop Recording`）；当前网页端暂不支持直接回放 `.jlrec`。

### 状态与统计

状态栏实时显示连接状态、HSS 实际采样率与丢点计数，点击（或执行 `jlinkd: Show Statistics`）可查看详细统计信息。

---

## 5. RTT 终端与结构化解码

只要在工程中提供 `.jlinkd/rtt.yaml`（或在 ELF 中包含 `_SEGGER_RTT` 符号并配置通道），即可启用 RTT；未配置 RTT 时不会主动扫描 RAM，以免抢占 SWD 带宽影响 HSS。

- 状态栏 `RTT` 按钮可打开通道 0 终端并支持下行输入。
- 命令 `jlinkd: Open RTT Signals` 可打开网页端 RTT 终端与解码信号列表（`http://127.0.0.1:47110/rtt`）。

`.jlinkd/rtt.yaml` 示例：

```yaml
controlBlock: auto                 # auto 取 ELF 中 _SEGGER_RTT 地址，也可写具体十六进制地址
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
    sync: 0xA55A
    struct: TelemetryFrame         # 直接按 ELF 中的结构体布局解析，改字段不用改配置
    timestamp:
      field: cyccnt
      clockHz: 72000000
    timeline:
      - jointPos[0..5]
      - { expr: loopUs, group: timing }
  3:
    name: motor
    type: lua
    script: .jlinkd/decode_motor.lua
  4:
    name: metrics
    type: text-regex
    pattern: 'err=(?P<err>-?\d+(\.\d+)?)'
```

> 提示：RTT 与 1 kHz HSS 同开且日志吞吐较高时，会共享 SWD 总线带宽，可能挤占 HSS；如需满速 HSS，可提高 SWD `speed` 或减小 RTT 输出频率。

---

## 6. FreeRTOS 与 SVD 外设视图

- **FreeRTOS Tasks 视图**：固件使用 FreeRTOS 时自动识别，基于 DWARF 布局解析 TCB 与链表（支持 CM3 / CM4F），显示任务状态、优先级、剩余栈空间及各任务独立调用栈。在 Call Stack 视图中点击对应任务线程即可切换查看上下文。
- **Peripherals 视图**：需配置 CMSIS-SVD（`launch.json` / `project.yaml` 指定，或由已安装的 Ozone 目录 `/opt/SEGGER` / `C:\Program Files\SEGGER` 自动匹配）。支持按位段查看与修改寄存器。
  - 对于带读副作用的寄存器（如 `DR`、`RDR`、`RXDR` 等），默认跳过自动读取并显示 `<not read: reading changes it>`，需要时可右键手动读取（该屏蔽列表结合 `readAction` 与常见外设名称推断，可能存在个别遗漏）。

---

## 7. 项目配置（`.jlinkd/project.yaml`）

在工程根目录放置 `.jlinkd/project.yaml`（全部字段可选）：

```yaml
target:
  family: stm32h7          # 可选；内置 stm32f1 / stm32g4 / stm32h7 / stm32h74，通常可由 device 自动识别

debug:
  freezeOnHalt: [iwdg1, wwdg1, tim1]   # 暂停时冻结的外设，可用名称见 devices/*.yaml
  flashBreakpoints: auto               # auto | never | always
  maskInterruptsOnStep: true           # 单步时屏蔽中断
  vectorCatch: true                    # 捕获 Fault 异常

sampling:
  hss: { periodUs: 1000 }
  watchRateHz: 5

svd: path/to/device.svd                # 相对工程目录
visualizers:                           # 用 Lua 自定义 C++ 模板类型的显示方式
  - .jlinkd/visualizers.lua
rtt:
  config: .jlinkd/rtt.yaml
```

---

## 8. 命令行（CLI）

`jlinkd` 二进制可脱离 VS Code 独立使用。未指定 `--device` 时默认值为 `STM32H723VG`（可通过 `--device` 或 `JLINKD_DEVICE` 覆盖）：

```bash
jlinkd info --device STM32G474RE                          # 查看探针与目标状态（--no-target 仅查探针）
jlinkd flash firmware.elf --device STM32G474RE            # 增量烧录（--no-run 烧录后保持暂停）
jlinkd read uwTick --elf firmware.elf --device STM32G474RE # 不暂停读取变量或表达式
jlinkd read 0x20000000 --count 16 --device STM32G474RE    # 读取内存
jlinkd tasks --elf firmware.elf --backtrace --device STM32G474RE # 查看 FreeRTOS 任务与调用栈
jlinkd symbols firmware.elf --lookup main                 # 离线查看 ELF 符号与缓存耗时（无需连接探针）
```

常用全局选项：`--interface <swd|jtag>`、`--speed <khz>`、`--serial <sn>`、`--dll <path>`、`--log <filter>`，完整参数请执行 `jlinkd --help`。

---

## 9. 缓存位置、环境变量与后台进程

`jlinkd` 写入的符号索引缓存（`index/`）、后台日志（`serve.log`）以及默认录制目录（`recordings/`）存放在统一缓存目录下：

- **Linux**：`$XDG_CACHE_HOME/jlinkd` 或 `~/.cache/jlinkd`
- **Windows**：系统「文档」目录下的 `jlinkd` 文件夹
- **自定义目录**：设置环境变量 `JLINKD_CACHE_DIR` 即可覆盖上述默认位置

常用环境变量：

| 环境变量 | 作用 |
|---|---|
| `JLINKD_CACHE_DIR` | 自定义缓存、日志与录制文件目录 |
| `JLINKD_DLL` | 指定 `libjlinkarm.so` 或 `JLink_x64.dll` 路径 |
| `JLINKD_DEVICE` | 命令行默认器件名 |
| `JLINKD_LOG` | 命令行日志过滤级别（如 `info`、`jlink=trace`；后台 `serve.log` 固定为 `info`） |

> 调试时会拉起一个后台 `jlinkd` 服务进程持有探针（监听本机 `127.0.0.1:47110` 和 `127.0.0.1:47111`），调试结束且无网页连接约 30 秒后自动退出。同一时间只允许一个调试会话控制目标；升级插件或修改环境变量后，请等待旧后台进程退出或重载 VS Code 窗口再重新调试。

---

## 10. 常见问题与已知限制

### 常见问题

| 现象 | 处理建议 |
|---|---|
| 提示找不到 J-Link 库 | 确认已安装 SEGGER J-Link 驱动；绿色版或自定义路径请在 `launch.json` 设置 `dll` 或传 `--dll` / `JLINKD_DLL` |
| 连不上目标板 | 先运行 `jlinkd info --device <器件>` 排查；检查 SWD 接线与供电，或适当降低 `speed` |
| 断点显示灰色未绑定 | 确认 ELF 是当前源码编译且带 `-g`，或该行在优化后没有生成对应机器指令 |
| 提示另一个调试会话正在控制目标 | 关闭其他 VS Code 窗口中的调试会话 |
| Timeline 没有波形数据 | 确认 `Sample` 采样率不是 `Off`、目标处于运行态，且变量为全局或静态变量 |
| 信号提示放不下 HSS | HSS 带宽与块数受探针硬件上限限制，可减少信号、降低采样率，或改用 `poll` |
| RTT 没有输出 | 确认已配置 `.jlinkd/rtt.yaml`、固件启用了 RTT，且 ELF 中存在 `_SEGGER_RTT` |
| Windows 烧录路径报错 | Windows 下 J-Link DLL 只接受系统当前代码页（`CP_ACP`）路径，避免路径包含当前代码页无法表示的字符 |
| 修改环境或升级插件未生效 | 彻底重启 VS Code 窗口，并等待约 30 秒让旧后台进程空闲退出 |
| 其他异常定位 | 查看缓存目录下的 `serve.log` 与 VS Code 调试控制台输出 |

### 已知限制与硬件验证情况

- **实测范围**：已在 **STM32F103C8**、**STM32G4 系列**、**STM32H723** + J-Link V9 下完成实际调试与验证；**STM32H74x/H75x** 配置已内置（参考 `RM0433`），但受限于手头硬件，尚未完成充分实机验证。其他 STM32 型号也能连接，但可能缺少内置 `DBGMCU` / 外设冻结配置。
- 芯片读保护解锁或 J-Link 固件升级时是否会弹出阻塞对话框尚未验证（后台模式下建议保持探针固件正常）。
- 如果使用「Run to main」但固件在进入 `main` 前就死循环，需要手动点暂停或停止来中断等待。
- `.jlrec` 录制文件暂时不能在网页界面中回放。

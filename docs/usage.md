# Haltd 使用手册

`Haltd` 是面向 STM32（ARM Cortex-M）的 J-Link 调试后端、VS Code 扩展与命令行工具。除了常规的断点与单步，它还支持在目标不停机的情况下实时读写变量（Live Watch）、绘制高速波形（Timeline），以及结构化解码 RTT 数据。

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

发布包中的 `.vsix` 已内置对应平台的 `haltd`（或 `haltd.exe`）可执行文件：

```bash
# Linux x64
code --install-extension haltd-<版本>-linux-x64.vsix

# Windows x64
code --install-extension haltd-<版本>-win32-x64.vsix
```

也可以在 VS Code 扩展面板右上角的 `...` 菜单中选择「从 VSIX 安装」。如果使用了 VS Code 配置文件（Profile），可在命令后加上 `--profile <名称>`。

> 如需把 `haltd` 单独放到命令行使用，可将压缩包中的 `haltd` / `haltd.exe` 放到 `PATH` 中（扩展默认也会查找自身内置目录与 `~/.cargo/bin/haltd(.exe)`）。

---

## 2. 快速开始

1. 编译固件时保留 DWARF 调试信息（gcc / clang 开启 `-g`，`-O2` 优化同样可用）。
2. 用 VS Code 打开工程目录，在 `.vscode/launch.json` 中添加：

   ```json
   {
     "version": "0.2.0",
     "configurations": [
       {
         "name": "J-Link (Haltd)",
         "type": "haltd",
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
| `device` | 是 | J-Link 器件名称，如 `STM32F103C8`、`STM32G474RE`、`STM32H723VG`（在 `launch.json` 中支持自动补全、悬停查看内核/Flash/RAM 与匹配 profile、未知型号 Warning 提示，也可运行命令 `Haltd: Select Device` 快捷选择） |
| `elf` | 是 | 带调试信息的 ELF 固件路径 |
| `interface` | | `SWD`（默认）或 `JTAG` |
| `speed` | | 接口时钟（kHz），默认 `4000`；也可写 `"auto"` |
| `flash` | | `changed`（默认，只擦写变化的扇区）、`always`、`never` |
| `runToMain` | | 启动后运行到 `main` 停下 |
| `stopOnEntry` | | 启动后停在入口指令（优先级高于 `runToMain`） |
| `preLaunchTask` | | 调试前运行的构建任务，需与 `tasks.json` 的 `label` 一致；`F6` 也使用它 |
| `svd` | | CMSIS-SVD 文件路径，用于 Peripherals 视图（缺省时尝试从已安装的 Ozone 目录匹配） |
| `project` | | 项目配置文件路径，默认 `${workspaceFolder}/.haltd/project.yaml`，没有也可以 |
| `dll` | | 显式指定 `libjlinkarm.so` 或 `JLink_x64.dll` 的路径（多版本共存或绿色版时使用） |
| `serial` | | 连接多台探针时指定序列号（数字或字符串） |
| `rtt` | | RTT 自动解析选项：`{ "autoDecode": true, "autoTimeline": true }`（均默认 `true`），可控制未在 `rtt.yaml` 配置的通道是否自动按 ELF 结构体/文本解码及自动加入 Timeline |

**附加到正在运行的固件（Attach）**：把 `request` 改成 `"attach"`，不会复位也不会烧录。可用字段为 `device`、`elf`、`interface`、`speed`、`project`、`svd`、`dll`、`serial`、`rtt`。

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
- **硬件断点用完时**会自动改用 Flash 断点（需要探针具备 FlashBP 许可）；若不想写 Flash，可在 `.haltd/project.yaml` 中关闭。

**数据断点（Watchpoint）**：在 Variables 里右键变量选择「Break on Value Change」，支持读、写、读写触发并可附带条件。注意监视长度必须是 2 的幂且按自身地址对齐。

**Fault 自动捕获与译码**：HardFault、BusFault、MemManage、UsageFault 默认开启捕获，可在 Breakpoints 视图开关。触发后会弹出 `CFSR` / `HFSR` / `MMFAR` / `BFAR` / `AFSR` 译码说明，并将调用栈直接定位到出错代码行（注：不精确总线错误 `IMPRECISERR` 无法精确指向触发指令）。

**周期计数**：每次停下时，状态栏会显示距离上一次停下经过的 `DWT_CYCCNT` 周期数与换算时间（暂停期间挂起的中断在恢复后会执行并计入其中）。

---

## 4. 不停机观察（Live Watch & Timeline）

以下功能基于 AHB-AP / HSS，不需要暂停目标。

### Live Watch

位于调试侧栏。在源码或 Variables 中右键「Add to Live Watch」，或点击视图标题栏 `+` 添加表达式。调试期间原生 Watch 视图会隐藏并将「Add to Watch」接管到 Live Watch（可通过设置项 `haltd.replaceWatchView: false` 关闭）。

- 刷新率支持 `0–5 Hz` 每行独立设置（`0` 表示仅在目标暂停时刷新）。
- 结构体、数组与指针可逐层展开，支持多选，展开后的成员可直接加入 Timeline。
- 支持直接修改目标内存值，并将表达式列表保存到 `.haltd/state.json`。
- 指针表达式只在添加或 Reload 时解析一次地址，运行期间指针自身变化不会自动重新寻址。

**Live Watch 与调试快捷操作**（仅在 `haltd` 调试会话期间生效，可在 VS Code Keyboard Shortcuts 中自定义）：

| 快捷键 | 作用域 | 效果 |
|---|---|---|
| `Ctrl+G`（macOS `Cmd+G`） | 源码编辑器 / Live Watch | 将选中变量加入 Timeline（HSS）（调试结束后恢复原生「转到行」） |
| `Ctrl+H`（macOS `Cmd+H`） | 源码编辑器 | 将选中表达式或光标所在变量加入 Live Watch |
| `Del`（macOS `Backspace`） | Live Watch 视图 | 删除选中监视项 |
| `F2` / `Enter` | Live Watch 视图 | 编辑可写标量值（结构体按 `Enter` 展开/折叠） |
| `Ctrl+R`（macOS `Cmd+R`） | Live Watch 视图 | 设置选中项刷新率（0–5 Hz） |
| `Ctrl+C`（macOS `Cmd+C`） | Live Watch 视图 | 复制选中项为 `expr = value` |
| `Ctrl+Shift+T`（macOS `Cmd+Shift+T`） | 调试会话内 | 打开 Timeline 面板 |

> 注：受限于 VS Code 扩展 API，原生 Variables 视图无法通过快捷键读取当前焦点行，Variables 视图仍通过右键菜单加入 Live Watch 或 Timeline。

### Timeline

采用参考 SEGGER Ozone Timeline / Data Graph 的多轨道同步示波器布局：每条轨道左侧为独立图例与控制栏（含来源标识 `HSS` / `POLL` / `RTT` 与数值读数），右侧为波形画布，底部共用同一条时间轴与可拖拽游标手柄（`S` / `A` / `B`）。点击状态栏 Timeline 按钮、按 `Ctrl+Shift+T` 或执行 `Haltd: Open Timeline` 打开，也可通过 <http://127.0.0.1:47110/timeline> 在浏览器访问。

**添加信号与采样率**：
- 在源码或 Live Watch 按 `Ctrl+G`（或右键「Add to Timeline (HSS)」）、在顶部输入表达式回车，或将左下角 `RTT` 抽屉中的解码信号直接拖拽到任意轨道或空白区新建轨道；结构体与数组会自动展开（默认上限 64 路）。请使用全局或静态变量，局部变量无法持续采样。
- **选好信号后，需在工具栏 `Sample` 下拉框选择采样率（`20 Hz` ~ `1 kHz`）才会开始采集 HSS/Poll**；选 `Off` 时暂停采样（RTT 自动解码信号在固件输出时即会到达）。

| 采样模式 | 说明 |
|---|---|
| `auto` | 优先走 HSS，超出探针带宽预算的部分自动转轮询 |
| `hss` | J-Link 高速采样（J-Link V9 最高 1 kHz，受块数与字节预算限制） |
| `poll` | 主机轮询读取，最高 100 Hz |
| `rtt` | 来自 RTT 解码通道的信号 |

**鼠标操作（Ozone 交互逻辑）**：

| 操作 | 效果 |
|---|---|
| 滚轮 | 横向滚动时间窗 |
| `Shift` + 滚轮 | 缩放鼠标所在轨道的 Y 轴 |
| `Ctrl` + 滚轮 | 以鼠标位置为中心缩放时间轴 |
| 左键拖动 / 中键拖动 | 上下左右自由平移（纯纵向拖动保持实时跟随，横向拖动暂停跟随） |
| `Shift` + 左键拖动 | 仅上下平移当前轨道 Y 轴（保持时间轴实时跟随） |
| `Ctrl` + 左键拖动 | 框选时间区间放大 |
| 单击 / 双击轨道或时间轴 | 在该位置放置临时采样游标 `S`（虚线，图例与波形交点显示当前值，悬停时显示 `Δt`） |
| 拖动游标线或底部手柄 | 移动采样游标 `S`、测量游标 `A` / `B` 或固定标签 `M1`, `M2`… |
| 右键菜单 | Zoom to Fit、Y 自动缩放、放置 `A`/`B`/`S`、添加/删除/清除标签（含 `S`）、清除游标、改色、移动到其他轨道/新建轨道、隐藏/独显、删除信号、导出可见 CSV |
| 拖动图例信号 / RTT 信号 | 拖到其他轨道改变分组，拖到空白区域新建轨道 |
| 拖动轨道底边框 | 调整该轨道高度（自动记忆）；拖动图例右边界可调整图例栏宽度 |

**键盘快捷键**：

| 快捷键 | 效果 |
|---|---|
| `Space` | 切换跟随 / 暂停 |
| `←` / `→` | 后退 / 前进 1 格（1 Division） |
| `Ctrl+←` / `Ctrl+→` | 后退 / 前进 1 屏 |
| `Home` / `End` | 跳到最早记录 / 最新位置并恢复跟随 |
| `+` / `-` | 放大 / 缩小时间刻度 |
| `Ctrl+0` | 缩放以适配全部历史区间（Fit） |
| `↑` / `↓` | 选择上一条 / 下一条轨道 |
| `Del` | 删除当前选中的信号 |
| `A` / `B` | 在鼠标悬停处放置游标 `A` / `B`（底部 `Cursors` 抽屉与状态栏显示 `Δt`、`1/Δt`、`ΔV`、`dV/dt`） |
| `Esc` | 关闭弹窗或清除游标 `S` / `A` / `B` |
| `H` / `S` | 隐藏或显示选中信号 / 独显选中信号（Solo） |
| `Y` | 恢复当前选中轨道的 Y 轴自动缩放 |
| `/` / `?` | 聚焦表达式输入框 / 显示快捷键与鼠标帮助 |

**其他面板功能**：
- 顶部概览条显示全部历史与暂停/异常事件，点击或拖动可快速跳转。
- 工具栏可直接对目标执行运行（`▶`）、暂停（`❚❚`）、复位（`⟲`）与 `Fit`。
- 支持 `Grouped`（按轨道分组）/ `Split`（每路独立轨道）切换、`Bands`（Min/Max 包络）、状态栏 `Events` 事件跳转、导出可见窗口 CSV、保存 PNG 截图、深色/浅色/高对比主题切换（`Auto` 自动跟随 VS Code）。
- 支持录制 `.hlrec` 文件（`Record` 按钮或命令 `Haltd: Start/Stop Recording`）；当前网页端暂不支持直接回放 `.hlrec`。

### 状态与统计

状态栏实时显示连接状态、HSS 实际采样率与丢点计数，点击（或执行 `Haltd: Show Statistics`）可查看详细统计信息。

---

## 5. RTT 终端与结构化解码

只要在 ELF 中包含 `_SEGGER_RTT` 符号或提供 `.haltd/rtt.yaml`，即可启用 RTT；未检测到 `_SEGGER_RTT` 且未配置 `rtt.yaml` 时不会主动扫描 RAM，以免抢占 SWD 带宽影响 HSS。

- 状态栏 `RTT` 按钮可打开通道 0 终端并支持下行输入。
- 命令 `Haltd: Open RTT Signals` 可打开网页端 RTT 终端与解码信号列表（`http://127.0.0.1:47110/rtt`）。

### 零配置自动解码（`rtt.autoDecode` / `rtt.autoTimeline`）

未写在 `.haltd/rtt.yaml` 中的上行通道默认按以下规则自动解码（`rtt.yaml` 中显式配置的通道始终优先级最高）：

1. **按 ELF 结构体自动解码（`auto-struct`）**：当固件调用 `SEGGER_RTT_ConfigUpBuffer` 设置的通道名形如 `Type`、`struct:Type` 或 `Type@0xA55A`（支持 `ns::Type` 与 `typedef struct`），且 ELF 中存在该结构体/类/联合体定义时，自动按 DWARF 布局解码各数值字段为 `<通道名>.<字段>` 信号：
   - 带 `@0x<十六进制>` 后缀（如 `TelemetryFrame@0xA55A`）时使用同步字对齐与丢帧重同步（配合结构体里的 `sync` / `seq` 字段）。
   - 无 `@0x..` 且结构体不含 `sync` 字段时按 `sizeof(Type)` 连续切分；**固件必须使用 `SEGGER_RTT_MODE_NO_BLOCK_SKIP` 模式**（避免半帧截断，RTT 状态栏会给出提示）。
   - 若结构体中存在 `sync` 字段但通道名未附带 `@0x<值>`，会回退为文本模式并在 RTT 状态中报错提示。
2. **文本键值自动提取（`auto-text`）**：其余上行通道保留完整文本终端输出，同时按行自动提取 `name=value` 或 `name: value` 形式的数值（忽略 `0x...` 十六进制与 `1.2.3` 版本号），生成 `<通道名>.<name>` 信号；每通道最多自动追踪 32 个去重信号名。
3. **自动加入 Timeline**：自动解码出的信号默认归入 `rtt:<通道名>` 轨道；当 `autoTimeline` 为 `true`（默认）时自动绘制到 Timeline；设为 `false` 时仅在 RTT 信号列表更新数值，可手动点击或拖拽加入 Timeline。

可在 `launch.json` 中按配置项开关（也可写在 `.haltd/project.yaml`）：

```jsonc
"rtt": {
  "autoDecode": true,
  "autoTimeline": true
}
```

### 手动配置 `.haltd/rtt.yaml` 示例：

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
    script: .haltd/decode_motor.lua
  4:
    name: metrics
    type: text-regex
    pattern: 'err=(?P<err>-?\d+(\.\d+)?)'
```

> 提示：RTT 与 1 kHz HSS 同开且日志吞吐较高时，会共享 SWD 总线带宽，可能挤占 HSS；如需满速 HSS，可提高 SWD `speed` 或减小 RTT 输出频率。

---

## 6. FreeRTOS 与 SVD 外设视图

- **FreeRTOS Tasks 视图**：固件使用 FreeRTOS 时自动识别，基于 DWARF 布局解析 TCB 与链表（支持 CM3 / CM4F），显示任务状态、优先级、剩余栈空间及各任务独立调用栈。默认设置 `haltd.rtos.threadView` 为 `currentOnly`（仿 SEGGER Ozone，调试器 Call Stack 仅高亮当前执行线程/中断上下文，其余任务在 FreeRTOS Tasks 面板查看；如需在 Call Stack 展开全部任务，可改为 `currentFirst`）。
- **与 SEGGER Ozone 同时 attach**：支持两方同时连接到同一块开发板/探针；当 Ozone 恢复运行、暂停或单步时，Haltd 会通过 CoreSight (`DHCSR`/`DCRDR`) 同步当前状态并刷新真实调用栈，若检测到已被另一调试器占用的硬件比较器也会在控制台发出提醒。
- **Peripherals 视图**：需配置 CMSIS-SVD（`launch.json` / `project.yaml` 指定，或由已安装的 Ozone 目录 `/opt/SEGGER` / `C:\Program Files\SEGGER` 自动匹配）。支持按位段查看与修改寄存器。
  - 对于带读副作用的寄存器（如 `DR`、`RDR`、`RXDR` 等），默认跳过自动读取并显示 `<not read: reading changes it>`，需要时可右键手动读取（该屏蔽列表结合 `readAction` 与常见外设名称推断，可能存在个别遗漏）。

---

## 7. 项目配置（`.haltd/project.yaml`）

在工程根目录放置 `.haltd/project.yaml`（全部字段可选）：

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
  - .haltd/visualizers.lua
rtt:
  config: .haltd/rtt.yaml
  autoDecode: true
  autoTimeline: true
```

---

## 8. 命令行（CLI）

`haltd` 二进制可脱离 VS Code 独立使用。未指定 `--device` 时默认值为 `STM32H723VG`（可通过 `--device` 或 `HALTD_DEVICE` 覆盖）：

```bash
haltd info --device STM32G474RE                          # 查看探针与目标状态（--no-target 仅查探针，--dump-stack <FILE> 导出现场栈与寄存器快照）
haltd flash firmware.elf --device STM32G474RE            # 增量烧录（--no-run 烧录后保持暂停）
haltd read uwTick --elf firmware.elf --device STM32G474RE # 不暂停读取变量或表达式
haltd read 0x20000000 --count 16 --device STM32G474RE    # 读取内存
haltd tasks --elf firmware.elf --backtrace --device STM32G474RE # 查看 FreeRTOS 任务与调用栈（支持 --dump-stack <FILE>）
haltd symbols firmware.elf --lookup main                 # 离线查看 ELF 符号与缓存耗时（无需连接探针）
haltd devices --filter STM32H7 --json                    # 查询 J-Link 支持器件列表、内核与 Flash/RAM 大小（仅需 J-Link 库，无需连接探针）
```

常用全局选项：`--interface <swd|jtag>`、`--speed <khz>`、`--serial <sn>`、`--dll <path>`、`--log <filter>`，完整参数请执行 `haltd --help`。

---

## 9. 缓存位置、环境变量与后台进程

`haltd` 写入的符号索引缓存（`index/`）、后台日志（`serve.log`）以及默认录制目录（`recordings/`）存放在统一缓存目录下：

- **Linux**：`$XDG_CACHE_HOME/haltd` 或 `~/.cache/haltd`
- **Windows**：系统「文档」目录下的 `haltd` 文件夹
- **自定义目录**：设置环境变量 `HALTD_CACHE_DIR` 即可覆盖上述默认位置

常用环境变量：

| 环境变量 | 作用 |
|---|---|
| `HALTD_CACHE_DIR` | 自定义缓存、日志与录制文件目录 |
| `HALTD_DLL` | 指定 `libjlinkarm.so` 或 `JLink_x64.dll` 路径 |
| `HALTD_DEVICE` | 命令行默认器件名 |
| `HALTD_LOG` | 命令行日志过滤级别（如 `info`、`jlink=trace`；后台 `serve.log` 固定为 `info`） |

> 调试时会拉起一个后台 `haltd` 服务进程持有探针（监听本机 `127.0.0.1:47110` 和 `127.0.0.1:47111`），调试结束且无网页连接约 30 秒后自动退出。同一时间只允许一个调试会话控制目标；升级插件或修改环境变量后，请等待旧后台进程退出或重载 VS Code 窗口再重新调试。

---

## 10. 常见问题与已知限制

### 常见问题

| 现象 | 处理建议 |
|---|---|
| 提示找不到 J-Link 库 | 确认已安装 SEGGER J-Link 驱动；绿色版或自定义路径请在 `launch.json` 设置 `dll` 或传 `--dll` / `HALTD_DLL` |
| 连不上目标板 | 先运行 `haltd info --device <器件>` 排查；检查 SWD 接线与供电，或适当降低 `speed` |
| 断点显示灰色未绑定 | 确认 ELF 是当前源码编译且带 `-g`，或该行在优化后没有生成对应机器指令 |
| 提示另一个调试会话正在控制目标 | 关闭其他 VS Code 窗口中的调试会话 |
| Timeline 没有波形数据 | 确认 `Sample` 采样率不是 `Off`、目标处于运行态，且变量为全局或静态变量 |
| 信号提示放不下 HSS | HSS 带宽与块数受探针硬件上限限制，可减少信号、降低采样率，或改用 `poll` |
| RTT 没有输出 | 确认已配置 `.haltd/rtt.yaml`、固件启用了 RTT，且 ELF 中存在 `_SEGGER_RTT` |
| Windows 烧录路径报错 | Windows 下 J-Link DLL 只接受系统当前代码页（`CP_ACP`）路径，避免路径包含当前代码页无法表示的字符 |
| 修改环境或升级插件未生效 | 彻底重启 VS Code 窗口，并等待约 30 秒让旧后台进程空闲退出 |
| 其他异常定位 | 执行命令面板 `Haltd: Collect Diagnostics` 复制脱敏诊断包，并查看输出面板 `Haltd`、缓存目录下的 `serve.log` 与调试控制台输出 |

### 已知限制与硬件验证情况

- **实测范围**：已在 **STM32F103C8**、**STM32G4 系列**、**STM32H723** + J-Link V9 下完成实际调试与验证；**STM32H74x/H75x** 配置已内置（参考 `RM0433`），但受限于手头硬件，尚未完成充分实机验证。其他 STM32 型号也能连接，但可能缺少内置 `DBGMCU` / 外设冻结配置。
- 芯片读保护解锁或 J-Link 固件升级时是否会弹出阻塞对话框尚未验证（后台模式下建议保持探针固件正常）。
- 如果使用「Run to main」但固件在进入 `main` 前就死循环，需要手动点暂停或停止来中断等待。
- `.hlrec` 录制文件暂时不能在网页界面中回放。

---

## 11. 从 jlinkd 迁移

项目自 `0.7.0` 起由 `jlinkd` 更名为 `Haltd`，涉及以下配置与环境变更：

1. **工程配置目录**：将工程根目录下的 `.jlinkd/` 重命名为 `.haltd/`（包含 `project.yaml`、`rtt.yaml`、`state.json` 及自定义的 Lua 脚本路径）。
2. **`launch.json`**：将配置中的 `"type": "jlinkd"` 改为 `"type": "haltd"`，如自定义了 `project` 字段或用户设置 `jlinkd.*`，一并改为 `.haltd/...` 与 `haltd.*`。
3. **环境变量**：`JLINKD_CACHE_DIR` / `JLINKD_DLL` / `JLINKD_DEVICE` / `JLINKD_LOG` 统一更名为 `HALTD_CACHE_DIR` / `HALTD_DLL` / `HALTD_DEVICE` / `HALTD_LOG`。
4. **VS Code 扩展**：扩展标识已变更为 `WaterKeyqwq.haltd-embedded`（与旧的 `jlinkd.jlinkd` 不同）。升级前请先在 VS Code 中卸载旧的 `jlinkd` 扩展，再安装新的 `haltd` `.vsix`。
5. **缓存目录**：默认缓存目录已变更为 `~/.cache/haltd`（Linux）或「文档」目录下的 `haltd`（Windows）。旧的 `jlinkd` 缓存目录可直接删除，符号索引会按需自动重建。
6. **录制文件兼容**：新录制的波形文件扩展名为 `.hlrec`，旧版录制的 `.jlrec` 文件仍可由 `Haltd` 正常读取。

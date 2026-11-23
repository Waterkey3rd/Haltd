# jlinkd 用户手册（Windows x64）

jlinkd 是通过 J-Link 调试 STM32 的 VS Code 插件和命令行工具。除了常规的断点和单步，它还能在目标运行时查看变量、绘制波形、接收 RTT 日志。

## 1. 安装

先装好：

- Windows 10 / 11 x64
- [SEGGER J-Link 软件包](https://www.segger.com/downloads/jlink/)，含 USB 驱动，jlinkd 从注册表找到它
- VS Code 1.90 或更新版本

不需要 Rust、Node 或 VC 运行库。

安装发布包里的插件（插件已内置 `jlinkd.exe`）：

```
code --install-extension jlinkd-<版本>-win32-x64.vsix
```

也可以在 VS Code 扩展面板右上角的 `...` 菜单里选“从 VSIX 安装”。如果你使用 VS Code 的配置文件，命令后面加 `--profile <名称>`。

## 2. 快速开始

1. 编译固件时保留调试信息（gcc 的 `-g`）。
2. 用 VS Code 打开工程目录，在 `.vscode/launch.json` 里添加：

   ```json
   {
     "version": "0.2.0",
     "configurations": [
       {
         "name": "J-Link (jlinkd)",
         "type": "jlinkd",
         "request": "launch",
         "device": "STM32F103C8",
         "interface": "SWD",
         "speed": 4000,
         "elf": "${workspaceFolder}/build/Debug/firmware.elf",
         "flash": "changed",
         "runToMain": true,
         "preLaunchTask": "CMakeBuild"
       }
     ]
   }
   ```

3. 接好探针和目标板，按 F5。

### launch.json 字段

| 字段 | 必填 | 说明 |
|---|---|---|
| `device` | 是 | J-Link 的器件名，如 `STM32F103C8` |
| `elf` | 是 | 带调试信息的固件 |
| `interface` | | `SWD`（默认）或 `JTAG` |
| `speed` | | 接口时钟，单位 kHz，默认 4000；也可以写 `"auto"` |
| `flash` | | `changed`（默认，只烧变化的部分）、`always`、`never` |
| `runToMain` | | 启动后运行到 `main` 停下 |
| `stopOnEntry` | | 启动后停在入口 |
| `preLaunchTask` | | 调试前运行的构建任务，要和 `tasks.json` 里的 `label` 一致。F6 也用它 |
| `svd` | | CMSIS-SVD 文件路径，用于 Peripherals 视图 |
| `project` | | 项目配置文件，默认 `.jlinkd/project.yaml`，没有也可以 |
| `dll` | | `JLink_x64.dll` 的路径，用于绿色版或指定某个 J-Link 版本。不写时用已安装的 J-Link 软件 |
| `serial` | | 接了多个探针时，用这个序列号选择。不写时由 J-Link 库选择 |

**接到正在运行的固件**：把 `request` 改成 `attach`，不复位也不烧录。可用字段是 `device`、`elf`、`interface`、`speed`、`project`、`svd`、`dll`、`serial`。

## 3. 基本调试

| 操作 | 按键 |
|---|---|
| 开始 / 继续 | F5 |
| 单步跳过 / 进入 / 跳出 | F10 / F11 / Shift+F11 |
| **编译并重新加载** | F6 |
| 重启 | Ctrl+Shift+F5 |

**F6（Build & Reload）** 先运行 `preLaunchTask`，再只烧写变化的扇区，断点和 Live Watch 会按新固件自动重新绑定。编译失败时不会改动目标。

Variables 视图、悬停和 Watch 都可以查看变量，变量值可以直接修改。Variables 里还有 Registers 作用域。Memory 视图可以读写内存，但不能写 Flash。

### 断点

在行号左侧点击设置，右键选 Edit Breakpoint 可以设置以下三种（可组合）：

| 类型 | 示例 | 效果 |
|---|---|---|
| 条件 | `count > 10 && g_state.items[2].gain != 0` | 表达式为真才停 |
| 命中次数 | `5`、`>5`、`>=5`、`<5`、`<=5`、`%10` | 恰好第 5 次；大于、小于；每 10 次 |
| 日志点 | `tick={uwTick} mode={g_state.mode}` | 打印到调试控制台，不停下 |

**表达式**支持算术、比较、逻辑、位运算、标量类型转换、成员、下标和解引用，不能调用固件函数。

使用时注意：

- 无符号减法不回绕：`count` 为 0 时 `count - 1` 得 -1。需要 C 的结果就写 `(uint32_t)(count - 1)`。
- 条件写错（比如符号不存在）时会停下并说明原因。
- 条件由电脑判断，目标每次命中都会真的停一下，单次约 6 ms。不要放在高频中断里，那里用 Timeline 或数据断点。
- 单步经过日志点时不输出。

**硬件断点用完时**自动改用 Flash 断点（需要探针带 FlashBP 许可）。Flash 断点会写 Flash，不想用可以在 `project.yaml` 里关闭，见第 7 节。

**数据断点**：在 Variables 里右键变量，选 Break on Value Change，读、写、读写都可以断，也可以带条件。Cortex-M3 有 4 个比较器；监视长度必须是 2 的幂且按自身对齐。

**Fault**：HardFault、BusFault、MemManage、UsageFault 默认捕获，可以在 Breakpoints 视图里开关。发生时弹出原因说明，并跳到出错的源码行。不精确总线错误（IMPRECISERR）的位置不准。

**周期计数**：每次停下，状态栏显示与上一次停下之间的周期数和时间，可以直接用来测耗时。暂停期间挂起的中断会算进去，所以结果比固件自测略大。

## 4. 运行时观察

以下功能不需要暂停目标。

### Live Watch

在调试侧栏。在代码或 Variables 里右键选 Add to Live Watch，或者用视图标题栏的 `+`。调试期间，VS Code 自带的 Add to Watch 也会进入这里，自带的 Watch 视图会隐藏（设置项 `jlinkd.replaceWatchView` 可以关闭）。

- 刷新率 0–5 Hz，每行单独设置；0 表示只在目标停下时刷新。
- 结构体和指针可以逐层展开，展开的成员可以加入 Timeline。
- 右键 Set Value 改值。
- 指针表达式只在添加和重新加载时解析一次地址，运行中指针变了不会跟随。

### Timeline

像示波器一样绘制变量。点状态栏的 Timeline 按钮打开，或运行命令 `jlinkd: Open Timeline`；命令 `jlinkd: Open Timeline in Browser` 用浏览器打开，地址是 <http://127.0.0.1:47110/timeline>。

**添加信号**：在代码或 Live Watch 里右键 Add to Timeline (HSS)，或者在页面顶部输入表达式后回车。结构体和数组会展开成多路，上限 64 路。应使用全局或静态变量，局部变量不能持续采样。

采样方式：

| 方式 | 说明 |
|---|---|
| `auto` | 优先 HSS，放不下的改用轮询 |
| `hss` | J-Link 高速采样，J-Link V9 最高 1 kHz，容量有限 |
| `poll` | 轮询，最高 100 Hz |
| `rtt` | 来自 RTT 解码的信号，见第 5 节 |

**Sample** 选择采样率（Off、20 Hz 到 1 kHz）。**信号都加好后要选一个采样率才开始采样**，选 Off 时不采样。

| 操作 | 效果 |
|---|---|
| 滚轮 | 以鼠标位置为中心缩放时间 |
| Shift + 滚轮 | 缩放当前子图的 Y 轴 |
| 左键拖动 | 平移时间，同时暂停跟随 |
| Shift + 拖动 | 平移当前子图的 Y 轴 |
| Ctrl + 拖动 | 框选时间范围放大 |
| 单击 | 依次放置游标 A、B，侧栏显示 Δt 和各信号差值 |
| 双击 | 子图 Y 轴恢复自动缩放 |
| 空格 | 跟随 / 暂停 |
| ← / → | 前后移动时间窗 |
| `-` / `+` | 缩放时间 |
| `A` / `B` | 在鼠标位置放置游标 A / B |
| Esc | 清除游标 |
| `/` | 聚焦表达式输入框 |
| `?` | 显示快捷键帮助 |

其他功能：

- 概览条可以点击或拖动来跳转历史。
- 工具栏可以暂停、继续、复位目标。
- 侧栏可以显示 / 隐藏、独显（S）信号，改颜色，改分组。相同 group 的信号画在同一张子图里；Grouped 按钮切换为每个信号一条轨道。
- Min/Max 切换包络显示。
- 状态栏的 Events 列出暂停、复位、重新加载、Fault 等事件，点击可跳到对应时刻。
- Export CSV 导出当前可见时间窗，PNG 保存截图，Theme 切换主题（默认跟随 VS Code）。
- Record 录制为 `.jlrec`（也可用命令 `jlinkd: Start/Stop Recording`）。目前界面上还不能回放。

### 统计信息

状态栏显示连接状态和 HSS 实际速率、丢点数，点击（或运行 `jlinkd: Show Statistics`）查看详细统计。

## 5. RTT

固件需要集成 SEGGER RTT，ELF 里有 `_SEGGER_RTT` 符号就会自动启用，不需要配置。

- 状态栏的 RTT 按钮打开通道 0 终端，支持输入。
- 命令 `jlinkd: Open RTT Signals` 打开通道和解码信号页面。

要解码二进制数据，新建 `.jlinkd/rtt.yaml`：

```yaml
controlBlock: auto                 # auto 取 _SEGGER_RTT 的地址，也可以直接写地址
channels:
  0: { name: log, type: text, ansi: true }
  1: { name: sysview, type: systemview, file: build/trace.SVDat }
  2:
    name: telemetry
    type: struct
    sync: 0xA55A
    struct: TelemetryFrame         # ELF 中的类型名，改结构体后不用改配置
    timestamp: { field: cyccnt, clockHz: 72000000 }
    timeline:
      - jointPos[0..5]
      - { expr: loopUs, group: timing }
  3: { name: motor, type: lua, script: .jlinkd/decode_motor.lua }
  4: { name: t, type: text-regex, pattern: 'a=(?P<a>\d+)' }
```

RTT 和 HSS 同时大流量运行时共用 SWD，可能挤占 HSS。需要全量 HSS 时，提高 `speed` 或减小 RTT 流量。

## 6. FreeRTOS 和外设

**FreeRTOS Tasks 视图**：固件使用 FreeRTOS 时自动识别，显示任务状态、优先级和栈剩余字节，展开可以看调用栈。每个任务也是一个线程，要切换到某个任务，在 Call Stack 视图里点击。

**Peripherals 视图**：需要 SVD。在 `launch.json` 的 `svd` 里指定；Ozone 装在 `C:\Program Files\SEGGER` 下时可以自动查找。显示外设、寄存器和位段，可以改值。

数据寄存器（DR、RDR 等）读取可能丢数据或清标志，不会自动读，显示 `<not read: reading changes it>`，需要时右键 Read Register Now。这份名单是按名称推断的，可能有遗漏。

## 7. 项目配置（可选）

`.jlinkd/project.yaml`：

```yaml
debug:
  freezeOnHalt: [iwdg, wwdg, tim2]   # 暂停时冻结这些外设，名称见芯片配置
  flashBreakpoints: auto             # auto | never | always
  maskInterruptsOnStep: true         # 单步时屏蔽中断
  vectorCatch: true                  # 捕获 Fault
sampling:
  hss: { periodUs: 1000 }
  watchRateHz: 5
svd: path/to/device.svd              # 相对工程目录
visualizers: [.jlinkd/visualizers.lua]   # 用 Lua 自定义类型的显示方式
rtt:
  config: .jlinkd/rtt.yaml
```

## 8. 命令行

发布包里的 `jlinkd.exe` 可以单独使用。默认器件是 `STM32H723VG`，其他芯片要加 `--device`，或设置环境变量 `JLINKD_DEVICE`。

```
jlinkd info --device STM32F103C8                         探针和目标信息
jlinkd flash firmware.elf --device STM32F103C8           烧录，--no-run 烧录后保持暂停
jlinkd read uwTick --elf firmware.elf --device STM32F103C8   不暂停读取变量
jlinkd read 0x20000000 --count 16 --device STM32F103C8   读取内存
jlinkd tasks --elf firmware.elf --backtrace --device STM32F103C8   FreeRTOS 任务和调用栈
jlinkd symbols firmware.elf --lookup main                检查符号，不需要探针
```

通用选项：`--interface`、`--speed`、`--serial`（多个探针时选序列号）、`--dll`。`jlinkd --help` 查看全部命令。

## 9. 文件位置和环境变量

符号索引缓存、后台日志 `serve.log` 和默认的录制文件都放在系统“文档”文件夹下的 `jlinkd` 目录。

| 环境变量 | 作用 |
|---|---|
| `JLINKD_CACHE_DIR` | 换一个缓存目录 |
| `JLINKD_DLL` | 仅命令行：指定 `JLink_x64.dll` 的路径 |
| `JLINKD_DEVICE` | 仅命令行：默认器件 |
| `JLINKD_LOG` | 仅命令行：日志级别，如 `info`、`debug`（后台日志固定为 `info`） |

```
setx JLINKD_CACHE_DIR D:\jlinkd-cache
```

设置后要**关闭所有 VS Code 窗口**再打开。

调试时后台有一个 `jlinkd.exe` 进程占用探针，不用时约 30 秒后自动退出，只监听本机端口 47110、47111。同一时间只能有一个调试会话控制目标。

## 10. 常见问题

| 现象 | 处理 |
|---|---|
| 提示找不到 J-Link 库 | 确认用安装程序装了 SEGGER 软件包。绿色版在 `launch.json` 里用 `dll` 指定 `JLink_x64.dll`，命令行用 `--dll` |
| 连不上目标 | 先运行 `jlinkd info --device <器件>`；检查 SWD 接线和供电，降低 `speed` |
| 断点是灰色的 | 确认 ELF 是当前代码编译的且带 `-g`；那一行可能没有生成代码 |
| 提示另一个调试会话正在控制目标 | 关掉其他 VS Code 窗口里的调试会话 |
| Timeline 没有数据 | 确认 Sample 不是 Off，目标正在运行，变量是全局或静态的 |
| 信号提示放不下 HSS | HSS 容量有限，减少信号或降低采样率，或者用 `poll` |
| RTT 没有内容 | 确认固件集成了 RTT，且 ELF 里有 `_SEGGER_RTT` |
| 烧录路径报错 | 路径含系统代码页无法表示的字符，换一个路径 |
| 升级插件后还是旧行为 | 重载窗口，等旧后台进程约 30 秒退出后再调试 |
| 环境变量不生效 | 关闭所有 VS Code 窗口再打开 |
| 其他问题 | 查看 `serve.log` 和调试控制台的输出 |

## 11. 已知限制

- 实测范围是 STM32F103C8 + J-Link V9。内置 STM32F1、STM32G4、STM32H72x/H73x、STM32H74x/H75x 的芯片配置，除 F103 外都未全面验证。其他 STM32 也能连接，但没有 DBGMCU 设置和外设冻结。
- 芯片读保护解锁、J-Link 固件升级时是否会弹出对话框未验证。
- `.jlrec` 录制暂时不能在界面上回放。

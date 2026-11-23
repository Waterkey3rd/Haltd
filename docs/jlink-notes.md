# J-Link 实测特性与注意事项

在开发和实机测试 `jlinkd` 的过程中，发现 SEGGER J-Link 探针与官方 DLL 存在不少文档未写明、或与直觉相反的行为。本文把这些实测结论整理出来，一方面解释 `jlinkd` 内部为什么要这样设计，另一方面也方便使用其他型号探针或芯片的开发者参考和补充测试。

> 以下数据主要基于 **J-Link V9（固件 2021-05-07 / DLL V8.96~V9.80）**，在 **STM32F103C8**、**STM32G4 系列** 与 **STM32H723** 上实测得到。不同硬件版本的 J-Link（如 V10/V11/PLUS/PRO/OB）在带宽和上限上可能存在差异。

---

## 1. SWD 时钟是离散分频

- 在 J-Link V9 上实测，SWD 接口时钟只能取 **`12 MHz / n`**（如 `12 MHz`、`6 MHz`、`4 MHz`、`3 MHz`…）。
- 传入非整除频率（例如 `8 MHz`、`15 MHz`）时，库内部会自动向下匹配到支持的档位；`jlinkd` 在状态与信息输出中展示的是 J-Link 实际生效的速率。

---

## 2. HSS（高速采样）能力与真实行为

### 实测上限（J-Link V9）

- `GetCaps` 返回：`MaxBlocks = 10`、`MaxFreq = 1000 Hz`。
- 每条 HSS 记录结构为 **`4 字节头 + 各采样块数据按顺序拼接`**（`flags=0` 时头部为序号，`flags=1` 时为微秒时间戳）。
- 在 4 MHz SWD 下，单帧带宽预算约为：`字节数 + 8 × 块数 ≤ 150`（随 SWD 时钟频率和采样周期按比例缩放）；极限连续读取吞吐实测约 `225 KB/s`。
- 相邻变量之间的内存间隔如果不超过 `32 字节`，合并成同一个块比拆成两个块更省头开销。

### 关键发现：超预算不丢点也不报错，而是悄悄降频

- 实测当请求的 HSS 字节数或块数超过探针吞吐上限时，**J-Link DLL 不会返回错误，也不会直接报告丢点，而是自动把实际采样周期拉长**。
- 因此 `jlinkd` 的采样规划器会在启动采样前按预算公式检查总开销：放得下的信号走 HSS，超预算的信号自动降级为轮询（`poll`），避免把整组 HSS 拖慢。

### HSS 时间戳的特性

- HSS 记录附带的微秒时间戳**在内核暂停期间会停止计数**，恢复运行后继续累加。
- 实测探针本地时钟与主机单调时钟之间存在约 `+0.37%` 的速率偏差，且 USB 批量读取的投递延迟在 `0 ~ 11 ms` 之间抖动。
- `jlinkd` 通过识别空读窗口（连续约 100 ms 无数据即视为目标暂停/重锚定）+ 滑动窗口最小偏移估计 + 实测 `3 ms` 投递延迟补偿，将 HSS 时间戳还原到与主机一致的时间轴上。

---

## 3. 数据断点（Watchpoint）与 `JLINKARM_SetWP` 的副作用

- **重要实测坑**：调用过 `JLINKARM_SetWP` 之后，J-Link DLL 会在每次调用 `go`（包括复位后）把 Cortex-M 的 `DEMCR` 寄存器改写成只保留 `TRCENA`，从而**静默清掉 `VC_HARDERR` / `VC_BUSERR` / `VC_MEMERR` / `VC_STATERR` 等异常向量捕获位**。
- 这会导致 HardFault / BusFault 自动拦截在无感知的情况下失效；即使在 `go` 之后补写 `DEMCR`，仍存在约 `1 ms` 的窗口可能漏掉刚恢复运行就触发的 Fault。
- 因此 `jlinkd` **不使用 `JLINKARM_SetWP`**，而是直接读写 CoreSight DWT 比较器寄存器（`DWT_COMP` / `DWT_MASK` / `DWT_FUNCTION`）。
  - 监视字节数必须是 2 的幂且按自身长度对齐（这是 Cortex-M DWT 硬件本身的约束）。
  - 读取 `DWT_FUNCTION.MATCHED` 位时会清除匹配标志，因此命中判断在读取时一次完成并缓存。

---

## 4. Flash 断点与擦写时机

- 调用 J-Link 设置或清除 Flash 断点的 API 本身几乎是瞬时的（清除仅几微秒），但 **J-Link 会把实际擦写 Flash 的动作延迟到下一次 `go` 时才执行**（实测带 Flash 断点变更的 `go` 耗时约 `180+ ms`）。
- Build & Reload（`F6`）重新烧录 ELF 前，`jlinkd` 会先撤掉全部活动断点，避免残留在 Flash 中的软件断点指令影响扇区内容比对。
- 实测烧录 ELF 时，对于 `.isr_vector` 与 `.text` 等段之间的对齐空隙，J-Link 可能跳过写入而保留擦除后的 `0xFF`。

---

## 5. RTT 与 HSS 共存特性

- **复位/重启时的状态残留**：目标复位后重新启动 RTT，实测 J-Link DLL 可能继续返回上一次会话的旧通道数，第二次调用 `START` 传入新的控制块地址会被忽略，且库内部不校验控制块魔数 ID。
  - `jlinkd` 的处理方式是：目标复位时先擦除 RAM 中旧的 `_SEGGER_RTT` 魔数，并使用 `JLINK_RTTERMINAL_CMD_SETADDR`（`SetRTTAddr`）显式指定控制块地址，等固件重新初始化后再读取。
- **全 RAM 扫描会饿死 HSS**：未配置 RTT 时不主动在 RAM 中扫描 `_SEGGER_RTT`，因为扫描过程会持续占用 SWD 带宽导致 HSS 严重失速。
- **HSS + RTT 同开**：两者在 SWD 上复用同一条物理通道，RTT 高流量输出会挤占 HSS 时间片；实测在控制好 RTT 输出量时，1 kHz HSS 与连续 RTT 日志可以做到零丢点同时工作。

---

## 6. 线程安全、总线读错误与 Windows 编码

- **DLL 线程模型**：J-Link 动态库未声明线程安全，所有访问必须严格串行化在同一个线程内。`jlinkd` 通过单线程 `ProbeActor` 加 5 级优先队列保证运行时高频采样不会阻塞单步、暂停和断点操作；同一时刻也只能有一个进程持有同一个 J-Link 探针。
- **读不存在外设地址**：在 STM32F103C8 上实测读取不存在的外设地址时，J-Link 返回读错误而不会挂死 SWD 总线（界面上显示 `<unreadable>`）。
- **Windows DLL 路径编码**：实测 Windows 下 `JLink_x64.dll` 的文件路径接口（如 `JLINK_DownloadFile`）不接受 UTF-8，传入非 ASCII 的 UTF-8 路径会返回 `-268`。`jlinkd` 在 Windows 下自动将路径转换为系统当前 ANSI 代码页（`CP_ACP`）；如果路径中包含系统代码页无法表示的字符，会直接拒绝并给出提示。

---

## 7. 邀请实测与基准测试工具（`jlbench`）

由于作者个人硬件条件有限，目前仅在 **J-Link V9 + STM32F103C8 / STM32G4 / STM32H723** 上做过实际验证。仓库内自带了一个独立基准测试工具 `jlbench`（位于 `bins/jlbench/`），可以用来测量你手头探针与芯片的真实能力（SWD 分频、内存读延迟、HSS 极限吞吐、Flash 断点耗时等）：

```bash
# 安全只读基准（不暂停内核、不写 Flash）
cargo run --release -p jlbench -- --device <你的器件名>

# 完整基准（含暂停、单步与 Flash 断点测试）
cargo run --release -p jlbench -- --device <你的器件名> --allow-halt --allow-flash --elf <firmware.elf>
```

如果你手头有其他 STM32 系列（如 STM32H74x/H75x、F4、L4、U5 等）或更新型号的 J-Link 探针（V10/V11/V12/PRO），欢迎跑一下实测结果或补充 `devices/*.yaml` 器件配置并提交反馈！

//! Structured error mapping for DAP and WebSocket JSON-RPC responses.

use std::fmt;

use engine::session::EngineError;
use probe_api::ProbeError;
use serde::Serialize;
use symbols::SymbolError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub id: u32,
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub causes: Vec<String>,
    pub actions: Vec<String>,
    #[serde(skip_serializing)]
    pub show_user: bool,
}

impl Diagnostic {
    fn new(
        id: u32,
        code: &'static str,
        message: impl Into<String>,
        detail: Option<String>,
        causes: &[&str],
        actions: &[&str],
        show_user: bool,
    ) -> Self {
        Self {
            id,
            code,
            message: message.into(),
            detail: detail.filter(|d| !d.trim().is_empty()),
            causes: causes.iter().map(|s| (*s).to_owned()).collect(),
            actions: actions.iter().map(|s| (*s).to_owned()).collect(),
            show_user,
        }
    }

    /// Short user-facing summary for VS Code's native DAP error popup.
    /// Keeps `message` intact when `show_user` is false so clients checking
    /// expected messages (such as `"the target is running"`) still match.
    pub fn user_summary(&self) -> String {
        if !self.show_user || self.causes.is_empty() {
            self.message.clone()
        } else {
            format!("{} — 可能原因：{}", self.message, self.causes[0])
        }
    }

    pub fn from_message(msg: &str) -> Self {
        let lower = msg.to_ascii_lowercase();

        if msg == "the target is running" {
            return Self::new(
                1006,
                "E_TARGET_RUNNING",
                msg,
                None,
                &["目标 CPU 正在运行，该操作只能在暂停状态下执行"],
                &["先暂停目标CPU（Pause），然后再重试"],
                false,
            );
        }

        if lower.contains("another debug session is already controlling") {
            return Self::new(
                1008,
                "E_SESSION_BUSY",
                msg,
                None,
                &["已有另一个 VS Code 调试会话正在控制当前后台进程"],
                &["停止另一处调试会话后再重新连接"],
                true,
            );
        }

        if lower.contains("no target is connected") {
            return Self::new(
                1002,
                "E_PROBE_OPEN",
                msg,
                None,
                &[
                    "尚未启动调试会话，或先前的会话已释放探针",
                    "J-Link 探针连接已断开",
                ],
                &["先通过 F5 启动或附加调试会话，或检查 J-Link USB 连接"],
                true,
            );
        }

        if lower.contains("external debugger") || lower.contains("ozone") {
            return Self::new(
                1009,
                "E_EXTERNAL_DEBUGGER",
                msg,
                None,
                &["同时连接的外部调试器（如 Ozone）修改了目标运行状态或硬件断点"],
                &["确认外部调试器状态；必要时暂停并重新同步上下文"],
                true,
            );
        }

        if lower.contains("all hardware breakpoints in use")
            || lower.contains("all watchpoints in use")
            || lower.contains("every dwt comparator is already in use")
        {
            return Self::new(
                1012,
                "E_HW_BP_LIMIT",
                msg,
                None,
                &[
                    "芯片硬件断点（FPB）或数据观察点（DWT）比较器已占满",
                    "同时连接的外部调试器（如 Ozone）可能占用了部分比较器",
                ],
                &["删除部分现有断点/观察点，或启用 Flash 断点"],
                true,
            );
        }

        if lower.contains("launch configuration needs \"elf\"") {
            return Self::new(
                1011,
                "E_CONFIG",
                msg,
                None,
                &[
                    "launch.json 中缺少 \"elf\" 字段（注意：Haltd 使用 \"elf\" 而非 \"program\"）",
                ],
                &["在 .vscode/launch.json 中添加 \"elf\": \"${workspaceFolder}/build/firmware.elf\""],
                true,
            );
        }

        if lower.contains("launch configuration needs \"device\"")
            || lower.contains("unknown interface")
            || lower.contains("invalid speed")
            || lower.contains("invalid serial")
            || lower.contains("no svd file")
        {
            return Self::new(
                1011,
                "E_CONFIG",
                msg,
                None,
                &["launch.json 调试配置字段缺失或格式无效"],
                &["检查 .vscode/launch.json 的 device、interface、speed、serial 或 svd 配置（可用 Haltd: Select Device）"],
                true,
            );
        }

        if lower.contains("cannot read target memory") || lower.contains("memory access") {
            return Self::new(
                1007,
                "E_MEM_ACCESS",
                msg,
                None,
                &[
                    "目标地址未映射、指针为空/越界，或触发总线错误",
                    "对应外设 RCC 时钟未开启或 MCU 处于低功耗休眠",
                ],
                &["检查变量指针与地址有效性，确认目标外设时钟已使能"],
                false,
            );
        }

        if lower.contains("no symbol named")
            || lower.contains("cannot parse expression")
            || lower.contains("is ambiguous")
            || lower.contains("no member")
            || lower.contains("not in memory")
            || lower.contains("unknown watch")
            || lower.contains("unknown variable")
        {
            return Self::new(
                1010,
                "E_SYMBOL",
                msg,
                None,
                &[
                    "表达式语法有误、符号名拼写错误或需要带上命名空间",
                    "变量被编译器（如 -O2）优化掉，或为局部变量无法持续采样",
                ],
                &["核对变量名与作用域；Live Watch 和 Timeline 请使用全局或静态变量"],
                false,
            );
        }

        if lower.contains("hss")
            && (lower.contains("not supported")
                || lower.contains("capacity")
                || lower.contains("budget"))
        {
            return Self::new(
                1013,
                "E_HSS_UNAVAILABLE",
                msg,
                None,
                &["当前 J-Link 探针或固件不支持高速采样（HSS），或 HSS 块/带宽预算已满"],
                &["将采样模式切换为 auto（自动降级轮询）或 poll，或减少 HSS 信号数量"],
                true,
            );
        }

        Self::new(
            1099,
            "E_INTERNAL",
            msg,
            None,
            &["操作执行失败，请查看详细错误信息与输出日志"],
            &["运行命令 \"Haltd: Collect Diagnostics\" 收集日志与诊断报告排查"],
            true,
        )
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<&ProbeError> for Diagnostic {
    fn from(error: &ProbeError) -> Self {
        let message = error.to_string();
        match error {
            ProbeError::Load(detail) => Self::new(
                1001,
                "E_DLL_NOT_FOUND",
                message,
                Some(detail.clone()),
                &[
                    "未安装 SEGGER J-Link Software，或默认安装目录中找不到动态库",
                    "launch.json 中的 \"dll\" 路径不存在或与平台位数（64 位）不匹配",
                ],
                &[
                    "安装 SEGGER J-Link Software（Linux: libjlinkarm.so / Windows: JLink_x64.dll）",
                    "或在 launch.json 中通过 \"dll\" 指定正确的动态库绝对路径",
                ],
                true,
            ),
            ProbeError::NotConnected | ProbeError::ActorStopped => Self::new(
                1002,
                "E_PROBE_OPEN",
                message,
                None,
                &[
                    "J-Link 探针未连接、USB 松动，或后台探针线程已停止",
                    "探针被其他程序独占，或连接期间意外掉线",
                ],
                &[
                    "检查 J-Link USB 连接与指示灯，重新启动调试会话",
                    "关闭独占探针的程序（如 J-Link Commander / STM32CubeProgrammer）",
                ],
                true,
            ),
            ProbeError::Call { func, code, detail } => {
                let clean = detail.strip_prefix(": ").unwrap_or(detail);
                let full_detail = if clean.is_empty() {
                    format!("{func} returned {code}")
                } else {
                    format!("{func} returned {code}: {clean}")
                };
                if *func == "JLINKARM_OpenEx" || *func == "JLINKARM_EMU_SelectByUSBSN" {
                    Self::new(
                        1002,
                        "E_PROBE_OPEN",
                        message,
                        Some(full_detail),
                        &[
                            "J-Link USB 未插好，或指定的 serial 序列号找不到对应探针",
                            "探针已被其他软件独占，或 Linux 下缺少 SEGGER udev 权限规则",
                        ],
                        &[
                            "检查 USB 线缆与 launch.json 的 \"serial\" 配置",
                            "关闭其他占用探针的软件；Linux 请确认已安装 99-jlink.rules 并有权限访问",
                        ],
                        true,
                    )
                } else if *func == "JLINK_DownloadFile" {
                    Self::new(
                        1005,
                        "E_FLASH",
                        message,
                        Some(full_detail),
                        &[
                            "目标芯片开启了 Flash 读/写保护（RDP / WRP / PCROP）",
                            "launch.json 中的 \"device\" 型号与实际芯片 Flash 大小或布局不符",
                            "烧录过程供电不稳或 ELF 链接地址超出 Flash 范围",
                        ],
                        &[
                            "核对 \"device\" 型号并检查链接脚本中的 Flash 起始地址与大小",
                            "使用 J-Link Commander / STM32CubeProgrammer 解除读保护后重试",
                        ],
                        true,
                    )
                } else if *func == "JLINKARM_SetBPEx"
                    && (clean.to_ascii_lowercase().contains("breakpoint")
                        || clean.to_ascii_lowercase().contains("no hw"))
                {
                    Self::new(
                        1012,
                        "E_HW_BP_LIMIT",
                        message,
                        Some(full_detail),
                        &[
                            "硬件断点比较器已全部占用（Cortex-M 通常为 4~8 个）",
                            "Flash 断点不可用或当前处于受限状态",
                        ],
                        &["清理不需要的断点，或确认未使用禁止 Flash 断点的配置"],
                        true,
                    )
                } else {
                    Self::new(
                        1003,
                        "E_CONNECT_CORE",
                        message,
                        Some(full_detail),
                        &[
                            "目标板未上电（VTref 电压异常）或 SWDIO/SWCLK/GND 接线松动",
                            "\"device\" 芯片型号拼写错误，或固件复用了 SWD 引脚 / 进入了低功耗睡眠模式",
                            "SWD 通信速率（speed）过高导致信号完整性不足",
                        ],
                        &[
                            "检查目标板供电与 SWD 接线（VTref / SWDIO / SWCLK / GND）",
                            "使用 \"Haltd: Select Device\" 核对器件名，或将 \"speed\" 降至 1000 kHz 再试",
                            "若芯片处于睡眠或关闭 SWD 状态，尝试按住复位键后再连接",
                        ],
                        true,
                    )
                }
            }
            ProbeError::Memory { addr, len, detail } => {
                let clean = detail.strip_prefix(": ").unwrap_or(detail);
                let full_detail = if clean.is_empty() {
                    format!("{len} bytes @ {addr:#010x}")
                } else {
                    format!("{len} bytes @ {addr:#010x}: {clean}")
                };
                Self::new(
                    1007,
                    "E_MEM_ACCESS",
                    message,
                    Some(full_detail),
                    &[
                        "访问的目标地址未映射、越界或指针无效，触发总线错误（BusFault / HardFault）",
                        "访问外设寄存器前未使能对应的 RCC 时钟，或目标处于低功耗模式",
                    ],
                    &[
                        "核对地址是否在有效 RAM/Flash/外设区间内",
                        "若读取外设寄存器，先确认其总线时钟已开启",
                    ],
                    true,
                )
            }
            ProbeError::Unsupported(what) => Self::new(
                1099,
                "E_UNSUPPORTED",
                message,
                Some(what.clone()),
                &["当前 J-Link DLL 版本、固件或目标芯片不支持该能力"],
                &["升级 SEGGER J-Link Software 或检查目标架构支持情况"],
                true,
            ),
            ProbeError::Other(msg) => {
                let lower = msg.to_ascii_lowercase();
                if lower.contains("cannot open j-link") || lower.contains("no j-link with serial") {
                    Self::new(
                        1002,
                        "E_PROBE_OPEN",
                        message,
                        Some(msg.clone()),
                        &[
                            "J-Link 探针未插入 USB、找不到指定序列号，或正被其他程序独占",
                            "Linux 缺少 udev 权限规则或 USB 驱动异常",
                        ],
                        &[
                            "检查 USB 连接与 launch.json 的 \"serial\" 字段",
                            "关闭其他占用探针的程序后重试",
                        ],
                        true,
                    )
                } else if lower.contains("device =") {
                    Self::new(
                        1003,
                        "E_CONNECT_CORE",
                        message,
                        Some(msg.clone()),
                        &[
                            "launch.json 中的 \"device\" 名称不被 J-Link DLL 识别",
                            "或 J-Link 器件库版本过旧",
                        ],
                        &["在 launch.json 中运行 \"Haltd: Select Device\" 选择标准芯片型号"],
                        true,
                    )
                } else {
                    Self::from_message(msg)
                }
            }
        }
    }
}

impl From<ProbeError> for Diagnostic {
    fn from(error: ProbeError) -> Self {
        Self::from(&error)
    }
}

impl From<&SymbolError> for Diagnostic {
    fn from(error: &SymbolError) -> Self {
        let message = error.to_string();
        match error {
            SymbolError::Io { .. } | SymbolError::Object(_) | SymbolError::Dwarf(_) => Self::new(
                1004,
                "E_ELF",
                message.clone(),
                Some(message),
                &[
                    "ELF 文件不存在、路径错误或未完成编译（请确认 launch.json 使用 \"elf\" 字段）",
                    "文件损坏、不是小端 ARM ELF，或编译时未开启调试信息（-g）",
                ],
                &[
                    "核对 launch.json 中的 \"elf\" 路径并先执行固件构建",
                    "确认编译器开启了 -g（推荐 -gdwarf-5 或 -g）",
                ],
                true,
            ),
            SymbolError::Memory(detail) => Self::new(
                1007,
                "E_MEM_ACCESS",
                message,
                Some(detail.clone()),
                &["读取指针或内存变量时目标返回内存访问失败（空指针、野指针或未使能区域）"],
                &["检查指针值是否合法以及所在内存区域是否可读"],
                false,
            ),
            SymbolError::Parse(_)
            | SymbolError::NotFound(_)
            | SymbolError::Ambiguous { .. }
            | SymbolError::Type(_) => Self::new(
                1010,
                "E_SYMBOL",
                message.clone(),
                Some(message),
                &[
                    "表达式语法有误，或该符号在 ELF 调试信息中不存在",
                    "符号存在多个重名候选（需要加命名空间），或局部变量在当前帧被优化",
                ],
                &["核对表达式拼写；必要时使用完整命名空间（如 fw::g_state）"],
                false,
            ),
        }
    }
}

impl From<SymbolError> for Diagnostic {
    fn from(error: SymbolError) -> Self {
        Self::from(&error)
    }
}

impl From<EngineError> for Diagnostic {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Probe(probe) => Self::from(probe),
            EngineError::Symbol(symbol) => Self::from(symbol),
            EngineError::Other(msg) => Self::from_message(&msg),
        }
    }
}

impl From<String> for Diagnostic {
    fn from(message: String) -> Self {
        Self::from_message(&message)
    }
}

impl From<&str> for Diagnostic {
    fn from(message: &str) -> Self {
        Self::from_message(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_probe_load_to_dll_not_found() {
        let diag = Diagnostic::from(ProbeError::Load("missing libjlinkarm.so".to_owned()));
        assert_eq!(diag.code, "E_DLL_NOT_FOUND");
        assert!(diag.show_user);
        assert!(!diag.causes.is_empty());
        assert!(!diag.actions.is_empty());
    }

    #[test]
    fn maps_probe_open_errors() {
        let diag = Diagnostic::from(ProbeError::Other(
            "cannot open J-Link: No emulators connected via USB".to_owned(),
        ));
        assert_eq!(diag.code, "E_PROBE_OPEN");
        assert!(diag.show_user);

        let serial = Diagnostic::from(ProbeError::Other(
            "no J-Link with serial number 12345 found".to_owned(),
        ));
        assert_eq!(serial.code, "E_PROBE_OPEN");
    }

    #[test]
    fn maps_connect_and_flash_calls() {
        let connect = Diagnostic::from(ProbeError::Call {
            func: "JLINKARM_Connect",
            code: -1,
            detail: ": Could not connect to target.".to_owned(),
        });
        assert_eq!(connect.code, "E_CONNECT_CORE");
        assert!(connect
            .detail
            .as_deref()
            .unwrap()
            .contains("Could not connect"));

        let flash = Diagnostic::from(ProbeError::Call {
            func: "JLINK_DownloadFile",
            code: -2,
            detail: ": Flash write failed".to_owned(),
        });
        assert_eq!(flash.code, "E_FLASH");
        assert!(flash.show_user);
    }

    #[test]
    fn maps_elf_and_memory_errors() {
        let elf = Diagnostic::from(SymbolError::Object("bad magic".to_owned()));
        assert_eq!(elf.code, "E_ELF");
        assert!(elf.show_user);

        let probe_mem = Diagnostic::from(ProbeError::Memory {
            addr: 0x2000_0000,
            len: 4,
            detail: ": transfer error".to_owned(),
        });
        assert_eq!(probe_mem.code, "E_MEM_ACCESS");
        assert!(probe_mem.show_user);

        let sym_mem = Diagnostic::from(SymbolError::Memory("bus error".to_owned()));
        assert_eq!(sym_mem.code, "E_MEM_ACCESS");
        assert!(!sym_mem.show_user);
    }

    #[test]
    fn preserves_target_is_running_summary_for_expected_failures() {
        let diag = Diagnostic::from("the target is running");
        assert_eq!(diag.code, "E_TARGET_RUNNING");
        assert!(!diag.show_user);
        assert_eq!(diag.message, "the target is running");
        assert_eq!(diag.user_summary(), "the target is running");
    }

    #[test]
    fn user_summary_includes_primary_cause_when_show_user() {
        let diag = Diagnostic::from(ProbeError::Load("missing".to_owned()));
        let summary = diag.user_summary();
        assert!(summary.contains(&diag.message));
        assert!(summary.contains("可能原因："));
    }
}

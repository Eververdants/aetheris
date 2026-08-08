# aetheris — v1 核心服务设计文档

日期:2026-08-08
状态:待评审

## 1. 背景与目标

构建一款 Windows 游戏优化工具。核心硬约束:**工具自身资源占用第一**,Zero-Overhead + Native Only + Headless Default。

| 指标 | 目标 |
|---|---|
| 稳态内存 | 2–5 MB |
| 稳态 CPU | < 0.1%(空闲时趋近 0)|
| 冷启动 | < 100 ms |
| 游戏进程生命周期响应 | 事件驱动,无轮询 |

## 2. 范围

### v1 内 (本期)

- Rust 核心引擎,纯用户态,无 GUI、无内核驱动、无 overlay
- ETW 实时消费 `Microsoft-Windows-Kernel-Process`(进程 Start/Stop)
- Policy engine:规则 + 游戏模式双触发
- 四种优化动作:优先级、亲和性、QoS(Job Object CPU quota)、挂起/恢复、内存瘦身
- Named Pipe IPC + CLI 客户端(查询状态 / reload 配置)
- TOML 配置,mmap 加载
- 安全防护:受保护进程名单 + 显式 opt-in + graceful degradation

### v2+ (延期)

内核驱动(仅监控)、DXGI overlay、Win32 配置 UI、网络 QoS。

## 3. 架构总览

```
┌──────────────────────────────┐
│        Game Process           │
└──────────────┬───────────────┘
               │ ETW Kernel-Process 事件 (实时)
┌──────────────▼───────────────┐
│   aetheris-service (core)     │
│  ┌─────────────────────────┐  │
│  │ ETW Consumer (专用线程)   │  │  OpenTrace/ProcessTrace 阻塞循环
│  │   → SPSC channel         │  │
│  ├─────────────────────────┤  │
│  │ Policy Engine (主循环)    │  │  状态机 Normal↔GameBoostActive
│  │  Aho-Corasick 规则匹配    │  │  热路径零分配
│  ├─────────────────────────┤  │
│  │ Action Executor          │  │  SetPriorityClass / SetAffinity /
│  │                          │  │  JobObject QoS / NtSuspend / WS Trim
│  ├─────────────────────────┤  │
│  │ IPC Server (Named Pipe)  │  │  \\.\pipe\aetheris
│  └─────────────────────────┘  │
└──────────────┬───────────────┘
               │ bincode over named pipe
┌──────────────▼───────────────┐
│  aetheris-cli (查询/reload)    │
└──────────────────────────────┘
```

## 4. Workspace 布局

```
aetheris/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ aetheris-core/          # lib: 全部引擎逻辑
│  ├─ aetheris-service/       # bin: 组装 core,console 启动(后续接 windows-service)
│  └─ aetheris-cli/           # bin: named pipe 客户端
├─ aetheris.toml              # 示例配置
├─ docs/superpowers/specs/    # 本文件
└─ .gitignore
```

依赖原则:
- **无 tokio / 无异步运行时**。单线程事件循环 + 专用 ETW 线程 + 专用 pipe 线程。
- 所有堆分配在初始化完成;热路径(事件处理)零 allocation。
- 依赖保持极简,每个依赖记录 license 合规性。

## 5. 模块设计

### 5.1 ETW 层 (aetheris-core::etw)

- 实时 trace session,消费 `Microsoft-Windows-Kernel-Process` Provider(GUID `{22FB2CD6-0E7B-422B-A0C7-2FAD1FD0E716}`,已核实)。
- 专用线程:`StartTraceW` / `EnableTraceEx2` / `OpenTraceW` / `ProcessTraceW` 阻塞循环;回调收到 event 后投递到 SPSC channel。
- 解码:TDH `TdhGetEventInformation` 解析 payload(PID、进程名、ParentPID);schema 按 Provider GUID/EventId/Version 缓存,解码开销 = 一次 cache probe(参照 ferrisetw schema-locator 设计)。
- 主循环通过 channel 拉取事件,更新进程表 + 触发 policy engine。
- 事件 ID(已核实,多参考源一致):Process Start=1、Process Stop=2、Thread Start=3、Thread Stop=4。关键字 `WINEVENT_KEYWORD_PROCESS = 0x10`。默认不订阅 `PerfInfo`/`CPUTime` 类(高量事件)。
- Buffer 调优(参照 windows-erg 实证):128 KB buffer,min 5 / max 25,1s flush;空闲时批量消费 + 100ms sleep,零忙等。
- 需要 elevation(管理员)才能开 kernel trace session。
- **兜底**:若 ETW session 打开失败(权限/被禁用),**fail-safe**:不做任何优化、记录明确错误并退出(安全优先)。vnite 有 WMI/polling 降级路径,但违反零轮询原则,不采用。

### 5.2 前台检测 (游戏模式触发)

- `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 监听前台窗口变化 → 事件回调获得前台进程 → 匹配 `[game]` 名单。
- 事件驱动,无轮询。
- **实现要点**:out-of-context WinEvent 钩子需要消息泵。专用线程跑 `GetMessage/DispatchMessage` 循环宿主钩子,回调转发到 channel 交给主循环。
- 仅当游戏进入前台时激活 GameBoost;若配置 `[game].boost_on_start = true`,则进程 Start 即激活,不依赖前台。

### 5.3 Policy Engine (aetheris-core::policy)

状态机:

```
Normal ──(game start / foreground)──> GameBoostActive
GameBoostActive ──(game exit / lost foreground)──> Normal
```

- **Normal 态**:仅执行 `[[rule]]` 常驻规则(按进程名匹配,进程 Start 时应用)。
- **GameBoostActive 态**:
  1. 对 `[[background]]` 名单内已运行进程:快照其优先级/亲和性/挂起状态 → 应用配置动作(降优先级、锁核、QoS 限额、挂起、内存瘦身)。
  2. 名单内新进程 Start 时同样处理。
- **退出 GameBoost**:对已快照进程逐一恢复原状态。
- 规则匹配:配置在加载时编译为 Aho-Corasick 自动机(进程名子串/精确匹配),热路径仅字节比较。
- 进程表:SoA 布局,cache-line 对齐(`#[repr(align(64))]`),避免 false sharing。

### 5.4 Action Executor (aetheris-core::actions)

| 动作 | API | 备注 |
|---|---|---|
| 优先级 | `SetPriorityClass` | 后台 → BELOW_NORMAL / IDLE |
| 亲和性 | `SetProcessAffinityMask` | 后台锁核,释放性能核 |
| QoS | `SetInformationJobObject` / `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` | CPU quota,后台限流。**注意**:目标进程已在 Job 内时 attach 新 Job 失败(浏览器/游戏常发生)→ fallback `PROCESS_MODE_BACKGROUND_BEGIN`(Background Processing Mode,OS 自动降 I/O 与 CPU 抢占) |
| 挂起/恢复 | `NtSuspendProcess` / `NtResumeProcess` (ntdll) | 需 `SE_DEBUG` 权限,失败静默记录 |
| 内存瘦身 | `SetProcessWorkingSetSize(h,-1,-1)` | 仅对显式 opt-in 进程 |

执行器注意(参考 Priority 工具 + super-thread 实证):

- **权限舞蹈**:启动时 `AdjustTokenPrivileges` 开启 `SeIncreaseBasePriorityPrivilege`(提高优先级必需)、`SeDebugPrivilege`(挂起/访问其他会话与受保护进程必需)。
- **>64 逻辑核 / 混合架构陷阱**:经典 `SetProcessAffinityMask`(ULONG_PTR,单 group)>64 线程或 P/E 核机器上静默失效。亲和性动作走 group-aware:首用 `SetProcessDefaultCpuSetMasks`(Win11),或 `NtSetInformationProcess(ProcessGroupAffinity)`;仅常规场景回退 `SetProcessAffinityMask`。另注意 Win11 线程化进程坑:一旦线程调 `SetThreadGroupAffinity`,进程亲和性即被锁定。
- **IO/内存优先级**:提升到后台可再降 `ProcessIoPriority`(0x21)/`ProcessPagePriority`(0x27) 经 `NtSetInformationProcess`。

### 5.5 IPC (aetheris-core::ipc)

- Named Pipe:`\\.\pipe\aetheris`。
- 消息:length-prefixed,serde + bincode(结构简单,避免 JSON 解析开销)。
- 消息类型 v1:
  - `GetState` → `StateSnapshot`(当前模式、激活游戏、受管进程状态)
  - `ReloadConfig` → `ReloadResult`
  - `QueryProcess <name>` → `ProcessInfo`
- 专用 pipe 线程,非阻塞 accept,请求在主循环处理。

### 5.6 配置 (aetheris-core::config)

- 文件 `aetheris.toml`,memmap2 mmap 后 toml 解析;reload 重新 mmap。
- 结构:

```toml
[game]
boost_on_start = true
processes = ["steam_app_*.exe"]          # Aho-Corasick 模式

[[background]]
name = "browser.exe"
suspend = true                          # 显式 opt-in
priority = "below_normal"
affinity = { cores = [0,1] }
qos_cpu_quota = 30                       # % CPU quota (Job Object)
trim_memory = false

[[rule]]
name = "updater.exe"
priority = "idle"
```

- 内置受保护进程名单(硬编码 + 配置可追加):`csrss.exe`、`services.exe`、`smss.exe`、`wininit.exe`、`winlogon.exe`、`dwm.exe`、`system`、自身进程等。名单内进程永不挂起/瘦身/降优先级到 idle。

### 5.7 安全与 Graceful Degradation

- 挂起/内存瘦身 = 危险动作,必须规则显式 `suspend = true` / `trim_memory = true`,默认 `false`。
- 受保护进程名单第一优先级,任何规则不覆盖。
- 自排除:永不优化自身。
- **Graceful degradation**:主循环采样系统负载(`NtQuerySystemInformation` SystemProcessorPerformanceInformation);超过阈值(如 CPU > 85%)时:延迟非关键动作、跳过内存瘦身、自身检查节奏从短间隔拉到长间隔。绝不在高负载下与游戏抢资源。

## 6. 性能预算

- 初始化后热路径零堆分配(规则自动机 + SoA 进程表 + 复用 buffer)。
- ETW 事件处理 < 1 µs/事件平均;channel 复用预分配 ring buffer。
- 进程名匹配:预编译 Aho-Corasick,失败走最短回退,无字符串分配。
- 日志/遥测:内存 ring buffer + 异步落盘,主路径不阻塞。

## 7. 测试策略

- **单测**(aetheris-core):
  - 规则编译与匹配(大小写、通配、子串)
  - 状态机迁移(游戏 start/exit、失焦、重复进入)
  - 快照/恢复正确性(模拟进程)
  - 配置解析与合法性校验
- **集成测试**:
  - 起 dummy 进程 → 注入 GameBoost → 验证优先级/挂起生效 → 退出 → 验证恢复
  - ETW 冒烟:起进程 → 收到 Start 事件
- **手动验收**:跑 aetheris.toml 默认配置,后台跑浏览器 + 一个"游戏"占位进程,观察状态与动作。

## 8. 验收标准

1. 空闲态资源:内存 ≤ 5 MB,CPU 基准进程跑 60s 平均 < 0.1%。
2. 游戏启动 → 后台进程按规则被降级/挂起;游戏退出 → 全部恢复原状。
3. 受保护进程名单内进程永不被动作。
4. `aetheris-cli get-state` 输出正确状态快照;`reload` 生效。
5. 全部单测 + 集成测试通过。

## 9. 参考项目与 License 合规

调研 workflow(2026-08-08,25 agent)结论:

### 9.1 顶层参考(按用途)

| 用途 | 项目 | 语言 | License | 可抄什么 |
|---|---|---|---|---|
| ETW kernel 消费 | vnite (ximu3) | Rust | GPL-3.0 | **仅参考**架构:游戏场景下 ETW 进程监控落地(会话 GUID、`WINEVENT_KEYWORD_PROCESS 0x10`、Start=1/Stop=2、WMI 降级) |
| ETW 消费实现 | system_monitor (wuanzhuan) | Rust | MIT | 原生 Rust ETW session/buffer 搭建、MOF 解码、过滤引擎(clean-room 重写) |
| ETW 消费 API 设计 | ferrisetw | Rust | MIT OR Apache-2.0 | session 控制 + callback 消费 + schema 缓存解码架构 |
| ETW 封装实证 | windows-erg | Rust | MIT | `SystemProvider::Process` + buffer 调优 128KB/5-25/1s flush、批量消费 100ms sleep |
| 服务骨架 | gpu-power-limit-daemon | Rust | MIT | windows-service 0.8 + TOML 配置 + install/run/console 三合一,最接近目标形态 |
| 零开销服务循环 | uberdisplay vdd_service | Rust | MIT | 无 async、单线程、raw `CreateNamedPipeW/ConnectNamedPipe` accept 循环 + AtomicBool 停机 |
| 服务生命周期 | shawl | Rust | MIT | Job Object 整树回收、ctrl-C、restart 策略、thin LTO + strip |
| 事件驱动服务 | lg-ultragear-dimming-fix | Rust | MIT | message-only 窗口 + `WM_WTSSESSION_CHANGE`/`WM_DEVICECHANGE`,零 async 事件派发 |
| 优先级/亲和性 API | Priority (MScholtes) | C++ | MIT | 完整 CPU+内存+IO 优先级+亲和性 API 面 + 权限舞蹈(SeIncreaseBasePriorityPrivilege/SeDebugPrivilege) |
| >64 核亲和性 | super-thread | C | **无 license** | 仅参考:group-aware 亲和性、CPU Sets、Win11 线程化进程坑(不复制代码) |
| Job Object QoS 服务 | Process Governor | C# | MIT | 服务模式规则 + Job Object 限额(零开销,内核强制),registry 存储 |
| 游戏模式功能清单 | OpenGameBoost | Python | MIT | 功能全集(suspend/working-set flush/电源方案/GPU 优先级/网络),5s psll 轮询为反例 |
| 游戏优化引擎(Rust) | Winderust | Rust | GPL-3.0 | **仅参考**:foreground 检测、分层规则、GPU 调度优先级 `D3DKMTGetProcessSchedulingPriorityClass` |
| 服务 crate 官方示例 | windows-service-rs | Rust | MIT OR Apache-2.0 | `define_windows_service!`/notify/install 示例近乎逐字复制 |

### 9.2 License 判定

- **SAFE-TO-COPY**(MIT/Apache-2.0/MIT-OR-Apache-2.0):windows-rs、ferrisetw、ntapi、aho-corasick、memmap2、windows-erg、system_monitor、System Informer、Priority、Process Governor、OpenGameBoost、PowerPlanSwitcher、shawl、gpu-power-limit-daemon、windows-service-rs、wpa-mcp、lg-ultragear-dimming-fix、windows-capture、dxgi-capture-rs、dcomp-overlay、hudhook。
- **REFERENCE-ONLY**(copyleft,GPL/LGPL):vnite (GPL-3.0)、Winderust (GPL-3.0)、SpecialK (GPL-3.0)、NotCPUCores (LGPL-3.0)、RyzenAdj (LGPL-3.0)。只读架构/算法/API 面,不复制代码;记录每项设计决策的参考来源(paper trail)。
- **UNDETERMINED / 保留所有权利**:super-thread、ETWProcessMon2、StandbyCleanerLite、DirectXHook、DirectCompositionDirectX12Sample。不复制,仅当思路参考。
- Roammand:MPL-2.0(文件级弱 copyleft),当参考。

### 9.3 合规检查清单(每次合并执行)

1. 只从 SAFE-TO-COPY 项目复制代码。
2. 每个含复制代码的文件保留作者版权 + 许可声明;`THIRD_PARTY`/`NOTICE` 文件记录每笔借用。Apache-2.0 额外:含 license 副本、标记修改文件、保留 NOTICE、认可专利授权。
3. REFERENCE-ONLY 项目 clean-room:吸收架构/算法/API 面,自写实现,不复制实质代码表达;留存设计决策参考笔记。
4. LGPL 只允许动态链接单独分发的未修改副本;本项目视 LGPL 候选为纯参考,不吸收进二进制。
5. 每次依赖变更跑 `cargo-deny`,全部依赖 + 传递闭包无 copyleft;借用时刻重新核验 GPL/LGPL 项目的 license 状态(license 可能变更)。

## 10. 依赖清单 (已定)

| crate | 版本基准 | 用途 | license | 备注 |
|---|---|---|---|---|
| windows | 0.6x (feature-gated) | ETW 表面 (`Win32_System_Diagnostics_Etw`)、线程、进程、性能 | MIT OR Apache-2.0 | 基座 FFI;热路径直接用 raw ETW 函数 |
| ntapi | 0.4.x | `NtSuspendProcess`/`NtResumeProcess`/`NtSetInformationProcess`(IoPriority 0x21/PagePriority 0x27) | Apache-2.0 OR MIT | windows crate 覆盖不到时用 |
| aho-corasick | 1.1.x | 规则匹配自动机 | Unlicense OR MIT | 热路径零分配 |
| memmap2 | 0.9.x | 配置 mmap | MIT OR Apache-2.0 | |
| serde + toml | 最新 | 配置序列化 | MIT OR Apache-2.0 | |
| bincode | 2.x | IPC 序列化 | MIT OR Apache-2.0 | |
| ctrlc | 最新 | console 模式 ctrl-C(v1 调试) | MIT | |

不引入:tokio/async runtime、ferrisetw/windows-erg(参考其架构,自写 ETW 消费以保最大控制与最小依赖)、日志 crate(自写环形内存缓冲)。

**ETW 实现路径**:windows crate raw `StartTraceW → EnableTraceEx2 → OpenTraceW → ProcessTrace` 专用线程 + schema 缓存解码,参照 ferrisetw/system_monitor/windows-erg 三者的架构 clean-room 自写。

`cargo-deny` 纳入 CI 门禁。

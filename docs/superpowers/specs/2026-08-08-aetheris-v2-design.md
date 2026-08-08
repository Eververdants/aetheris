# aetheris v2 设计文档

日期:2026-08-08
状态:待评审
前置:v1 已交付(见 `2026-08-08-aetheris-core-service-design.md`)。v2 依赖 v1.1 硬化轨道(A 轨道)先落地 live IPC state。

## 1. 目标

在 v1 零开销核心上补齐三大块:
- **A. 引擎特性**:真实跨进程 CPU 上限(QoS)、网络 QoS 调整、standby 内存清理
- **B. Win32 配置 UI**:状态面板 + 规则编辑器,非驻留
- **C. 外部遥测 overlay**:DirectComposition,零注入

v2 内实现顺序 A → B → C,各自独立 plan。**v2.1**:内核驱动(WHQL,需 EV 证书)不含。

## 2. 全局约束(v2 继承 v1)

- 无 tokio / 无异步运行时(独立进程间除外)。
- 热路径零堆分配。独立进程(UI/overlay)非驻留,用完即退 / 隐藏即释放资源。
- 受保护进程名单绝对;挂起/瘦身/QoS 显式 opt-in。
- graceful degradation 继续生效。
- 依赖锁定 + cargo-deny 门禁;仅 MIT/Apache 依赖;copyleft 仅参考。
- 所有可逆动作必须可还原(registry/Job/权限),还原失败记日志不静默。

## 3. A. 引擎特性

### 3.1 Job Object QoS(真跨进程 CPU 上限)

目标:`qos_cpu_quota` 从 v1 的 no-op 变成真实上限。

机制:
- `CreateJobObjectW` + `AssignProcessToJobObject` + `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`(`ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP`,`CpuRate = percent * 100`,单位 0.01%)。
- **绝不设置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`**(关闭句柄不杀进程,安全)。
- `percent == 0` = 清限流:置 `ControlFlags = 0`(unlimited)。

清限流语义(修正 v1 缺陷):
- 游戏退出(`exit_game_mode`)→ 对每个 boosted 且带 QoS 的 pid 清限流,再从 Job 表移除条目。
- 服务 `Stop` → 同上,先清所有限流再退出。
- 进程 `Stop` 事件 → 移除该 pid 的 Job 条目(进程已死,句柄可关)。
- `OsBackend::Drop`:不关闭任何仍有关联进程的 Job 句柄(未设 KILL_ON_JOB_CLOSE,关闭无害,但保守起见仅关闭已确认进程退出的)。

诚实局限(文档写明):
- 已在 Job 内的目标(浏览器/部分游戏)attach 失败(`ERROR_ACCESS_DENIED`)→ 降级:仅降优先级 + `log::warn`。
- 无法从 Job 移除进程;清限流后进程仍关联无上限的 Job(无害)。

测试:
- 集成:`dummy_proc` 关联 Job → 验证 `QueryInformationJobObject` 回读 CpuRate/ControlFlags;清限流后回读 unlimited;Drop 后进程存活。
- 引擎级:游戏进出,验证 QoS 应用 + 清除(扩展 `policy_restore.rs`)。

### 3.2 网络 QoS 调整(opt-in)

目标:游戏启动时减少网络延迟来源,退出还原。

机制(OpenGameBoost 参考,MIT,clean-room):
- Nagle 禁用:注册表 `HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{GUID}` 下 `TcpAckFrequency=1`、`TCPNoDelay=1`(需枚举活动网卡 GUID)。
- NetBIOS 关闭(可选):`HKLM\SYSTEM\CurrentControlSet\Services\NetBT\Parameters` `DisableNetbiosOverTcpip` 或 NETBT 服务停止(风险高,默认关)。
- 应用/还原都写回原值(先读后写,备份原值,还原时写回)。

安全:
- **opt-in**:配置 `[network]` 段,默认 `false`。
- 只动受控键,还原失败 `log::warn`。
- 网卡枚举失败 → 跳过 + 日志,不崩溃。

测试:
- 单元:注册表备份/还原逻辑(用 mock 或临时注册表键)。
- 集成:应用后读回键值,还原后读回原值。破坏性测试需注意环境。

### 3.3 Standby 内存清理(opt-in)

机制(StandbyCleanerLite 参考,无 license 仅思路):
- `NtSetSystemInformation(SystemMemoryListInformation = 0x50, MemoryPurgeStandbyList = 4)`。
- 需 `SeProfileSingleProcessPrivilege`。

触发:游戏进入 GameBoost 时清一次;不退还原(无害,OS 自动重建)。

opt-in:配置 `[game] purge_standby_on_boost = false` 默认。

测试:提权环境冒烟(非提权跳过,记日志)。此功能收益有争议(Win11 内存管理已好),文档写明。

### 3.4 live IPC state(依赖 A 轨道)

`GetState` 从空快照改为真实状态(见 v1.1 计划,与 v1.1 A 轨道共享)。实现方式:`Service` 维护 `Arc<RwLock<StateSnapshot>>`,主循环每次状态变更后更新;IPC 线程读锁返回。

## 4. B. Win32 配置 UI(`aetheris-ui`,新 crate)

定位:非驻留,启动即连 IPC,用完即退。

技术:程序化 Win32 对话框(`CreateWindowExW` + 标准控件),**无 .rc 资源编译依赖**。单线程消息泵。

窗口布局:
- 状态面板:`GetState` → 模式、受管进程列表、最近日志(`dump`)。
- 规则编辑器:三个列表(`[game]`/`[[background]]`/`[[rule]]`),增/删/改条目(名称、动作字段)。
- 操作按钮:保存(写 `aetheris.toml` + 通知服务 reload)、刷新、退出。

新 IPC:
- `Request::GetConfig` → `Response::Config(Config)`(服务回读当前配置)。
- `Request::SaveConfig(Config)` → 服务写文件 + `set_config` reload,返回成功/错误详情。

依赖:仅 `windows` crate(UI feature)+ 既有 `aetheris-core::ipc`。无 GUI 框架。

## 5. C. 外部遥测 overlay(`aetheris-overlay`,新 crate)

定位:独立进程,热键唤起,隐藏即释放。

技术(dcomp-overlay 参考,MIT,clean-room):
- DirectComposition:`CreateDesktopWindow` 或隐藏顶层窗口 + `IDCompositionVisual` + D3D11 swapchain,`SetContent` 到 DWM 合成树。点穿(hit-test 透明)。
- 渲染:每帧重绘文本(模式/受管进程/系统负载/动作状态)。文本用 DWrite(IDWriteTextFormat/IDWriteTextLayout,经 windows crate `Win32_Graphics_DirectWrite` feature),无需位图字体。

数据流:
- 1Hz IPC `GetState` 轮询(overlay 独立进程,不碰服务热路径)。可选:订阅实时推送(v2.x)。

唤起:
- 服务收到热键(`RegisterHotKey`)→ `CreateProcess` 拉起 overlay,传 pipe 名。
- overlay 隐藏/退出:`DestroyWindow` + 释放 swapchain/device + 退出进程。

预算:内存 <2MB,空闲隐藏零 CPU。

不做:游戏内 FPS(hook 才拿得到,反作弊风险,已确认不做)。

## 6. Workspace 布局(v2 增)

```
crates/
├─ aetheris-core/        # 既有:加 Job QoS/网络/standby/新 IPC/live state
├─ aetheris-service/     # 既有:热键、SaveConfig 处理
├─ aetheris-cli/         # 既有:query 实现真查询
├─ aetheris-ui/          # 新:Win32 配置对话框
└─ aetheris-overlay/     # 新:DirectComposition 遥测 overlay
```

## 7. 测试策略

- 引擎特性:单测(注册表备份/还原、Job 表管理、standby 权限)+ 集成(`dummy_proc` Job 回读、QoS 应用/清除、网络键值往返)。
- UI/overlay:手动验收(对话框交互、overlay 显示/隐藏);IPC 消息往返走 `aetheris-core` 集成测试。
- 全量 `cargo test --workspace` + `cargo-deny check` 门禁。

## 8. 验收标准(v2)

1. `qos_cpu_quota` 对不在 Job 内的目标真实限 CPU(回读验证);游戏退出/服务停止后限流清除。
2. 已在 Job 内目标降级路径生效,不崩溃,日志明确。
3. 网络调整应用/还原往返正确;opt-in 默认关。
4. standby 清理提权环境生效,非提权安全跳过。
5. `aetheris-ui` 可查看状态 + 编辑规则 + 保存生效;退出后无残留进程。
6. `aetheris-overlay` 热键唤起显示遥测,隐藏释放资源,内存 <2MB。
7. `get-state`/`query` 返回真实数据(依赖 A 轨道)。
8. 全部测试绿,cargo-deny 过。

## 9. 依赖与 License

新增依赖尽量为零(网络/standby/UI/overlay 都用 `windows` crate)。如引入:
- 字体渲染若用 `dwrite` → 属 windows crate feature,无新依赖。
- 参考源:dcomp-overlay(MIT)、OpenGameBoost(MIT)、StandbyCleanerLite(无 license,仅思路)。均 clean-room,不复制代码。

## 10. 与 v1.1 A 轨道的关系

v2 的 3.4(live state)与 v1.1 A 轨道(验收测量 + 硬化)交集。执行顺序:
1. A 轨道(v1.1):验收测量 + live state + 负载采样 + group-aware 亲和性 + 匹配器缓存 + pipe DACL。
2. v2-A(引擎特性)。
3. v2-B(UI)。
4. v2-C(overlay)。

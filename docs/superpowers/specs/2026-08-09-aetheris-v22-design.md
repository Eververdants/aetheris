# aetheris v2.2 设计文档

日期:2026-08-09
状态:待评审
前置:v1.0.0 / v1.1.0 / v2.0.0 / v2.1.0 已发布。v2.2 目标:把 aetheris 从"4 个二进制 + 手写配置"变成"一个应用程序,开箱即用"。

## 1. 目标

- **单应用程序**:四个二进制合并为一个 `aetheris.exe`,子命令分发。发行 = 一个文件。
- **易用**:双击即用、托盘常驻、自动提权、首次运行自动生成配置、启动即开界面。
- 保持零开销核心不变(服务仍无头、事件驱动、热路径零分配)。

## 2. 范围

### v2.2 内
- A. 单 exe 重构(子命令 `service|ui|overlay|cli`,无参 = ui)
- B. 托盘 UI(最小化到托盘、托盘菜单、服务状态)
- C. 自动提权 + 首次自动配置 + 启动即开界面

### v2.2 外(v2.3+)
- 内核驱动(WHQL,monitor-only)
- 内置预设名单、首次引导向导(用户明确不做)
- overlay 私有内存实测、多游戏重入、Reconcile 部分 revert marker 保留等既有 v2.2 硬化项 —— **并入本版收尾**(低成本)

## 3. 全局约束(v2.2 继承)

- 无 tokio / 无异步运行时。UI/service 均单线程消息泵。
- 服务热路径零分配不变;`aetheris-core` lib 几乎不动(仅必要的入口重构)。
- 受保护进程名单绝对;挂起/瘦身/QoS opt-in。
- 安全:SaveConfig 仍仅提权客户端;DACL 保持 GRGW+SYSTEM。
- 依赖锁定 + cargo-deny;仅 MIT/Apache。
- 每个任务绿 + 提交。

## 4. A. 单 exe 重构

### 4.1 结构

```
crates/
├─ aetheris-core/      # lib(引擎,不动,仅入口相关小改)
└─ aetheris-bin/       # 新 bin:单一 aetheris.exe
   └─ src/
      ├─ main.rs       # 子命令分发
      ├─ mode_service.rs   # 原 aetheris-service::main
      ├─ mode_ui.rs        # 原 aetheris-ui::main(含托盘,见 B)
      ├─ mode_overlay.rs   # 原 aetheris-overlay::main
      └─ mode_cli.rs       # 原 aetheris-cli::main
```

- `main.rs`:`aetheris [service|ui|overlay|cli ...]`,无参 → `ui`。
- 四个旧 bin crate 删除;其 `main` 逻辑平移为 mode 函数(同名入口,参数解析保持一致)。
- `aetheris-core` 中服务/UI 共用的 IPC、配置、动作等不重复;UI/overlay 依赖 core(现状已如此)。
- 子命令错误 → usage + exit 2。
- **重构纪律**:纯平移 + 组装,不改 core 行为;既有测试必须全绿证明等价。

### 4.2 发行

- `cargo build --release` → 单个 `target/release/aetheris.exe`。
- 子命令进程可自我调用:`aetheris service`(由 UI 经 UAC 拉起)、`aetheris overlay`(由 service 经 CreateProcess 拉起)。路径 = `current_exe()` 自身。

## 5. B. 托盘 UI

### 5.1 行为

- `aetheris ui`:窗口(现有对话框)+ 系统托盘图标(`Shell_NotifyIconW`)。
- 关窗口 → 最小化到托盘(不退出进程);托盘图标 → 左键弹菜单或开窗口,右键菜单。
- 托盘菜单项:
  - **启动服务** / **停止服务**:经 UAC(`ShellExecuteW runas`)拉起/终止 `aetheris service`;终止 = 先发 `Stop`(pipe)→ 优雅清场(exit_game_mode + 清 QoS),超时再 `TerminateProcess`(兜底)。
  - **切换 overlay**:发送 `ToggleOverlay` 语义(同 v2.1 热键路径)。
  - **打开界面**:ShowWindow。
  - **退出**:仅退出 UI 进程;服务若运行则保持(托盘可随时重启)。退出前停托盘图标。
- 服务状态图标化:运行中(绿)/停止(灰),按 GetState 可达性 + 心跳更新。

### 5.2 进程关系

- `aetheris ui`(提权,托盘常驻)+ `aetheris service`(提权,无头)双进程,pipe 通信。UI 崩溃不影响服务;服务崩溃 UI 提示可重启。
- UI 启动时探测 GetState;不可达 → 提示一键提权启动服务。

## 6. C. 自动提权 + 首次自动配置

### 6.1 首次运行

- 无 `aetheris.toml` → 自动写入默认配置(带注释示例规则,见仓库 `aetheris.toml` 现有内容)。默认路径定为 `%PROGRAMDATA%\aetheris\aetheris.toml`(管理员可写、位置无关、服务/UI 一致);显式 `--config <path>` 沿用现 CWD 相对默认。首次运行由服务进程(已提权)生成。
- UI 读配置经 `GetConfig`;保存经 `SaveConfig`(提权客户端)。

### 6.2 自动提权

- `aetheris`(双击,无参):先探测服务是否运行;未运行 → `ShellExecuteW runas "aetheris.exe" service` 提权拉起(不阻塞 UI),同时打开 UI 窗口。
- UI 自身以普通权限打开即可查看;**保存/启动服务动作**单独触发 UAC。更简单方案:UI 启动时若服务未运行,弹一次 UAC 提权拉起服务(用户确认一次)。采用:UI 保持普通权限,服务经 UAC 拉起;Save 经服务侧提权检查(已实现),UI 保存失败时提示以管理员运行 UI。

> **最终提权模型(决策)**:UI 普通权限常驻(托盘),服务经 UAC 提权。Save 需要提权 → 首次保存时提示用户以管理员运行 UI,或在 UI 内触发一次 UAC 重启自身(restart-as-admin)。选:**UI 检测到非提权且要 Save 时,弹 UAC 提权重启自身为管理员**(单进程重启,体验最顺)。启动服务始终经 UAC 拉起子进程。

### 6.3 启动即开界面

- `aetheris`(无参/双击)= `aetheris ui`,直接弹窗口 + 托盘。

## 7. 测试策略

- **core**:重构后既有 85+ 测试全绿(等价性证明)。
- **bin**:子命令分发单元测试(参数解析、默认 ui、usage exit 2);mode 模块编译 + 冒烟(与现行为一致)。
- **托盘**:手工验证(图标出现、最小化到托盘、菜单项动作、退出不杀服务)。
- **提权/首配**:手工(首次运行生成配置、UAC 拉起服务、提权重启自身)。
- 全量 `cargo test --workspace` + `cargo-deny` 门禁。

## 8. 验收标准(v2.2)

1. 发行一个 `aetheris.exe`;`aetheris` / `aetheris service|ui|overlay|cli` 行为与旧四 exe 等价。
2. 双击 `aetheris` → 界面打开 + 托盘图标出现 + 服务自动提权运行(首次弹一次 UAC)。
3. 无 `aetheris.toml` → 自动生成默认配置。
4. 托盘:最小化到托盘、菜单启动/停止服务、切换 overlay、退出仅退 UI。
5. 服务运行中 UI 崩溃不影响优化;服务崩溃 UI 提示可重启。
6. 全部测试绿,cargo-deny 过。
7. 既有硬化项(见 §2 v2.2 外最后一行)随本版收尾。

## 9. 收尾硬化项(并入 v2.2)

- Reconcile 部分 revert 保留 marker(1 行)
- 多游戏重入(功能缺口,较小)
- marker 测试避开真实 HKLM(改 scoped)
- ReloadConfig churn 限速注释
- hotkey 解析失败 log
- ToggleOverlay 触发快照刷新排除
- 其余 v2.1 已审 Minor 照 ledger 一并清

## 10. 依赖与 License

- 不新增外部依赖(托盘/提权/单 exe 均 windows crate feature)。新增 feature 需过 cargo-deny。
- THIRD_PARTY 无需变更(clean-room 引用已在)。

## 11. 与既有版本关系

- v2.1.0 为基线。本版是「壳」重构 + UI 增强,引擎行为不变。
- 发布:构建单 exe → 推送 → tag v2.2.0 → release(一个 aetheris.exe + 示例 toml)。

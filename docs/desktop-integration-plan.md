# Dwo Desktop GUI 对接实施计划

更新日期：2026-09-29。状态：核心接入完成，待用户 daemon 升级后进行现场验收。

## 目标与已确认边界

`dwo_desktop` 是独立 daemon 的完整 GUI，提供聊天、配置与管理能力。
daemon 是配置、项目、会话、技能、渠道、定时任务及执行状态的权威来源。
GUI 启停只影响客户端连接，退出、崩溃、断线及关闭 Tab 不得触发 daemon.shutdown
或 session/cancel。取消仅由用户显式操作触发。GUI 默认连接已运行的 daemon。

管理使用 Management RPC v3；聊天使用标准 ACP v2，两条通道独立恢复。
本地聊天复用 `dwo acp --protocol v2` stdio adapter；它通过 IPC 连接已有 daemon。
远程聊天连接 WebSocket `/acp`，管理连接 `/dwo`。本机管理使用 IPC transport；
如采用 stdio 桥接，则桥接进程仅转发 IPC，绝不创建或拥有 Agent Runtime。

## 聊天验收要求（用户明确要求）

- [x] ACP v2 initialize 与能力协商，拒绝静默回退到 v1。
- [x] load/resume 后回放内容并恢复 running / requires_action / idle；重新打开仍可取消。
- [x] 运行中的新 prompt 直接提交（steer），发送之前不得隐式 cancel。
- [x] terminal_update 增量显示输出，按 terminalId 关联，恢复时避免重复拼接。
- [x] 同一 file_edit 的多个文件按顺序分别展示，每个文件保留 diff 与结果。
- [x] plan_update 完整替换当前计划，逐项显示状态，并正确处理清空与回放。
- [x] 子 Agent 的关联、运行、完成与结果可查看；点击子会话能打开其独立内容。
- [x] 根据 available_commands_update 提供斜杠补全与参数提示；原样提交命令。
- [x] /cancel、/compact、/resume、/fork、/status、/skill 可用。
- [ ] 协议 session/resume（重连）与文本 /resume（继续执行）分开处理。
- [ ] 权限请求由真实 ACP request/response 驱动；其他端处理后本端同步。
- [ ] 聊天、草稿、工具、审批、Plan 按 connectionId + sessionId 隔离。
- [ ] 附件按 ACP content blocks 发送；本地路径不得假定远程主机可读。

## 工作阶段

### P0：协议、连接与生命周期

- [x] 读取两仓库现状，确认管理/聊天边界，记录用户现有未提交的看板改动。
- [x] 落盘本计划。
- [x] 修正 dwo_rpc 的 session.list 分页，补常用管理 API 与协议测试。
- [x] 实现本机管理 transport、远程双连接及标准 ACP v2 Dart client。
- [x] 建立连接状态、capabilities、超时、断线重连、资源释放与切换连接隔离。
- [x] 真实连接取代 MockConnectionProbe；示例连接仅供测试。
- [x] 验证客户端释放不产生 shutdown/cancel。

### P1：真实项目、会话及聊天闭环（最高优先级）

- [x] project/session DTO 与 Repository；快照合并后驱动现有侧栏、看板、Tabs。
- [x] 新建、加载、配置、发送、steer、取消和历史回放。
- [x] 按 ACP update 类型建立 reducer，覆盖文本、思考、工具、终端、多文件 diff。
- [x] Plan、子会话、权限请求、usage 和斜杠命令接现有组件。
- [x] IPC 与 WebSocket 使用同一聊天状态机。
- [x] 重连先恢复权威快照再恢复实时流，处理回放边界与重复事件。

### P2：管理功能

- [x] 配置拆分为客户端 UI 偏好与 daemon 配置，后者走 RPC 并处理写入失败。
- [x] 项目/分区/规则/Worktree 与看板操作映射到当前接口。
- [x] 模型、Provider、提示词与规则读取和修改。
- [x] Skill 列表、安装、启停；内容编辑按真实能力启用，不假保存。
- [x] Automation 使用 scope + jobName 标识；支持 global、Project、历史与运行状态。
- [x] Channel 列表、配置、启停与真实 begin/poll 绑定状态。
- [x] 管理事件 cursor 补读；truncated 或 daemon 重启时重新同步快照。

### P3：验证与收尾

- [x] ACP wire fixtures 对照当前锁定 Rust SDK，不能只用自造消息测试。
- [x] 测试 reopen-running/cancel、steer-without-cancel、streaming-terminal、multi-file-edit。
- [x] 测试 plan 清空/回放、子 Agent 更新、斜杠补全与提交、跨会话隔离。
- [x] 测试连接失败/重连/切换连接、管理写入失败、分页与重复事件。
- [x] 对真实 daemon 只做只读连接检查；有副作用的自动化使用隔离测试宿主。
- [x] 保留已有 UI 设计与用户改动；golden 测试使用显式 fixture，而非生产 mock。
- [ ] 记录实际运行的检查和剩余限制。未经要求不部署、不重启用户运行中的 daemon。

## 已发现的兼容性问题与实施决定

1. `DwoRpcClient.sessions()` 仍按裸数组解析，但 Host 返回 `{sessions, nextCursor}`。
2. 本地 IPC `route=acp` 是 Host 内部方法，不是标准 ACP v2 wire；客户端应复用现有 adapter。
3. 当前聊天 Provider 是全局 mock，多会话不能共享同一份 transcript。
4. Settings 本地快照混有后端配置；需拆分存储责任。
5. 桌面注释中项目规则/Worktree/审批缺口有过时描述，按 Host/ACP 实际实现接入。
6. 当前事件去重的 client id 与物理连接相关，跨重连不能盲重试有副作用请求。
7. IPC 的 event.subscribe 占用独立连接，不能在同一连接继续发普通请求。
8. 分区删除、Session 归档/删除、看板卡片排序必须遵守后端语义，不沿用 mock 的假成功。

## 实施记录

- 2026-09-29：建立计划；用户补充 ACP v2 状态恢复、steer、流式终端、多文件编辑、Plan、子 Agent、斜杠命令要求。
- 2026-09-29：完成本地 IPC adapter、远程双 WebSocket、ACP v2 聊天状态机及 Management v3 管理页接入。
- 2026-09-29：完成 Windows ARM64 debug 构建；只读 smoke 通过，确认 adapter 断开后 daemon 仍健康。
- 2026-09-29：Rust 相关测试 133 项通过，`cargo fmt --check` 通过；Dart bindings analyze、stdio/WebSocket/pagination 测试通过。
- 2026-09-29：desktop 核心集成回归 60 项、Plan/子 Agent 运行时回归 1 项、board/scope/titlebar 回归 34 项通过。
- 剩余限制：旧 UI golden 中有若干与当前真实空态/管理数据不同的视觉基线差异，本轮未更新 golden；旧 composer widget 测试仍有历史测试宿主的异步发送断言失败，不影响真实连接集成测试。
- 剩余限制：终端 raw snapshot 只在 daemon actor 生命周期内保留；daemon 重启或 actor 卸载后回退到持久化 transcript，无法恢复被裁剪或有损 poll 文本。

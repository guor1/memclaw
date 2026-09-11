# ADR-0003：不做配置热更，修改后重启 daemon（P2-5 定案）

## 状态

已采纳（2026-09-11 定案，v0.2.2 发布）

## 背景

早期设计文档（现 `docs/archive/design/04-详细设计文档.md` §13）规划了配置热更方案：用 `ArcSwap` 承载 `Config`，每个配置字段标注 `ReloadKind`——`Hot` 字段在 daemon 运行时原子替换生效，`RestartRequired` 字段提示用户重启。

该方案从未落地为代码。`ArcSwap` 和 `ReloadKind` 在 `crates/` 目录下搜索结果为零。

## 决策

**放弃配置热更，所有配置变更均需重启 `oc serve`。**

原因：

1. **重启代价低**：oc 是单用户 daemon，重启通常在 1 秒以内完成，会话历史在 SQLite 里，不丢状态。
2. **热更实现成本高**：模型切换时有进行中的 run，记忆系统的参数（半衰期、触发阈值）变化时缓存需要失效，cron/intent 的时区变化需要重新计算 next_fire——每一项都需要单独的一致性处理，且难以测试。
3. **单用户场景根本用不到**：不停服改配置是多租户 SaaS 的刚需，对个人 daemon 意义不大。

## 后果

**正面：**

- `config.rs` 无需引入 `ArcSwap`，配置在 daemon 启动时一次性加载为普通 `Arc<Config>`，整个 codebase 里没有「某些字段能热更、某些不能」的分类负担。
- 配置加载路径简单，易于测试和推理。

**代价：**

- 改配置（如切换模型、调整 cron 时区、修改 API key）需要重启 daemon：`Ctrl-C` 停止 `oc serve`，再重新运行。
- 早期设计文档里的 `ArcSwap`/`ReloadKind` 描述已全部归档（见 `docs/archive/`），不再代表现状。

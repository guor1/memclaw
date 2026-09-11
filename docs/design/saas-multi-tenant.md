# SaaS 多租户方案

> 状态：草稿，未排期，不进 board

---

## 问题

当前 oh-my-claw 是单租户设计。数据库所有表（`session`、`entry`、`memory`、`cron`、`standing_intent`、`audit`、`kv`）均无用户隔离字段；`ServerState` 持有全局唯一的 `Store`、事件总线和 `SessionRegistry`。多用户部署下记忆、会话、待办全部混杂。

---

## 核心决策：一用户一 DB 文件

对 SQLite 来说，**one-file-per-tenant** 是最简洁的隔离方式：

- 数据物理隔离，不可能因为漏写 `WHERE user_id = ?` 泄露数据；
- `oc-store` 的 `Store::open(path)` 已按路径打开，不需要改 store 内部逻辑；
- 租户 GC 时直接关闭整个 `Store`，干净。

不选择行级 `user_id` 过滤的原因：需要在所有查询路径加过滤条件，漏掉一处即数据泄露，审计成本高，且 FTS5 虚表无法直接附带 `user_id` 列。

---

## 目录结构

```
data_root/
  users/
    {tenant_id}/
      data.db          ← 该租户的 SQLite 数据库
      soul/
        SOUL.md
        USER.md
        MEMORY.md
        skills/
          *.md
```

`tenant_id` 建议用 UUID v7（有序、URL 安全）。

---

## 需要改动的四个层次

### 1. `oc-http`：认证中间件

在每个请求入口解析身份，得到 `TenantId` 后挂进请求上下文。

认证方式（选一，后续决策）：

- **Bearer token**：`Authorization: Bearer <token>`，适合 API 客户端；
- **API key header**：`X-OC-Key: <key>`，适合自托管场景。

token → TenantId 的映射表最简单存 `GlobalState` 里一个静态 `DashMap`，或独立一个 SQLite 管理库（`data_root/auth.db`），视规模决定。

未通过认证的请求直接返回 `401`，不进入协议层。

### 2. `oc-server`：`ServerState` 拆成两层

把当前的单态 `ServerState` 拆为：

```
GlobalState
  └── tenant_pool: DashMap<TenantId, Arc<TenantState>>

TenantState                       ← 内容与当前 ServerState 基本一致
  ├── store: oc_store::Store
  ├── registry: SessionRegistry
  ├── event_tx: broadcast::Sender<Event>
  ├── idem: DashMap<IdemKey, IdemEntry>
  ├── approvals: ApprovalRegistry
  ├── inputs: InputRegistry
  ├── ledger: TaskLedger
  ├── usage: DashMap<SessionId, u32>
  ├── runtime: RuntimeInfo
  ├── diag: DiagRegistry
  └── intent_defaults: IntentDefaults
```

`GlobalState` 负责：

- 懒创建（首次请求时 `entry().or_insert_with(|| TenantState::new(tenant_id, ...))`）；
- GC：长时间无请求的 `TenantState` 整个移除（actor 退出、store 关闭）；
- auth token 校验（或委托给 `oc-auth` crate）。

每个 HTTP 请求拿到 `TenantId` 后，从 `GlobalState` 取出（或创建）对应 `Arc<TenantState>`，后续逻辑完全沿用现有路径。

### 3. `oc-store`：租户路径解析

不需要改 `oc-store` 内部。在 `GlobalState` 里加一个路径计算函数：

```rust
fn tenant_db_path(data_root: &Path, tenant_id: &TenantId) -> PathBuf {
    data_root.join("users").join(tenant_id.to_string()).join("data.db")
}

fn tenant_soul_dir(data_root: &Path, tenant_id: &TenantId) -> PathBuf {
    data_root.join("users").join(tenant_id.to_string()).join("soul")
}
```

首次创建 `TenantState` 时确保目录存在，然后 `Store::open(db_path)`。

### 4. 配置与 SOUL 按租户隔离

当前 `SessionConfig` 的 `soul`、`skills`、`soul_dir` 等字段是启动时从本机 `~/.oc/` 读进来、通过 CLI 传入的。SaaS 下改为从 `tenant_soul_dir` 加载，逻辑与现在一样，路径不同。

`settings.yaml` 里的全局配置（模型、API key 等）仍然是平台级配置，不随租户变化（除非实现 per-tenant 配置覆盖，见开放问题）。

---

## 开放问题（排期前需要决定）

**LLM API key 策略**

两种模式互斥：

- **平台统一 key + 计量**：`GlobalState` 持有一个 `Provider`，所有租户共用，`audit` 表记用量。适合 SaaS 计费场景。
- **租户自带 key**：`TenantState` 各存自己的 `Provider` 配置，部署时每个用户填自己的 key。适合私有化部署或 API key 由用户自己付费的场景。

**用户注册/管理接口**

需要一个管理端来维护 token → TenantId 映射和配额。最简方案是一个管理 CLI 命令（`oc admin user add`）写 `auth.db`；完整方案是加一个管理 HTTP API（`POST /admin/users`）。这个不属于核心协议，单独做。

---

## 实施顺序（供后续排期参考）

按每步可独立测试来切：

1. **`TenantState` 壳**：把现有 `ServerState` 原样搬进 `TenantState`，`GlobalState` 持有一个硬编码 `default` 租户——功能不变，结构到位。
2. **认证中间件**：`oc-http` 加 token 解析，hardcode 一个测试 token，验证路由可以拿到 `TenantId`。
3. **租户 pool + 路由**：`GlobalState` 接入 `tenant_pool`，按 `TenantId` 懒创建 `TenantState`，每个租户拿自己的 `store` 和 `registry`。
4. **soul_dir 按租户隔离**：`SessionConfig` 的 `soul_dir` 从 `tenant_soul_dir` 计算。
5. **租户 GC**：心跳 tick 扫 `tenant_pool`，移除长期无活动的 `TenantState`。
6. **用户管理 CLI/API**：按上面开放问题决定的方案实现。

# ROAD-1 Skills 完整形态 — 设计

> 状态：待评审。对应 BOARD.md `ROAD-1`。日期 2026-09-11。

## Context

当前 `~/.oc/skills/*.md` 是最简版：文件名去扩展名当技能名、**全文整段灌进 system prompt**
（`oc-cli/src/skills_loader.rs` + `oc-core/src/prompt.rs:163`）。没有 frontmatter、没有按需注入、
没有资格门控。技能一多就把上下文预算吃光，且无法承载 ClawHub 装回来的标准技能包。

ROAD-1 要把它升级成「SKILL.md 标准格式 + `<available_skills>` 按需注入 + 资格门控」三件【必须】项。
**硬约束（用户要求）**：ClawHub 上的技能要能直接安进来并生效——即加载器必须兼容
`docs/design/skill-format.md` 定义的目录式 `SKILL.md` + YAML frontmatter 格式。

## 技能格式（对齐 `docs/design/skill-format.md`）

- 一个技能 = 一个目录 `~/.oc/skills/<name>/`，含 `SKILL.md`（接受 `skill.md`；`skills.md` 暂不支持）。
- `SKILL.md` 顶部是 YAML frontmatter，包在 `---` 之间；`description` 是技能摘要（进可用列表）。
- 目录名须满足 `1..=64` 小写字母/数字/连字符；ClawHub 的 routable slug 与目录名一致。

## 数据模型（`oc-core/src/prompt.rs`）

`SkillBrief` 更名为 `Skill`，字段：

```rust
pub struct Skill {
    pub name: String,        // frontmatter.name 或目录名
    pub description: String, // frontmatter.description，进 <available_skills> 列表
    pub body: String,        // 剥离 frontmatter 后的正文
    pub version: String,     // 正文哈希（16 位 hex，FNV-1a），变了触发模型重读
    pub enabled: bool,       // frontmatter.enabled，缺省 true
}
```

> `version` 复用 `oc-server/src/dreaming.rs:288` 的 `content_hash`（FNV-1a）。该函数注释明确
> 「仅用于变没变的比对，非安全用途」——与按需注入「正文变了要重读」的语义完全一致。
> 不新引入 sha2 依赖。抽到 `oc-core`（或作为工具函数）供两处复用。

## Frontmatter 解析（手写极简解析器）

首版只认三键，其余忽略（不报错）：

- `name`（字符串，缺省用目录名）
- `description`（字符串，缺省空）
- `enabled`（布尔，缺省 true）

解析器位置：`oc-core/src/skill.rs`（新模块）。逻辑：

1. 若文件不以 `---` 开头 → 视为无 frontmatter，**整篇都是正文**（但目录名仍是技能名）。
2. 找到第二个 `---`（闭界），中间按行拆 `key: value`；用**手写扫描**而非 serde_yaml。
3. 嵌套块（如 `metadata:` 后面的缩进行）首版**跳过**，不解析、不报错——ClawHub 包的
   `metadata.openclaw.*` 字段本阶段读不到也没关系（见门控范围）。

> 不引入 `serde_yaml` 的理由：首版只认三个标量字段；`metadata` 嵌套结构在 ROAD-6/后续
> （env/bin 门控）需要时再升级解析器。YAGNI。

## 加载器（`oc-cli/src/skills_loader.rs`）

- 遍历 `~/.oc/skills/*/SKILL.md`（目录式），跳过非目录条目。
- 每目录读 `SKILL.md`（`SKILL.md` → `skill.md` 回退），解析 frontmatter → `Skill`。
- `version = content_hash(body)`。
- frontmatter 缺失/读失败 → 跳过该目录（不 panic，不阻塞启动），打 `tracing::warn`。
- 旧平铺 `~/.oc/skills/*.md` **直接忽略**（用户定案：只认新格式，不做降级兼容）。

签名从 `load()` 改为 `load(skills_cfg)`，接门控（见下）。

## 资格门控（config + os）

1. **`enabled`**：frontmatter 里 `enabled: false` → 过滤。
2. **config.toml 新增 `[skills]` 节**（`oc-core/src/config.rs`）：

   ```toml
   [skills]
   allowlist = []      # 非空则只加载列表内名字
   denylist  = []      # 永不加载（优先于 allowlist）
   ```

   字段 `#[serde(default)]`，老配置无此节也能解析（保持「缺键兼容」既有约定）。
3. **os 门控**：解析 `metadata.openclaw.os`（字符串数组，如 `["darwin"]`），不匹配当前
   OS 则跳过。为解析这一个嵌套字段，极简解析器需**最少支持** `metadata.openclaw.os` 这条
   缩进路径（见下方「解析器最小嵌套支持」）。

纯函数 `gated_skills(all, cfg)` 在 `oc-core/src/skill.rs`，输入全部已加载技能 + `SkillsConfig`，
输出过滤后的列表。OS 匹配在 CLI 侧注入当前平台字符串（复用 `oc-tools` 里已有的平台判断或
`std::env::consts::OS`），保持 `oc-core` 纯策略无 IO。

**范围外**（本阶段不做，留后续）：`requires.env` / `requires.bins` / `requires.anyBins` /
`envVars` 运行时门控。这些需要探测进程环境与二进制存在性，且是「用的时候才报错」的体验问题，
不阻塞「装进来能生效」这条硬约束。

## 解析器最小嵌套支持

手写解析器只需读懂两种形状：

```
name: foo
description: bar
enabled: false
metadata:
  openclaw:
    os:
      - darwin
```

实现：逐行扫描；遇 `key: <标量>` 记录；遇 `metadata:` / `openclaw:` 进上下文；遇
`    os:` 后的 `- value` 列表项收集进 `os: Vec<String>`。其余嵌套键一律跳过。这是为 os 门控
专门加的最小支持，不实现通用 YAML。

## 按需注入（`oc-core/src/prompt.rs` 改）

「技能」段从全文改为索引列表：

```text
# 技能
可用技能列表（正文不在本提示词内）：
- <name> — <description> [version <hash>]
...
要使用某个技能时，用 file 工具 read `~/.oc/skills/<name>/SKILL.md` 读正文；
若该技能的 version 与上次读到的不一致，需要重新读取全文。
```

要点：

- 列表按 `name` 字典序排序（延续现有确定性约定，见 `prompt.rs:164`）。
- **正文不再进 system prompt**——`render_system_prompt` 里删掉 `body` 注入。
- 现有 `prompt_wiring.rs` 测试 `skills_reach_model_request` 断言「正文注入系统提示词」，
  需同步改为断言「技能名/描述进提示词、正文不进」。

## 文件工具读技能（依赖已有能力，无需新工具）

模型按需读正文走 `file` 工具的 `read` op。`~/.oc/skills/` 落在 `allowed_roots`（`OC_HOME`）
内（见 `provider_setup.rs:128-137`），故无需改 path_guard。仅需在 `file` 工具的
`description` 里点一句「技能正文在 `~/.oc/skills/<name>/SKILL.md`，用 read 读」，
帮模型发现这个路径。

## onboard 迁移

`oc-cli/src/onboard.rs`：

- 不再写 `skills/example.md`。
- 改写 `skills/example/SKILL.md`（含 frontmatter：`name: example`、`description`、`enabled: true`）。

## 测试

- **oc-core**（`skill.rs` 新模块单测）：
  - frontmatter 解析：三键正常 / 无 frontmatter 整篇为正文 / `enabled` 缺省 true /
    `name` 缺省用目录名。
  - 最小嵌套 `metadata.openclaw.os` 解析。
  - `gated_skills`：enabled 过滤 / allowlist / denylist 优先 / os 匹配矩阵。
  - `render_system_prompt`：只含索引不含正文（锁快照）。
- **oc-cli**（`skills_loader` 单测，用 tempdir）：
  - 目录式扫描；`skill.md` 回退；旧平铺 `*.md` 忽略；坏 frontmatter 的目录跳过不 panic。
- **oc-server**：`prompt_wiring.rs` 改写 `skills_reach_model_request` 断言（描述注入、正文不注入）。

## 待改文件

- `crates/oc-core/src/skill.rs`（新）：`Skill` 类型 + frontmatter 解析 + `gated_skills` + `content_hash`。
- `crates/oc-core/src/prompt.rs`：`SkillBrief`→`Skill`，技能段改索引注入。
- `crates/oc-core/src/config.rs`：`SkillsConfig { allowlist, denylist }` + `Config.skills`（`#[serde(default)]`）。
- `crates/oc-core/src/lib.rs`：注册 `skill` 模块。
- `crates/oc-cli/src/skills_loader.rs`：目录式 + frontmatter + 门控，签名 `load(&SkillsConfig, platform)`。
- `crates/oc-cli/src/provider_setup.rs`：`load()` → `load(&cfg.skills, platform)`。
- `crates/oc-cli/src/onboard.rs`：写 `skills/example/SKILL.md`。
- `crates/oc-tools/src/file.rs`：`description` 补一句技能路径提示。
- `crates/oc-server/src/{session,run}.rs` + `tests/prompt_wiring.rs`：`SkillBrief` → `Skill`。

## 依赖 / 约束

- 不新增第三方依赖（手写 YAML 极简解析 + FNV-1a 复用）。
- `oc-core` 保持「纯策略、无 IO」：frontmatter 解析与门控入参是字符串/配置，不碰文件系统；
  文件 IO 全在 `oc-cli` 的 loader。

## 验证

1. `cargo test -p oc-core -p oc-cli -p oc-server`。
2. 手工：`oc onboard` 后 `~/.oc/skills/example/SKILL.md` 存在；往 `~/.oc/skills/` 放一个
   带 frontmatter 的技能目录，`oc serve` 起后发消息，确认 base prompt 里只有技能索引、
   模型能 `file read` 拿到正文；`enabled:false` / denylist 的技能不出现在索引里。

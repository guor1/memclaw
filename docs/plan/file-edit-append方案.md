# file 工具加 edit / append

## 动机

`file` 目前只有 `write`（整文件覆写）。真机上生成 PPT 的任务里，模型改一处
括号不匹配也只能重发整个脚本，或者干脆另写一个 `fix_ppt.py` 去打补丁
（2026-09-09 workspace 里 `make_ppt.py` 19915 字节、`fix_ppt.py` 1825 字节
并存，就是这个绕法的产物）。

代价用当时量到的吐字速率算得出来：约 **48 字符/秒**。

| 操作 | 参数量 | 流式耗时 |
|---|---|---|
| 覆写 20KB 脚本 | ~20000 字符 | **~7 分钟** |
| edit 改一处 | ~200 字符 | **~4 秒** |

这不是常数级优化。整个任务 390 秒里 86% 是模型吐字，而吐字量直接由参数长度
决定——减少重发量是唯一能实质提速的方向（换模型之外）。

另有一处自相矛盾要一并收口：`run.rs` 的 `TOOL_TRUNCATION_INSTRUCTION` 里写了
「需要写长文件就分多次追加」，但工具根本没有 append，那句提示是空头支票。

## 契约

```jsonc
// 精确替换
{ "op": "edit", "path": "make_ppt.py",
  "old_string": "labels=[('陆逊'", "new_string": "labels=[('陆逊',",
  "replace_all": false }   // 可省，默认 false

// 追加（文件不存在则创建）
{ "op": "append", "path": "notes.md", "content": "\n新增一段\n" }
```

`old_string` / `new_string` 这组命名对齐 openclaw 与 Claude Code 的 Edit 工具
（openclaw 的 `attempt.tool-call-argument-repair.ts` 里 `old_string`→`new_string`
是它已知的键序对）。选它不是因为好看，而是模型在训练中见过大量这种样例，
上手成本最低；自创参数名要靠提示词教，是净负担。

## 失败语义

这是决定 edit 到底省不省时间的部分：**如果失败率高，往返次数反而增加。**

`ToolOutput::err` 而非 `ToolError`——已确认 tools_bridge 会把 `err` 的 content
原样回喂给模型（`ToolError` 会加「工具错误: 」前缀）。文案要能让模型自纠。

| 情形 | 处置 |
|---|---|
| `old_string` 为空 | `BadArgs`。空串匹配任意位置，语义无意义 |
| `old_string == new_string` | `BadArgs`。无操作，通常是模型出错 |
| 未匹配 | `err` + 诊断（见下） |
| 匹配 N>1 且未 `replace_all` | `err`，告知出现 N 次，要求补足上下文或显式 `replace_all` |
| 成功 | 报替换处数与字节变化 |

**唯一性默认必需。** 歧义替换默默改错位置比报错更糟——模型不会知道自己改坏了。

### 未匹配时的诊断阶梯

模型看不到文件的确切字节，最常见的失败是凭记忆重构、空白对不上。所以不能只回
「未找到」，要指出方向：

1. **CRLF 回退**（Windows 上的主要失败源）：文件含 CRLF 而 `old_string` 用 LF
   时，把 `old_string` / `new_string` 的 LF 转成 CRLF 再试。命中则按文件原有
   换行风格写回——不整体重写换行，避免顺带改掉无关行。
2. **空白归一化探测**：把两边的连续空白折叠后比较。若这样能匹配，明确告诉模型
   「内容对得上但空白/缩进不同」，而不是让它瞎猜。
3. **首行定位**：若 `old_string` 的第一行在文件中出现，回报其行号，让模型知道
   该 `read` 哪一段。

这三条都是廉价的字符串操作，但直接决定模型第二次能不能改对。

## 改动清单

| 文件 | 改动 |
|---|---|
| `crates/oc-tools/src/file.rs` | `FileArgs` 加 `Edit` / `Append` 两个变体；`spec()` 的 op 枚举与参数描述同步；实现两个分支 + 诊断阶梯 |
| `crates/oc-core/src/prompt.rs` | 硬编码的 op 列表补 `edit`/`append`；加一句「改已有文件优先 edit，不要整文件覆写」——这是真正让模型改行为的开关 |
| `crates/oc-tools/tests/tools.rs` | 见下 |

`spec()` 里的 `op` 是 JSON Schema 的 `enum`，漏改模型就不会调新 op；`prompt.rs`
里那份 op 清单是给模型看的说明，两处都得动，不能只改一处。

审批策略不变：`file` 现在 `may_need_approval: false`，而 `write` 已经能覆写文件，
`edit` 的破坏力不超过它，保持一致。

## 测试

`crates/oc-tools/tests/tools.rs`（沿用现有 `tempdir` + `ToolCtx::detached` 模式）：

- edit 命中唯一目标 → 文件内容按预期变化，其余部分不动
- `old_string` 出现两次且未 `replace_all` → 失败，且错误文案含出现次数
- 同上加 `replace_all: true` → 两处都替换，回报处数
- 未匹配 → 失败，且文案提示检查空白
- 空白不同但归一化后能匹配 → 错误文案明确指出是空白问题（锁住诊断阶梯第 2 条）
- CRLF 文件 + LF `old_string` → 成功替换，且文件其余行的 CRLF 保持不变
  （锁住第 1 条，Windows 回归）
- append 到已有文件 → 内容接在末尾，原内容不变
- append 到不存在的文件 → 创建
- edit / append 的路径同样受 `allowed_roots` 约束（根外拒绝）

## 不做

**批量 edits（一次多处替换）。** openclaw 的参数修复表里有 `edits` 复数键，说明
它支持。能进一步减少往返，但要先定清部分失败的语义（第 3 处匹配不上时，前 2 处
要不要回滚？），那是独立的一轮设计。本轮先把单处替换做对。

**行号定位。** 模型手里的行号来自几轮前的 `read`，中间只要改过一次就全错位，
而且错了不报错、默默改坏文件。字符串替换天然自校验。

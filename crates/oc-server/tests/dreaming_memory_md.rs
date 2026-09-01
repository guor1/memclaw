//! P1-4：dreaming 巩固模型轮重写 MEMORY.md（设计 §11.4）。
//!
//! 覆盖：正常重写、乐观并发退化 append、无 ctx 时不碰文件、模型空输出不清空文件、
//! 首次巩固（文件不存在）、既有内容参与合并。
//!
//! **写安全是本组测试的重点**：MEMORY.md 是用户可手编的文件，覆盖逻辑出错会丢数据。

use std::path::PathBuf;
use std::sync::Arc;

use oc_core::dreaming::DreamCfg;
use oc_llm::mock::MockProvider;
use oc_server::dreaming::{scan_with, ConsolidateCtx};

/// 在「模型生成时」改写 MEMORY.md 的 provider，用于制造乐观并发冲突。
///
/// 真实场景是用户在 dreaming 跑模型的这段时间里手编了 MEMORY.md。mock 没有
/// 副作用钩子，故在此本地实现 Provider：stream_chat 被调用时先写文件再回内容。
struct RacingProvider {
    reply: String,
    /// 生成时写入此路径，模拟并发修改。
    race_path: PathBuf,
    race_body: String,
}

#[async_trait::async_trait]
impl oc_llm::Provider for RacingProvider {
    fn id(&self) -> &str {
        "racing-mock"
    }

    async fn stream_chat(
        &self,
        _req: oc_llm::ModelRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> oc_llm::LlmResult<futures_util::stream::BoxStream<'static, oc_llm::LlmResult<oc_llm::Delta>>>
    {
        // 关键：在返回内容之前改文件，让落盘前的 hash 重校验必然不一致。
        std::fs::write(&self.race_path, &self.race_body).unwrap();
        let text = self.reply.clone();
        Ok(Box::pin(futures_util::stream::iter(vec![Ok(oc_llm::Delta::Text(text))])))
    }
}

/// 放宽时间窗的 cfg：内存库 created_at=now，age≈0 会被 TooRecent 拒。
fn loose_cfg() -> DreamCfg {
    DreamCfg { min_age_secs: 0, max_age_secs: 0, ..DreamCfg::default() }
}

/// 预置一条能过双门的 episodic 候选（Agent 来源、高分、use_count≥2）。
async fn seed_candidate(store: &oc_store::Store, id: &str, text: &str) {
    let w = store.writer();
    w.upsert_memory(oc_store::NewMemory {
        id: id.into(),
        tier: oc_store::Tier::Episodic,
        origin: oc_store::Origin::Agent,
        text: text.into(),
        keywords: None,
        importance: 0.8,
        content_hash: format!("h-{id}"),
        pref_key: None,
    })
    .await
    .unwrap();
    // 频次门要 use_count ≥ 2。
    w.touch_memory(id.into(), 0).await.unwrap();
    w.touch_memory(id.into(), 0).await.unwrap();
}

fn ctx(dir: &std::path::Path, reply: &str) -> ConsolidateCtx {
    ConsolidateCtx {
        provider: Arc::new(MockProvider::echo_text(reply)),
        model: "mock".into(),
        soul_dir: dir.to_path_buf(),
    }
}

/// 正常路径：文件未被并发修改 → 模型输出整体覆盖 MEMORY.md。
#[tokio::test]
async fn consolidation_rewrites_memory_md() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("MEMORY.md");
    std::fs::write(&path, "# MEMORY.md\n\n（占位）\n").unwrap();

    let store = oc_store::Store::open_memory().unwrap();
    seed_candidate(&store, "ep-1", "用户常在周五做复盘").await;

    let c = ctx(tmp.path(), "## 工作习惯\n- 周五做复盘");
    let promoted = scan_with(&store, 0, &loose_cfg(), Some(&c)).await;
    assert_eq!(promoted, 1, "候选应被巩固");

    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("周五做复盘"), "模型输出应写入 MEMORY.md: {body}");
    assert!(!body.contains("（占位）"), "整体重写应替换掉旧内容: {body}");
    assert!(body.ends_with('\n'), "应以换行结尾");
    // 不留临时文件。
    assert!(!tmp.path().join("MEMORY.md.tmp").exists(), "不应残留 .tmp");
}

/// 既有内容与新条目都必须进 prompt——否则重写会丢掉历史记忆。
#[tokio::test]
async fn existing_content_and_new_items_reach_the_model() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("MEMORY.md");
    std::fs::write(&path, "## 旧习惯\n- 早上喝咖啡\n").unwrap();

    let store = oc_store::Store::open_memory().unwrap();
    seed_candidate(&store, "ep-1", "用户常在周五做复盘").await;

    // CapturingMock 记录收到的 ModelRequest，可断言 prompt 真的带上了两边内容。
    let provider = Arc::new(oc_llm::mock::CapturingMock::new("## 合并\n- 早上喝咖啡\n- 周五复盘"));
    let captures = provider.captures();
    let c = ConsolidateCtx {
        provider: provider.clone(),
        model: "mock".into(),
        soul_dir: tmp.path().to_path_buf(),
    };
    scan_with(&store, 0, &loose_cfg(), Some(&c)).await;

    let reqs = captures.lock().unwrap();
    assert_eq!(reqs.len(), 1, "应只跑一轮巩固模型");
    let prompt = &reqs[0].messages[0].content;
    assert!(prompt.contains("早上喝咖啡"), "既有内容必须进 prompt: {prompt}");
    assert!(prompt.contains("周五做复盘"), "新巩固条目必须进 prompt: {prompt}");
    // 系统提示词应带防编造约束。
    let sys = reqs[0].system.as_deref().unwrap_or("");
    assert!(sys.contains("不要"), "系统提示应含禁止编造的约束: {sys}");
}

/// 乐观并发：生成期间文件被改 → **不得覆盖**，退化为追加。
#[tokio::test]
async fn concurrent_modification_falls_back_to_append() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("MEMORY.md");
    std::fs::write(&path, "原始内容\n").unwrap();

    let store = oc_store::Store::open_memory().unwrap();
    seed_candidate(&store, "ep-1", "用户常在周五做复盘").await;

    // RacingProvider 在生成时写文件，模拟用户在 dreaming 跑模型期间手编 MEMORY.md。
    let c = ConsolidateCtx {
        provider: Arc::new(RacingProvider {
            reply: "## 新内容\n- 条目".into(),
            race_path: path.clone(),
            race_body: "用户手改的内容\n".into(),
        }),
        model: "mock".into(),
        soul_dir: tmp.path().to_path_buf(),
    };
    scan_with(&store, 0, &loose_cfg(), Some(&c)).await;

    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains("用户手改的内容"),
        "并发修改不得被覆盖（会丢用户数据）: {body}"
    );
    assert!(body.contains("条目"), "新内容应以追加形式保留: {body}");
    assert!(body.contains("检测到并发修改"), "应带追加标记便于人工辨认: {body}");
}

/// 不配 ctx（soul_dir=None 的场景）→ 只做 DB 内巩固，绝不碰文件。
#[tokio::test]
async fn without_ctx_no_file_is_touched() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("MEMORY.md");
    std::fs::write(&path, "不该被动\n").unwrap();

    let store = oc_store::Store::open_memory().unwrap();
    seed_candidate(&store, "ep-1", "用户常在周五做复盘").await;

    let promoted = scan_with(&store, 0, &loose_cfg(), None).await;
    assert_eq!(promoted, 1, "DB 内巩固仍应发生");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "不该被动\n",
        "无 ctx 时文件必须保持原样"
    );
}

/// 模型空输出 → 保持原文件不变（**不能清空**用户的长期记忆）。
#[tokio::test]
async fn empty_model_output_keeps_file_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("MEMORY.md");
    std::fs::write(&path, "重要记忆\n").unwrap();

    let store = oc_store::Store::open_memory().unwrap();
    seed_candidate(&store, "ep-1", "用户常在周五做复盘").await;

    let c = ctx(tmp.path(), "   "); // 只有空白 → 视为无输出
    scan_with(&store, 0, &loose_cfg(), Some(&c)).await;

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "重要记忆\n",
        "模型无输出时绝不能清空文件"
    );
}

/// 首次巩固：MEMORY.md 尚不存在 → 应创建而非报错。
#[tokio::test]
async fn creates_file_when_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("MEMORY.md");
    assert!(!path.exists());

    let store = oc_store::Store::open_memory().unwrap();
    seed_candidate(&store, "ep-1", "用户常在周五做复盘").await;

    let c = ctx(tmp.path(), "## 习惯\n- 周五复盘");
    scan_with(&store, 0, &loose_cfg(), Some(&c)).await;

    assert!(path.exists(), "文件不存在时应创建");
    assert!(std::fs::read_to_string(&path).unwrap().contains("周五复盘"));
}

/// 无候选通过双门 → 不跑模型、不碰文件。
#[tokio::test]
async fn no_candidates_skips_model_round() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("MEMORY.md");
    std::fs::write(&path, "原样\n").unwrap();

    let store = oc_store::Store::open_memory().unwrap();
    // 不可信来源：被门2排除。
    store
        .writer()
        .upsert_memory(oc_store::NewMemory {
            id: "ep-bad".into(),
            tier: oc_store::Tier::Episodic,
            origin: oc_store::Origin::Untrusted,
            text: "网页声称的事实".into(),
            keywords: None,
            importance: 0.9,
            content_hash: "hb".into(),
            pref_key: None,
        })
        .await
        .unwrap();

    let c = ctx(tmp.path(), "不该被写入");
    let promoted = scan_with(&store, 0, &loose_cfg(), Some(&c)).await;
    assert_eq!(promoted, 0, "不可信来源不应巩固");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "原样\n",
        "无巩固时不应触发模型轮写文件"
    );
}

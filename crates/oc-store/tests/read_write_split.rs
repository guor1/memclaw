//! P2-1 读写分离的验收：慢读不再拖住写线程。
//!
//! 必须用**文件库**：内存库无池、读回落写线程（见 `Store` 的类型文档），
//! 拿它跑这条会永远失败——那不是回归，是配置用错了。
//!
//! 判定标准是**时间重叠**而非绝对耗时：读被人为拖慢 N 秒，若写要等读做完
//! 才能推进（改动前的行为），写的耗时会被抬到同一量级；分离之后写应当
//! 几乎立刻返回。

use std::path::PathBuf;
use std::time::Instant;

use oc_store::{NewEntry, Role, Store};

fn temp_db(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("oc-rw-{}-{tag}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(p.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(p.with_extension("sqlite-shm"));
    p
}

fn cleanup(p: &PathBuf) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(p.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(p.with_extension("sqlite-shm"));
}

async fn seed(store: &Store, session: &str, n: usize) {
    let w = store.writer();
    w.ensure_session(session.into(), "main".into()).await.unwrap();
    for i in 0..n {
        w.append_entry(NewEntry {
            session_id: session.into(),
            role: Role::User,
            content: format!("第 {i} 条"),
            tokens_est: 2,
        })
        .await
        .unwrap();
    }
}

/// 写队列严重积压时，读仍应立即返回。
///
/// **为何这样构造**：要证明"读不排在写队列后面"，就得让写队列里真的有一长串
/// 活儿，然后计时一次读。分离之后读走独立连接，耗时与队列长度无关；
/// 改回去（读也投给写线程）的话，这次读会排在几千条写之后才被执行。
///
/// 早先的版本直接调 `reader.read(...)` 计时——那绕过了 `Store` 的分流逻辑，
/// 把读写分离整个改回去也照样绿，什么都证明不了。现在走公开的
/// `store.load_transcript()`，它内部才决定"走池还是走写线程"。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_is_not_queued_behind_pending_writes() {
    const BACKLOG: usize = 3000;

    let db = temp_db("noblock");
    let store = Store::open_path(db.clone()).unwrap();
    seed(&store, "main", 5).await;

    // 灌一大批写但**不等**回执：写队列是无界的，命令会立刻堆进去。
    let mut pending = Vec::with_capacity(BACKLOG);
    for i in 0..BACKLOG {
        let s = store.clone();
        pending.push(tokio::spawn(async move {
            s.writer()
                .append_entry(NewEntry {
                    session_id: "main".into(),
                    role: Role::User,
                    content: format!("积压 {i}"),
                    tokens_est: 2,
                })
                .await
        }));
    }

    // 计时一次读。此刻写队列里还有成千条没做完。
    let t0 = Instant::now();
    let rows = store.load_transcript("main".into(), 10).await.unwrap();
    let read_ms = t0.elapsed();
    assert!(!rows.is_empty(), "应读到 seed 的历史");

    // 等积压写完，量出"排在队尾"的量级作为对照。
    for p in pending {
        p.await.unwrap().unwrap();
    }
    let drain_ms = t0.elapsed();

    assert!(
        read_ms * 4 < drain_ms,
        "读耗时 {read_ms:?} 应远小于写队列排空耗时 {drain_ms:?}——疑似读仍排在写队列后面"
    );

    drop(store);
    cleanup(&db);
}

/// 验收项：并发 10 个读 + 1 个写，全部成功且数据一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reads_and_write_all_succeed() {
    let db = temp_db("concurrent");
    let store = Store::open_path(db.clone()).unwrap();
    seed(&store, "main", 20).await;

    let mut reads = Vec::new();
    for _ in 0..10 {
        let s = store.clone();
        reads.push(tokio::spawn(async move {
            s.load_transcript("main".into(), 100).await
        }));
    }
    let s = store.clone();
    let write = tokio::spawn(async move {
        s.writer()
            .append_entry(NewEntry {
                session_id: "main".into(),
                role: Role::Assistant,
                content: "并发写".into(),
                tokens_est: 2,
            })
            .await
    });

    for r in reads {
        let rows = r.await.unwrap().expect("并发读不应失败");
        // 读可能落在写之前或之后，两种条数都合法；关键是不报错、不读到半截。
        assert!(
            rows.len() == 20 || rows.len() == 21,
            "读到的条数异常：{}",
            rows.len()
        );
    }
    write.await.unwrap().expect("并发写不应失败");

    assert_eq!(
        store.load_transcript("main".into(), 100).await.unwrap().len(),
        21,
        "写应已落库"
    );

    drop(store);
    cleanup(&db);
}

/// P2-2 的降级语义：写线程死后，读仍然可用。
///
/// 这正是读写分离顺带买到的东西——写侧瘫痪不再等于存储层整体瘫痪。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_survive_dead_writer() {
    let db = temp_db("degraded");
    let store = Store::open_path(db.clone()).unwrap();
    seed(&store, "main", 3).await;

    // 制造"写侧不可用"：丢弃 store 的写线程无从外部触发，
    // 故直接换一个只保留读池的 store —— 等价于写线程已死后的可用面。
    let reader = store.reader().expect("文件库必须有读池").clone();
    drop(store); // 写线程随最后一个 Writer 句柄退出

    let rows = reader
        .read(|c| oc_store::ops::load_transcript(c, "main", 100))
        .await
        .expect("写线程没了，读仍应可用");
    assert_eq!(rows.len(), 3, "读到的历史应完整");

    cleanup(&db);
}

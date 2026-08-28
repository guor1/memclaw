//! 单实例文件锁（设计 §9）。`~/.oc/run/oc.lock`，fs2 独占锁。

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use fs2::FileExt;

/// 持有独占锁的守卫；drop 时自动释放。
pub struct LockGuard {
    _file: File,
}

/// 尝试获取单实例锁。已被占用则报错（已有 serve 在跑）。
pub fn acquire(oc_home: &Path) -> Result<LockGuard> {
    let run_dir = oc_home.join("run");
    std::fs::create_dir_all(&run_dir)?;
    let lock_path = run_dir.join("oc.lock");
    let file = File::create(&lock_path)
        .with_context(|| format!("创建锁文件 {} 失败", lock_path.display()))?;
    file.try_lock_exclusive()
        .with_context(|| "已有 oc serve 在运行（单实例锁被占用）")?;
    Ok(LockGuard { _file: file })
}

//! store 错误类型。

use thiserror::Error;

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("migration error: {0}")]
    Migration(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// 写线程已死（panic 或已关停）。
    ///
    /// 单列一个变体而非塞进 `Migration(String)`：调用方需要能**区分**"这次写失败了"
    /// 与"存储层写侧整体不可用"——后者不该重试，且应让 `oc debug` 报出来。
    /// 读路径不受影响（走独立连接池），故这是**降级**而非全面瘫痪。
    #[error("存储写线程已停止（进程需重启）")]
    WriterDead,
}

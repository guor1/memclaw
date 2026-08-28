//! `oc doctor`（设计 §9，M1 验收入口）。
//!
//! - 建库 + 前向迁移，报告 schema 版本
//! - 校验默认配置形状
//! - 可选导出协议 JSON Schema

use std::fs;

use anyhow::{Context, Result};

use crate::paths;

pub fn run(dump_schema: bool) -> Result<()> {
    println!("oc doctor");
    println!("=========");

    // 1) 磁盘布局
    let home = paths::oc_home()?;
    fs::create_dir_all(&home).with_context(|| format!("创建 {} 失败", home.display()))?;
    println!("[ok] oc home: {}", home.display());

    // 2) 建库 + 迁移
    let db = paths::db_path()?;
    let conn = oc_store::open(&db).context("打开/迁移数据库失败")?;
    let version = oc_store::schema_version(&conn)?;
    println!(
        "[ok] 数据库: {} (schema v{}, 目标 v{})",
        db.display(),
        version,
        oc_store::migrate::TARGET_VERSION
    );
    drop(conn);

    // 3) 配置校验（当前用默认配置演示；M2+ 从 config.toml 加载）
    let cfg = oc_core::Config::default_local();
    match cfg.validate_shape() {
        Ok(()) => println!("[ok] 配置校验通过（默认配置）"),
        Err(report) => {
            println!("[err] 配置校验失败:\n{report}");
            anyhow::bail!("配置无效");
        }
    }

    // 4) 协议 schema 导出
    if dump_schema {
        let schema = oc_proto::schema::export_schema();
        let out_dir = std::path::Path::new("schema");
        fs::create_dir_all(out_dir)?;
        let out = out_dir.join("oc-proto.json");
        let pretty = serde_json::to_string_pretty(&schema)?;
        fs::write(&out, pretty).with_context(|| format!("写入 {} 失败", out.display()))?;
        println!("[ok] 协议 schema 导出: {}", out.display());
    }

    println!("\n全部检查通过。");
    Ok(())
}

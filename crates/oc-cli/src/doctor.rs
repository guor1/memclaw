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

    // 2.5) 库内形状校验。
    //
    // 版本号对不代表表结构对：开发阶段的 schema 变更是**直接改建表 DDL**
    // （不写迁移步进，见项目约定），于是旧库的 `user_version` 已经等于目标值、
    // 迁移整个 no-op，但表结构还是老的。没有这一步，`oc doctor` 会对着一个
    // 「每次记忆操作都报 no such table」的库照样打印"全部检查通过"，
    // 真正的报错要等到运行时才冒出来。
    if let Err(missing) = oc_store::check_shape(&conn) {
        println!("[err] 数据库结构与当前版本不符，缺少：{missing}");
        println!();
        println!("      开发阶段不做数据迁移。请停掉 daemon 后删库重建：");
        println!("        rm {}*        # 连 -wal / -shm 一起删", db.display());
        println!("        oc doctor              # 按新 DDL 重建");
        anyhow::bail!("数据库结构过旧");
    }
    drop(conn);

    // 3) 配置校验：存在 config.toml 则解析+校验真实配置，否则校验默认配置。
    let cfg_path = crate::paths::config_path()?;
    let (cfg, source) = if cfg_path.exists() {
        let text = fs::read_to_string(&cfg_path)
            .with_context(|| format!("读取 {} 失败", cfg_path.display()))?;
        let cfg: oc_core::Config = toml::from_str(&text)
            .with_context(|| format!("解析 {} 失败（TOML 格式错误）", cfg_path.display()))?;
        (cfg, format!("{}", cfg_path.display()))
    } else {
        (oc_core::Config::default_local(), "默认配置".to_string())
    };
    match cfg.validate_shape() {
        Ok(()) => println!("[ok] 配置校验通过（{source}）"),
        Err(report) => {
            println!("[err] 配置校验失败:\n{report}");
            anyhow::bail!("配置无效");
        }
    }

    // 3.5) 工作区：agent 的初始目录 + 文件访问范围。单独报出来，让人一眼看到
    // 模型实际能碰哪里（这一项无配置，只随 OC_HOME 走）。
    let ws = paths::workspace()?;
    fs::create_dir_all(&ws).with_context(|| format!("创建工作区 {} 失败", ws.display()))?;
    println!("[ok] 工作区: {}", ws.display());
    println!("     file/sys 允许根 = 此目录 + {}", home.display());

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

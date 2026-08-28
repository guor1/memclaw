//! JSON Schema 导出（设计 §2.4）。
//!
//! 即使只有 CLI，也生成 schema，作为未来渠道适配器的接入契约。
//! `oc doctor --dump-schema` 落盘 `schema/oc-proto.json`。

use crate::frame::Frame;

/// 导出顶层 `Frame` 的 JSON Schema。
///
/// 因为 `Frame` 内联了 `Req`/`Res`/`Event` 及其全部子类型，
/// 单个 schema 即覆盖整个协议契约。
pub fn export_schema() -> serde_json::Value {
    let schema = schemars::schema_for!(Frame);
    serde_json::to_value(schema).expect("schema serializes to json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_exports_and_is_object() {
        let v = export_schema();
        assert!(v.is_object(), "schema root must be an object");
    }
}

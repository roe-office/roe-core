//! 从工具结果里取出 save_document 产物的虚拟路径(供 UI 在保存后刷新/提示)。

use rig_core::tool::ToolResult;

/// 仅当工具名是 `save_document` 且执行成功、输出是含字符串字段
/// `virtual_path` 的 JSON 对象时返回该路径;其余情况一律 `None`。
pub fn extract_saved_path(tool_name: &str, result: &ToolResult) -> Option<String> {
    if tool_name != "save_document" || !result.is_success() {
        return None;
    }
    result
        .output()
        .as_json()?
        .get("virtual_path")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_doc::SaveDocumentOutput;
    use crate::worker::DocKind;
    use rig_core::tool::{ToolExecutionError, ToolOutput};

    fn ok_json(v: serde_json::Value) -> ToolResult {
        ToolResult::success(ToolOutput::json(v))
    }

    #[test]
    fn returns_path_for_successful_save_document() {
        let out = SaveDocumentOutput {
            virtual_path: "/exports/报表.xlsx".to_string(),
            kind: DocKind::Calc,
            size_bytes: 12,
        };
        let r = ok_json(serde_json::to_value(&out).unwrap());
        assert_eq!(extract_saved_path("save_document", &r), Some("/exports/报表.xlsx".to_string()));
    }

    #[test]
    fn other_tool_name_gives_none() {
        let r = ok_json(serde_json::json!({"virtual_path": "/exports/a.xlsx"}));
        assert_eq!(extract_saved_path("list_workspace", &r), None);
    }

    #[test]
    fn failed_result_gives_none() {
        let r = ToolResult::failed(ToolExecutionError::other("boom"));
        assert_eq!(extract_saved_path("save_document", &r), None);
    }

    #[test]
    fn missing_or_non_string_virtual_path_gives_none() {
        let r1 = ok_json(serde_json::json!({"kind": "calc"}));
        let r2 = ok_json(serde_json::json!({"virtual_path": 5}));
        assert_eq!(extract_saved_path("save_document", &r1), None);
        assert_eq!(extract_saved_path("save_document", &r2), None);
    }
}

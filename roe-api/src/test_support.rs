//! v119新增：测试/验证脚本共享的辅助函数。
//! 仅供 examples/ 下的验证脚本使用，不是正式 C-ABI 的一部分。
//!
//! 背景：render_calc_one.rs 和 render_calc_batch.rs 曾经各自维护一份几乎
//! 完全相同的 open_calc 函数，v119 修复 ROOT-001 时只同步更新了其中一份，
//! 导致另一份变成过期路径，核对时产生假阳性、浪费了排查时间。
//! 现在两者都改为调用这里的共享实现，以后只需要改一处。

use std::path::Path;

/// 打开一个 Calc 文档（xlsx/ods/fods），并执行一次"打开文档时的公式重算"
/// （等价于 roe_open_document_from_bytes 内部的行为），返回重算后的 Workbook。
pub fn open_calc_for_test(path: &str) -> Result<roe_model::calc::Workbook, String> {
    let data = std::fs::read(path).map_err(|e| format!("read error: {}", e))?;
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mut doc = match ext.as_str() {
        "xlsx" | "xlsm" => roe_parser::RoeDocument::open_xlsx_bytes(&data),
        "ods" => roe_parser::RoeDocument::open_odf_bytes(&data),
        "fods" => roe_parser::RoeDocument::open_fods_bytes(&data),
        other => return Err(format!("unsupported ext: {}", other)),
    }
    .map_err(|e| format!("parse error: {:?}", e))?;

    let _calc_engine = crate::document::init_calc_engine_for_doc(&mut doc);

    match doc {
        roe_parser::RoeDocument::Calc(wb) => Ok(wb),
        _ => Err("not a calc doc".to_string()),
    }
}

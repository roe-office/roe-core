//! Calc 列宽自动适应（C-ABI 层）。
//!
//! 与 char_format.rs / find_replace.rs 同一分层原则——本文件只负责
//! FFI 边界（handle 定位、sheet_idx 越界校验、列范围遍历、写回
//! sheet.col_widths），核心测量与宽度计算全部下沉在
//! roe_core::auto_fit::auto_fit_column_width，本文件不重新实现任何
//! 测量算法。
//!
//! 与 roe_set_column_width（calc_geometry.rs）的关系：本函数内部对
//! 每一列算出新宽度后，复用同款"col_widths 不足时 resize 补齐"的写回
//! 逻辑，但不直接调用 roe_set_column_width（避免重复走一次 handle
//! 查找/校验，本函数已经在 with_entry 闭包内持有 &mut Sheet）。
//!
//! 空列处理：范围内某一列没有任何可测量的非空单元格
//! （auto_fit_column_width 返回 None）时，跳过该列，保持其原有宽度
//! 不变，不强行回退到默认列宽——与 roe-core 层"没有可推算依据就不
//! 改动"的保守原则一致。

use roe_core::tile::CalcRenderConfig;
use roe_core::auto_fit::auto_fit_column_width;
use roe_parser::RoeDocument;

use crate::error::set_last_error;

/// 对 [start_col, end_col) 范围内的每一列做自动列宽适应，写回
/// sheet.col_widths。
///
/// 每列的新宽度取该列所有非空、非合并（非左上角）单元格按各自字体/
/// 字号测出的单行宽度最大值 + padding（见 roe_core::auto_fit 模块
/// 文档：不追求逐像素复刻某个具体 office 软件的官方算法，已实测
/// 确认合并单元格不参与所在列宽度计算）。
///
/// 扫描的行范围固定为该 sheet 当前实际有数据的最大行数（不额外接受
/// 行范围参数——自动列宽语义上是"整列"操作，不像 replace_text 那样
/// 有"只处理某一节"的精细化需求）。
///
/// 返回 true 表示成功（即使某些列因为是空列而被跳过，只要没有句柄/
/// 文档类型/越界错误，仍算成功）；失败（无效句柄/非 Calc 文档/
/// start_col >= end_col/sheet_idx 越界）返回 false，调用
/// roe_last_error_message 获取详情。
#[no_mangle]
pub extern "C" fn roe_auto_fit_columns(
    handle: u64,
    sheet_idx: u32,
    start_col: u32,
    end_col: u32,
) -> bool {
    if start_col >= end_col {
        set_last_error(format!(
            "roe_auto_fit_columns: start_col({start_col}) >= end_col({end_col})，非法区间"
        ));
        return false;
    }

    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Calc(workbook) = &mut entry.doc else {
            set_last_error("roe_auto_fit_columns: 非 Calc 文档");
            return false;
        };
        let Some(sheet) = workbook.sheets.get_mut(sheet_idx as usize) else {
            set_last_error(format!("roe_auto_fit_columns: sheet_idx={sheet_idx} 越界"));
            return false;
        };

        // 扫描行范围：该 sheet 实际有数据的最大行数（沿用
        // calc_geometry.rs::used_rows 同款推算方式：稀疏存储没有独立
        // 的"总行数"字段，按各列 CellBlock 的 start_row + values.len()
        // 累加求最大值）。
        let row_end = sheet
            .cells
            .cols
            .values()
            .flat_map(|sc| sc.blocks.iter())
            .map(|b| b.start_row + b.values.len() as u32)
            .max()
            .unwrap_or(0);

        let cfg = CalcRenderConfig::default();
        let mut font_context = parley::FontContext::new();
        let mut layout_context = parley::LayoutContext::new();

        for col in start_col..end_col {
            let Some(new_width) = auto_fit_column_width(
                sheet,
                col,
                0,
                row_end,
                &mut font_context,
                &mut layout_context,
            ) else {
                // 空列：保持原宽度不变，不算错误。
                continue;
            };

            let col_usize = col as usize;
            if sheet.col_widths.len() <= col_usize {
                sheet.col_widths.resize(col_usize, cfg.default_col_width);
                sheet.col_widths.push(new_width);
            } else {
                sheet.col_widths[col_usize] = new_width;
            }
        }

        true
    });

    result.unwrap_or_else(|| {
        set_last_error("roe_auto_fit_columns: 无效句柄");
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle;
    use roe_model::calc::{CellBlock, CellValue, SparseColumn, SparseGrid, Sheet, Workbook};
    use smallvec::SmallVec;
    use std::collections::{HashMap, VecDeque};

    fn open_calc_with_col(col: u32, texts: &[&str]) -> u64 {
        let values: SmallVec<[CellValue; 8]> = texts
            .iter()
            .map(|t| {
                if t.is_empty() {
                    CellValue::Empty
                } else {
                    CellValue::Text(t.to_string())
                }
            })
            .collect();
        let block = CellBlock { start_row: 0, values };
        let mut cols = HashMap::new();
        cols.insert(col, SparseColumn { blocks: vec![block].into() });

        let sheet = Sheet {
            name: "Sheet1".to_string(),
            cells: SparseGrid { cols },
            ..Default::default()
        };

        let workbook = Workbook {
            sheets: vec![sheet],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };

        handle::insert(handle::DocumentEntry {
            doc: RoeDocument::Calc(workbook),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: Default::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn get_col_width(h: u64, col: u32) -> f32 {
        handle::with_entry(h, |entry| {
            let RoeDocument::Calc(wb) = &entry.doc else { unreachable!() };
            let sheet = &wb.sheets[0];
            let cfg = CalcRenderConfig::default();
            roe_core::tile::col_width_at(sheet, col, &cfg)
        })
        .unwrap()
    }

    #[test]
    fn auto_fit_sets_wider_column_for_longer_text() {
        let h = open_calc_with_col(0, &["Hello World This Is Long Text"]);
        let before = get_col_width(h, 0);
        let ok = roe_auto_fit_columns(h, 0, 0, 1);
        assert!(ok);
        let after = get_col_width(h, 0);
        assert!(after > 0.0);
        // 长文字应该产生比默认列宽更宽的结果（默认列宽约48pt，长句子
        // 明显超过这个宽度）。
        assert!(after > before || after > 48.0);
    }

    #[test]
    fn auto_fit_invalid_range_returns_false() {
        let h = open_calc_with_col(0, &["Hi"]);
        let ok = roe_auto_fit_columns(h, 0, 5, 2);
        assert!(!ok);
    }

    #[test]
    fn auto_fit_invalid_handle_returns_false() {
        let ok = roe_auto_fit_columns(0, 0, 0, 1);
        assert!(!ok);
    }

    #[test]
    fn auto_fit_sheet_idx_out_of_range_returns_false() {
        let h = open_calc_with_col(0, &["Hi"]);
        let ok = roe_auto_fit_columns(h, 99, 0, 1);
        assert!(!ok);
    }

    #[test]
    fn auto_fit_empty_column_leaves_width_unchanged() {
        let h = open_calc_with_col(0, &["Hi"]);
        let before = get_col_width(h, 5); // 列5没有任何数据
        let ok = roe_auto_fit_columns(h, 0, 5, 6);
        assert!(ok);
        let after = get_col_width(h, 5);
        assert_eq!(before, after, "空列的宽度不应被改动");
    }
}

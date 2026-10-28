//! Calc 列宽自动适应（Gemini 缺口清单第1条 之一，ROE_Progress_v195.md
//! 后续会话新增）。
//!
//! 设计要点（与用户在 LibreOffice/WPS 实测对比后确认，见交接记录）：
//! - 不追求逐像素复刻某个具体 office 软件的官方算法——实测 WPS
//!   （8.25 字符）与 LibreOffice（0.89 英寸）默认列宽本身就不统一，
//!   说明这本来是"体验层面的合理值"而非有唯一标准答案的规范。
//! - 已确认的行为：合并单元格不参与所在列的自动列宽计算（只按未合并
//!   单元格的内容推算），与 measure_row_height_wrap "非左上角合并
//!   单元格 continue 跳过"的既有处理方式一致。
//! - 已确认的行为：自动列宽会在纯文字宽度基础上留出大致 1 个字符
//!   宽度的空白（实测观察），本实现按"半个字符宽度 + 固定 2pt 最小值"
//!   估算 padding，不是写死的固定 pt 数——同一字号/字体下 padding
//!   跟着字符宽度缩放，比固定值更合理，具体系数留有调整空间。
//! - 空列（该列范围内无任何非空、非合并单元格）返回 None，调用方
//!   应保持原列宽不变，不强行回退到默认列宽（与"没有可推算依据就
//!   不改动"的保守原则一致）。
//! - 测量口径与实际渲染一致：字体名同样先过
//!   resolve_font_family_for_parley 归一化，字号/字体 fallback 规则
//!   与 measure_row_height_wrap 完全一致（CellFormat 缺失时 fallback
//!   到 12.0pt / "Calibri"），避免测量结果与真实渲染出现"测绘撕裂"。

use roe_model::calc::Sheet;

use crate::row_height_measure::{measure_text_width_pt, resolve_font_family_for_parley};

/// 单列自动适应宽度计算。
///
/// 扫描 `sheet` 中第 `col_idx` 列、行范围 `[row_start, row_end)` 内所有
/// 非空单元格（跳过非左上角的合并单元格，跳过 CellValue::Empty），
/// 各自按其 CellFormat 的字号/字体测出单行不换行宽度，取最大值。
///
/// 返回 `Some(新列宽pt)`；范围内没有任何可测量的单元格时返回 `None`
/// （调用方应保持原列宽不变）。
pub fn auto_fit_column_width(
    sheet: &Sheet,
    col_idx: u32,
    row_start: u32,
    row_end: u32,
    font_context: &mut parley::FontContext,
    layout_context: &mut parley::LayoutContext,
) -> Option<f32> {
    let Some(sc) = sheet.cells.cols.get(&col_idx) else {
        return None;
    };

    let mut max_width: Option<f32> = None;

    for block in &sc.blocks {
        let block_end = block.start_row + block.values.len() as u32;
        let scan_start = row_start.max(block.start_row);
        let scan_end = row_end.min(block_end);
        if scan_start >= scan_end {
            continue;
        }

        for row in scan_start..scan_end {
            let i = (row - block.start_row) as usize;
            let val = match block.values.get(i) {
                Some(v) if !matches!(v, roe_model::calc::CellValue::Empty) => v,
                _ => continue,
            };

            // 跳过非左上角的合并单元格：与 measure_row_height_wrap 同款
            // 判断逻辑，已确认的用户实测行为（合并单元格不参与所在列的
            // 自动列宽计算）。
            let merge = sheet.merged_cells.iter().find(|mc| {
                row >= mc.r1 && row <= mc.r2 && col_idx >= mc.c1 && col_idx <= mc.c2
            });
            if let Some(mc) = merge {
                if row != mc.r1 || col_idx != mc.c1 {
                    continue;
                }
            }

            let cf = sheet
                .cell_formats
                .get(&(row, col_idx))
                .or_else(|| sheet.col_style.get(col_idx as usize).and_then(|o| o.as_ref()));

            let num_fmt = cf.and_then(|f| f.num_format.as_deref());
            let text = crate::tile_render_helpers::cell_value_to_text_fmt(val, num_fmt, sheet.date1904);
            if text.is_empty() {
                continue;
            }

            let font_size_pt = cf.and_then(|f| f.font_size).unwrap_or(12.0_f32);
            let font_family_raw = cf.and_then(|f| f.font_family.as_deref()).unwrap_or("Calibri");
            let font_family = resolve_font_family_for_parley(font_family_raw);

            let text_width = measure_text_width_pt(
                font_context,
                layout_context,
                &text,
                font_size_pt,
                font_family,
            );

            // padding：半个字符宽度（用一个中性字符 "0" 在同一字号/字体下
            // 的宽度近似"一个字符宽度"，取一半）+ 固定 2pt 最小值，避免
            // 极窄内容（如单个字符）时 padding 趋近于 0、贴边显得局促。
            let char_width = measure_text_width_pt(
                font_context,
                layout_context,
                "0",
                font_size_pt,
                font_family,
            );
            let padding = (char_width * 0.5).max(2.0_f32);

            let total_width = text_width + padding * 2.0;
            max_width = Some(max_width.map_or(total_width, |m: f32| m.max(total_width)));
        }
    }

    max_width
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::calc::{CellBlock, CellFormat, CellValue, MergedCell, Sheet, SparseColumn, SparseGrid};
    use smallvec::SmallVec;

    fn sheet_with_col(col: u32, texts: &[&str]) -> Sheet {
        let mut sheet = Sheet::default();
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
        let mut cols = std::collections::HashMap::new();
        cols.insert(col, SparseColumn { blocks: vec![block].into() });
        sheet.cells = SparseGrid { cols };
        sheet
    }

    fn ctx() -> (parley::FontContext, parley::LayoutContext) {
        (parley::FontContext::new(), parley::LayoutContext::new())
    }

    #[test]
    fn empty_column_returns_none() {
        let sheet = Sheet::default();
        let (mut fc, mut lc) = ctx();
        let result = auto_fit_column_width(&sheet, 0, 0, 10, &mut fc, &mut lc);
        assert!(result.is_none());
    }

    #[test]
    fn single_short_text_returns_some_positive_width() {
        let sheet = sheet_with_col(0, &["Hi"]);
        let (mut fc, mut lc) = ctx();
        let result = auto_fit_column_width(&sheet, 0, 0, 10, &mut fc, &mut lc);
        assert!(result.is_some());
        assert!(result.unwrap() > 0.0);
    }

    #[test]
    fn longer_text_produces_wider_result() {
        let sheet_short = sheet_with_col(0, &["Hi"]);
        let sheet_long = sheet_with_col(0, &["Hello World This Is Long"]);
        let (mut fc, mut lc) = ctx();
        let w_short = auto_fit_column_width(&sheet_short, 0, 0, 10, &mut fc, &mut lc).unwrap();
        let w_long = auto_fit_column_width(&sheet_long, 0, 0, 10, &mut fc, &mut lc).unwrap();
        assert!(w_long > w_short);
    }

    #[test]
    fn takes_max_width_across_rows() {
        let sheet = sheet_with_col(0, &["Hi", "A much longer piece of text here", "X"]);
        let (mut fc, mut lc) = ctx();
        let result = auto_fit_column_width(&sheet, 0, 0, 10, &mut fc, &mut lc).unwrap();

        let sheet_single = sheet_with_col(0, &["A much longer piece of text here"]);
        let expected = auto_fit_column_width(&sheet_single, 0, 0, 10, &mut fc, &mut lc).unwrap();
        assert!((result - expected).abs() < 0.01);
    }

    #[test]
    fn respects_row_range() {
        let sheet = sheet_with_col(0, &["Hi", "A much longer piece of text here"]);
        let (mut fc, mut lc) = ctx();
        // 只看第0行，不应受第1行更长文字的影响。
        let result_row0_only = auto_fit_column_width(&sheet, 0, 0, 1, &mut fc, &mut lc).unwrap();
        let result_all = auto_fit_column_width(&sheet, 0, 0, 2, &mut fc, &mut lc).unwrap();
        assert!(result_row0_only < result_all);
    }

    #[test]
    fn merged_cell_non_top_left_is_skipped() {
        let mut sheet = sheet_with_col(0, &["A very very long merged text that would dominate"]);
        // 把 (0,0) 声明为合并区域 (0,0)-(0,2) 的左上角——它应该仍参与
        // 计算（左上角本身不跳过，只跳过"非左上角"的合并单元格）。
        sheet.merged_cells.push(MergedCell { r1: 0, c1: 0, r2: 0, c2: 2 });
        let (mut fc, mut lc) = ctx();
        let result = auto_fit_column_width(&sheet, 0, 0, 1, &mut fc, &mut lc);
        assert!(result.is_some(), "左上角合并单元格本身应参与计算");
    }

    #[test]
    fn merged_cell_non_top_left_column_is_excluded_from_that_column() {
        // 合并区域 (0,0)-(0,2)：col 1 和 col 2 在 row 0 属于该合并区域的
        // "非左上角"部分。即使 col 1 自己在 sheet.cells 里被塞了内容
        // （模拟合并单元格底层仍保留原格数据的情况），auto_fit 对 col 1
        // 的计算也应该跳过它，不受这份内容影响所在列宽度。
        let mut sheet = sheet_with_col(1, &["A very very long text that should be ignored here"]);
        sheet.merged_cells.push(MergedCell { r1: 0, c1: 0, r2: 0, c2: 2 });
        let (mut fc, mut lc) = ctx();
        let result = auto_fit_column_width(&sheet, 1, 0, 1, &mut fc, &mut lc);
        assert!(result.is_none(), "非左上角的合并单元格不应参与所在列宽度计算");
    }

    #[test]
    fn cell_format_font_size_affects_width() {
        let mut sheet = sheet_with_col(0, &["Hello"]);
        sheet.cell_formats.insert((0, 0), CellFormat { font_size: Some(30.0), ..Default::default() });
        let (mut fc, mut lc) = ctx();
        let w_large = auto_fit_column_width(&sheet, 0, 0, 1, &mut fc, &mut lc).unwrap();

        let sheet_default = sheet_with_col(0, &["Hello"]);
        let w_default = auto_fit_column_width(&sheet_default, 0, 0, 1, &mut fc, &mut lc).unwrap();

        assert!(w_large > w_default, "更大字号应产生更宽的自动列宽");
    }
}

// v185后续：为 xlsx "真实文件 round-trip 测试"探测合适样本。
//
// 重要前提：xlsx_writer.rs 文件头注释（第1-8行）是v183时期的"第一版范围"
// 旧描述，已经过时——本次核实代码实际实现（而非仅凭注释）确认真实范围是：
//   支持（写出+有专门round-trip测试覆盖）：
//     - cell 值：Number/Text/Formula/Bool/Error/RichText
//     - merged_cells（合并单元格）
//     - cell_formats + conditional_formatting（单元格格式/条件格式）
//     - sheet_view（冻结窗格视图等）
//     - col_hidden/col_outline_level/row_hidden/row_outline_level（列行隐藏/分组）
//   不支持（确认无写出路径，非遗漏是明确的"本次任务范围外"）：
//     - col_widths（列宽数值本身，区别于hidden/outline属性）
//     - row_heights（行高数值）
//     - frozen_rows/frozen_cols（Sheet结构体字段存在，但writer未读取/未写出）
//
// 本脚本按这个"真实范围"（而非过时注释）统计每个文件的内容分布，挑选
// "干净"（不含 col_widths/row_heights/frozen_rows/frozen_cols 这几个真正
// 不支持的字段）且能覆盖较多"实际支持"字段（merged_cells/cell_formats/
// conditional_formatting/sheet_view/hidden+outline）的样本，尽量让
// round-trip 测试真正验证到 writer 现有能力，而不是仅仅验证最基础的
// cell 值写出（那样测试价值不够，起不到"真实文件验证"的作用）。
//
// 用法: cargo run --example scan_xlsx_roundtrip_candidates -p roe-parser -- <目录>

use roe_model::calc::CellValue;

#[derive(Default)]
struct Stat {
    file: String,
    sheet_count: usize,
    cell_count: usize,
    number_cells: usize,
    text_cells: usize,
    formula_cells: usize,
    bool_cells: usize,
    error_cells: usize,
    richtext_cells: usize,
    merged_cell_regions: usize,
    has_cell_format: bool,
    has_conditional_fmt: bool,
    has_sheet_view_nondefault: bool,
    has_col_row_hidden_outline: bool,
    has_col_widths: bool,
    has_row_heights: bool,
    has_frozen: bool,
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("用法: scan_xlsx_roundtrip_candidates <xlsx目录>");

    let mut stats: Vec<Stat> = Vec::new();
    let mut parse_failures: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("读取目录失败");
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("xlsx") {
            continue;
        }
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                parse_failures.push(format!("{file_name}: 读取失败 {e}"));
                continue;
            }
        };
        let wb = match roe_parser::ooxml::xlsx::XlsxParser::parse(&data) {
            Ok(w) => w,
            Err(e) => {
                parse_failures.push(format!("{file_name}: 解析失败 {e:?}"));
                continue;
            }
        };

        let mut st = Stat {
            file: file_name,
            sheet_count: wb.sheets.len(),
            ..Default::default()
        };

        for sheet in &wb.sheets {
            if !sheet.col_widths.is_empty() {
                st.has_col_widths = true;
            }
            if !sheet.row_heights.is_empty() {
                st.has_row_heights = true;
            }
            if sheet.frozen_rows > 0 || sheet.frozen_cols > 0 {
                st.has_frozen = true;
            }
            if sheet.col_hidden.iter().any(|&h| h)
                || sheet.col_outline_level.iter().any(|&o| o > 0)
                || sheet.row_hidden.iter().any(|&h| h)
                || sheet.row_outline_level.iter().any(|&o| o > 0)
            {
                st.has_col_row_hidden_outline = true;
            }
            if !sheet.merged_cells.is_empty() {
                st.merged_cell_regions += sheet.merged_cells.len();
            }

            for (_col_idx, column) in sheet.cells.cols.iter() {
                for block in &column.blocks {
                    for v in block.values.iter() {
                        st.cell_count += 1;
                        match v {
                            CellValue::Empty => {}
                            CellValue::Number(_) => st.number_cells += 1,
                            CellValue::Text(_) => st.text_cells += 1,
                            CellValue::Formula { .. } => st.formula_cells += 1,
                            CellValue::Bool(_) => st.bool_cells += 1,
                            CellValue::Error(_) => st.error_cells += 1,
                            CellValue::RichText(_) => st.richtext_cells += 1,
                        }
                    }
                }
            }
        }

        let wb_dbg = format!("{:?}", wb.styles);
        if wb_dbg.len() > 20 {
            st.has_cell_format = true;
        }

        stats.push(st);
    }

    stats.retain(|s| s.cell_count > 0);
    stats.sort_by(|a, b| {
        let a_clean = !a.has_col_widths && !a.has_row_heights && !a.has_frozen;
        let b_clean = !b.has_col_widths && !b.has_row_heights && !b.has_frozen;
        let a_features = a.merged_cell_regions.min(1) + (a.has_col_row_hidden_outline as usize)
            + (a.formula_cells > 0) as usize + (a.richtext_cells > 0) as usize;
        let b_features = b.merged_cell_regions.min(1) + (b.has_col_row_hidden_outline as usize)
            + (b.formula_cells > 0) as usize + (b.richtext_cells > 0) as usize;
        b_clean.cmp(&a_clean)
            .then(b_features.cmp(&a_features))
            .then(a.cell_count.cmp(&b.cell_count))
    });

    println!("=== 解析失败 {} 个 ===", parse_failures.len());
    for f in &parse_failures {
        println!("  ! {f}");
    }
    println!();
    println!("=== 候选样本（干净优先、特性覆盖数降序、cell_count升序）===");
    println!(
        "{:<55} sheets cells num text fml bool err rich merge hid/out widthsBAD heightsBAD frozenBAD",
        "file"
    );
    for s in stats.iter() {
        println!(
            "{:<55} {:>6} {:>5} {:>3} {:>4} {:>3} {:>4} {:>3} {:>4} {:>5} {:>7} {:>9} {:>10} {:>9}",
            s.file, s.sheet_count, s.cell_count, s.number_cells, s.text_cells,
            s.formula_cells, s.bool_cells, s.error_cells, s.richtext_cells,
            s.merged_cell_regions, s.has_col_row_hidden_outline,
            s.has_col_widths, s.has_row_heights, s.has_frozen
        );
    }
}

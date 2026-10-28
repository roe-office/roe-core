// v186后续：为 ods/fods "真实文件 round-trip 测试"探测合适样本。
//
// 重要前提：fods_writer.rs 文件头注释交叉核实结论（本次核实了实际
// match 分支，未发现类似 fodp_writer.rs Table/Chart/Group 那样的
// "BodyElement/ShapeContent级别静默丢弃"——fods_writer.rs 未发现任何
// `=> {}` 空分支，cell 值/合并单元格/单元格格式均有真实写出路径）：
//
//   完全支持（有写出路径 + parser 能读回，可验证 round-trip 相等）：
//     - CellValue::Number/Text/Formula/Bool/Error/RichText
//     - merged_cells（合并单元格）
//     - cell_formats（单元格格式，含 inline style）
//
//   有写出路径，但 parser 读不回来（文件头注释明确说明，非本脚本猜测）：
//     - cond_formats（条件格式）
//     - data_validations（数据验证）
//     - auto_filter（自动筛选）
//   这三项写出后符合 ODF 规范（供 LibreOffice 等外部软件识别），但 ROE
//   自身 parser 不解析这几个标签，无法在 round-trip 测试里断言"读回来
//   还是原值"——出现这几项的样本不适合做 round-trip 相等性断言的主要
//   验证点，应挑选不含这些字段、或即使含有也只验证"其余字段仍然相等"
//   的样本。
//
// 本脚本扫描 test_samples/curated/ods/ 下每个文件（同时处理 .ods 和
// .fods 两种扩展名），统计各 sheet 内容分布，挑出"干净"（不含
// cond_formats/data_validations/auto_filter）且尽量覆盖更多"完全支持"
// 字段（merged_cells/cell_formats/多种cell值类型）的样本。
//
// 用法: cargo run --example scan_ods_roundtrip_candidates -p roe-parser -- <目录>

use roe_model::calc::CellValue;
use roe_parser::RoeDocument;

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
    has_cond_formats: bool,      // 写出但读不回，不适合做主要验证点
    has_data_validations: bool,  // 同上
    has_auto_filter: bool,       // 同上
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("用法: scan_ods_roundtrip_candidates <ods目录>");

    let mut stats: Vec<Stat> = Vec::new();
    let mut parse_failures: Vec<String> = Vec::new();

    let entries = std::fs::read_dir(&dir).expect("读取目录失败");
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("ods") && ext != Some("fods") {
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

        let doc = if ext == Some("fods") {
            RoeDocument::open_fods_bytes(&data)
        } else {
            RoeDocument::open_odf_bytes(&data)
        };
        let doc = match doc {
            Ok(d) => d,
            Err(e) => {
                parse_failures.push(format!("{file_name}: 解析失败 {e:?}"));
                continue;
            }
        };
        let wb = match doc {
            RoeDocument::Calc(w) => w,
            other => {
                parse_failures.push(format!("{file_name}: 不是 Calc 文档 ({other:?} variant)"));
                continue;
            }
        };

        let mut st = Stat {
            file: file_name,
            sheet_count: wb.sheets.len(),
            ..Default::default()
        };

        for sheet in &wb.sheets {
            if !sheet.cond_formats.is_empty() {
                st.has_cond_formats = true;
            }
            if !sheet.data_validations.is_empty() {
                st.has_data_validations = true;
            }
            if sheet.auto_filter.is_some() {
                st.has_auto_filter = true;
            }
            if !sheet.merged_cells.is_empty() {
                st.merged_cell_regions += sheet.merged_cells.len();
            }
            if !sheet.cell_formats.is_empty() {
                st.has_cell_format = true;
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

        stats.push(st);
    }

    stats.retain(|s| s.cell_count > 0);
    stats.sort_by(|a, b| {
        let a_clean = !a.has_cond_formats && !a.has_data_validations && !a.has_auto_filter;
        let b_clean = !b.has_cond_formats && !b.has_data_validations && !b.has_auto_filter;
        let a_features = a.merged_cell_regions.min(1) + (a.has_cell_format as usize)
            + (a.formula_cells > 0) as usize + (a.richtext_cells > 0) as usize;
        let b_features = b.merged_cell_regions.min(1) + (b.has_cell_format as usize)
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
    println!("=== 候选样本（干净优先[无cond_fmt/valid/filter]、特性覆盖数降序、cell_count升序）===");
    println!(
        "{:<55} sheets cells num text fml bool err rich merge fmt condFmtBAD validBAD filterBAD",
        "file"
    );
    for s in stats.iter() {
        println!(
            "{:<55} {:>6} {:>5} {:>3} {:>4} {:>3} {:>4} {:>3} {:>4} {:>5} {:>3} {:>9} {:>7} {:>9}",
            s.file, s.sheet_count, s.cell_count, s.number_cells, s.text_cells,
            s.formula_cells, s.bool_cells, s.error_cells, s.richtext_cells,
            s.merged_cell_regions, s.has_cell_format,
            s.has_cond_formats, s.has_data_validations, s.has_auto_filter
        );
    }
}

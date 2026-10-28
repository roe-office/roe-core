//! CSV parser → roe-model::calc::Workbook
//!
//! 功能：
//!   - 自动检测分隔符（逗号 / 制表符 / 分号）
//!   - BOM 检测 + 编码转换（UTF-8 / UTF-16 / GBK / Shift-JIS → UTF-8）
//!   - 解析结果进 calc::SheetModel，和 xlsx 共享同一内存模型
//!   - 数字 / bool / 空值 / 文本自动推断 CellValue 类型

use encoding_rs::Encoding;
use roe_model::calc::{CellBlock, CellValue, FormulaDialectSource, Sheet, SparseColumn, SparseGrid, Workbook};
use crate::ParseError;

pub struct CsvParser;

impl CsvParser {
    pub fn parse(data: &[u8], sheet_name: Option<&str>) -> Result<Workbook, ParseError> {
        let utf8 = decode_to_utf8(data)?;
        let delimiter = detect_delimiter(&utf8);

        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(utf8.as_bytes());

        let mut col_data: std::collections::HashMap<u32, Vec<(u32, CellValue)>> =
            std::collections::HashMap::new();

        for (row_idx, result) in rdr.records().enumerate() {
            let record = result.map_err(|e| ParseError::Malformed(e.to_string()))?;
            let row = row_idx as u32;
            for (col_idx, field) in record.iter().enumerate() {
                let col = col_idx as u32;
                let val = infer_cell_value(field);
                if !matches!(val, CellValue::Empty) {
                    col_data.entry(col).or_default().push((row, val));
                }
            }
        }

        let grid = build_sparse_grid(col_data);
        let name = sheet_name.unwrap_or("Sheet1").to_string();
        let sheet = Sheet {
            col_style: vec![],
            tables: vec![],
            name,
            cells: grid,
            col_widths: Vec::new(),
            row_heights: Vec::new(),
            frozen_rows: 0,
            frozen_cols: 0,
        date1904: false, // ROOT-041: csv/odf场景无workbookPr概念，固定为1900系统
            zoom_scale: None,
            show_grid_lines: None,
            col_hidden: vec![],
            row_hidden: vec![],
            col_outline_level: vec![],
            row_outline_level: vec![],
            sparkline_groups: vec![],
            cell_formats:     std::collections::HashMap::new(),
            cond_formats:     vec![],
            data_validations: vec![],
            auto_filter:      None,
            merged_cells:     vec![],
            pivot:            None,
            floating_charts:  vec![],
            floating_stat_charts: vec![],
            floating_images:  vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            default_col_width: 0.0,
            default_row_height: 0.0,
        };

        Ok(Workbook {
            sheets: vec![sheet],
            named_ranges: std::collections::HashMap::new(),
            named_formulas: std::collections::HashMap::new(),
            styles: Default::default(),
            // ROOT-019: csv没有公式，方言无意义，用Excel占位即可。
            formula_dialect_source: FormulaDialectSource::Excel,
        })
    }
}

fn decode_to_utf8(data: &[u8]) -> Result<String, ParseError> {
    if data.starts_with(b"\xff\xfe") {
        let (cow, _, err) = Encoding::for_label(b"utf-16le").unwrap().decode(&data[2..]);
        if err { return Err(ParseError::Malformed("UTF-16LE decode error".into())); }
        return Ok(cow.into_owned());
    }
    if data.starts_with(b"\xfe\xff") {
        let (cow, _, err) = Encoding::for_label(b"utf-16be").unwrap().decode(&data[2..]);
        if err { return Err(ParseError::Malformed("UTF-16BE decode error".into())); }
        return Ok(cow.into_owned());
    }
    if data.starts_with(b"\xef\xbb\xbf") {
        return String::from_utf8(data[3..].to_vec())
            .map_err(|e| ParseError::Malformed(e.to_string()));
    }
    if let Ok(s) = std::str::from_utf8(data) {
        return Ok(s.to_string());
    }
    let (cow, _, err) = Encoding::for_label(b"gbk").unwrap().decode(data);
    if !err { return Ok(cow.into_owned()); }
    let (cow, _, _) = Encoding::for_label(b"shift_jis").unwrap().decode(data);
    Ok(cow.into_owned())
}

fn detect_delimiter(text: &str) -> u8 {
    let sample = &text[..text.len().min(4096)];
    let tabs   = sample.chars().filter(|&c| c == '\t').count();
    let commas = sample.chars().filter(|&c| c == ',').count();
    let semis  = sample.chars().filter(|&c| c == ';').count();
    if tabs >= commas && tabs >= semis { b'\t' }
    else if semis > commas { b';' }
    else { b',' }
}

fn infer_cell_value(s: &str) -> CellValue {
    let s = s.trim();
    if s.is_empty() { return CellValue::Empty; }
    if s.eq_ignore_ascii_case("true")  { return CellValue::Bool(true); }
    if s.eq_ignore_ascii_case("false") { return CellValue::Bool(false); }
    let clean = s.replace(',', "");
    if !s.chars().any(|c| c.is_alphabetic()) {
        if let Ok(n) = clean.parse::<f64>() {
            return CellValue::Number(n);
        }
    }
    CellValue::Text(s.to_string())
}

fn build_sparse_grid(
    col_data: std::collections::HashMap<u32, Vec<(u32, CellValue)>>,
) -> SparseGrid {
    use smallvec::SmallVec;
    let mut cols = std::collections::HashMap::new();
    for (col, mut rows) in col_data {
        rows.sort_by_key(|(r, _)| *r);
        let mut blocks: SmallVec<[CellBlock; 4]> = SmallVec::new();
        for (row, val) in rows {
            if let Some(last) = blocks.last_mut() {
                if row == last.start_row + last.values.len() as u32 {
                    last.values.push(val);
                    continue;
                }
            }
            let mut values: SmallVec<[CellValue; 8]> = SmallVec::new();
            values.push(val);
            blocks.push(CellBlock { start_row: row, values });
        }
        cols.insert(col, SparseColumn { blocks });
    }
    SparseGrid { cols }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_csv() {
        let data = b"name,age,score\nAlice,30,95.5\nBob,25,87.0\n";
        let wb = CsvParser::parse(data, None).unwrap();
        assert_eq!(wb.sheets[0].name, "Sheet1");
        let col1 = wb.sheets[0].cells.cols.get(&0).unwrap();
        assert!(matches!(&col1.blocks[0].values[0], CellValue::Text(s) if s == "name"));
        assert!(matches!(&col1.blocks[0].values[1], CellValue::Text(s) if s == "Alice"));
    }

    #[test]
    fn test_number_inference() {
        let data = b"x\n42\n3.14\n-7\n";
        let wb = CsvParser::parse(data, None).unwrap();
        let col1 = wb.sheets[0].cells.cols.get(&0).unwrap();
        assert!(matches!(col1.blocks[0].values[1], CellValue::Number(n) if (n-42.0).abs()<1e-9));
        assert!(matches!(col1.blocks[0].values[2], CellValue::Number(n) if (n-3.14).abs()<1e-9));
        assert!(matches!(col1.blocks[0].values[3], CellValue::Number(n) if (n+7.0).abs()<1e-9));
    }

    #[test]
    fn test_bool_inference() {
        let data = b"flag\ntrue\nfalse\nTRUE\n";
        let wb = CsvParser::parse(data, None).unwrap();
        let col1 = wb.sheets[0].cells.cols.get(&0).unwrap();
        assert!(matches!(col1.blocks[0].values[1], CellValue::Bool(true)));
        assert!(matches!(col1.blocks[0].values[2], CellValue::Bool(false)));
        assert!(matches!(col1.blocks[0].values[3], CellValue::Bool(true)));
    }

    #[test]
    fn test_tab_delimiter() {
        let data = b"a\tb\tc\n1\t2\t3\n";
        let wb = CsvParser::parse(data, None).unwrap();
        assert!(wb.sheets[0].cells.cols.contains_key(&2));
    }

    #[test]
    fn test_semicolon_delimiter() {
        let data = b"a;b;c\n1;2;3\n";
        let wb = CsvParser::parse(data, None).unwrap();
        assert!(wb.sheets[0].cells.cols.contains_key(&2));
    }

    #[test]
    fn test_utf8_bom() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(b"name,value\nhello,42\n");
        let wb = CsvParser::parse(&data, Some("MySheet")).unwrap();
        assert_eq!(wb.sheets[0].name, "MySheet");
    }

    #[test]
    fn test_empty_cells_skipped() {
        let data = b"a,,c\n1,,3\n";
        let wb = CsvParser::parse(data, None).unwrap();
        assert!(!wb.sheets[0].cells.cols.contains_key(&1));
        assert!(wb.sheets[0].cells.cols.contains_key(&0));
        assert!(wb.sheets[0].cells.cols.contains_key(&2));
    }

    #[test]
    fn test_custom_sheet_name() {
        let data = b"x,y\n1,2\n";
        let wb = CsvParser::parse(data, Some("Sales")).unwrap();
        assert_eq!(wb.sheets[0].name, "Sales");
    }
}

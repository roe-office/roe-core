//! xlsx 读写接口
//! 读取：calamine（支持 xlsx/xls/ods）

use polars::prelude::*;
use calamine::{open_workbook_auto, Reader, Data};

fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Int(n) => format!("{}", n),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{}", f)
            }
        },
        Data::String(s) => s.clone(),
        Data::Bool(b) => format!("{}", b),
        Data::DateTime(dt) => format!("{}", dt),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        _ => String::new(),
    }
}

/// 从 xlsx/xls/ods 文件路径读取第一个 Sheet → DataFrame
pub fn df_from_xlsx(path: &str) -> Result<DataFrame, String> {
    let mut workbook = open_workbook_auto(path)
        .map_err(|e| format!("calamine 打开失败: {e}"))?;

    let sheet_names = workbook.sheet_names().to_vec();
    let sheet_name = sheet_names.first()
        .ok_or("文件没有 sheet")?
        .clone();

    let range = workbook.worksheet_range(&sheet_name)
        .map_err(|e| format!("读取 sheet 失败: {e}"))?;

    let mut rows = range.rows();

    let headers: Vec<String> = match rows.next() {
        Some(row) => row.iter().map(cell_to_string).collect(),
        None => return Err("文件为空".to_string()),
    };

    let mut columns: Vec<Vec<String>> = vec![vec![]; headers.len()];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < columns.len() {
                columns[i].push(cell_to_string(cell));
            }
        }
        for i in row.len()..columns.len() {
            columns[i].push(String::new());
        }
    }

    let cols: Vec<Column> = headers.iter().zip(columns.iter())
        .map(|(name, col)| {
            Column::new(name.as_str().into(), col.as_slice())
        })
        .collect();

    DataFrame::new_infer_height(cols)
        .map_err(|e| format!("DataFrame 构建失败: {e}"))
}

/// DataFrame → TSV 字符串（含表头）
pub fn df_to_tsv(df: &DataFrame) -> String {
    let mut lines = Vec::new();
    let headers: Vec<String> = df.get_column_names()
        .iter().map(|s| s.to_string()).collect();
    lines.push(headers.join("\t"));

    for i in 0..df.height() {
        let row: Vec<String> = df.columns().iter().map(|col: &Column| {
            col.get(i).map(|v| match v {
                AnyValue::Null => String::new(),
                AnyValue::String(s) => s.to_string(),
                AnyValue::StringOwned(s) => s.to_string(),
                other => format!("{}", other),
            }).unwrap_or_default()
        }).collect();
        lines.push(row.join("\t"));
    }
    lines.join("\n")
}

pub fn df_to_xlsx(_df: &DataFrame, _path: &str) -> Result<(), String> {
    todo!("xlsx 写入：rust_xlsxwriter 接入后实现")
}

/// 返回前 max_rows 行数据（含表头），用于预览 sheet
pub fn df_preview(df: &DataFrame, max_rows: usize) -> (Vec<String>, Vec<Vec<String>>) {
    let headers: Vec<String> = df.get_column_names()
        .iter().map(|s| s.to_string()).collect();
    let n = df.height().min(max_rows);
    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        let row: Vec<String> = df.columns().iter().map(|col: &Column| {
            col.get(i).map(|v| match v {
                AnyValue::Null => String::new(),
                AnyValue::String(s) => s.to_string(),
                AnyValue::StringOwned(s) => s.to_string(),
                other => format!("{}", other),
            }).unwrap_or_default()
        }).collect();
        rows.push(row);
    }
    (headers, rows)
}

/// 生成 describe 统计表：列名/类型/总行数/非空数/空值数/均值/最大/最小
pub fn df_describe(df: &DataFrame) -> (Vec<String>, Vec<Vec<String>>) {
    let headers = vec![
        "列名".to_string(), "类型".to_string(), "总行数".to_string(),
        "非空数".to_string(), "空值数".to_string(),
        "均值".to_string(), "最小值".to_string(), "最大值".to_string(),
    ];
    let total = df.height();
    let mut rows = Vec::new();
    for col in df.columns() {
        let name = col.name().to_string();
        let dtype = format!("{}", col.dtype());
        let null_count = col.null_count();
        let non_null = total - null_count;
        // 尝试数值统计
        let (mean_s, min_s, max_s) = if let Ok(s) = col.cast(&DataType::Float64) {
            let ca = s.f64().unwrap();
            let mean = ca.mean().map(|v| format!("{:.2}", v)).unwrap_or_default();
            let min  = ca.min().map(|v| {
                if v == v.floor() { format!("{:.0}", v) } else { format!("{:.2}", v) }
            }).unwrap_or_default();
            let max  = ca.max().map(|v| {
                if v == v.floor() { format!("{:.0}", v) } else { format!("{:.2}", v) }
            }).unwrap_or_default();
            (mean, min, max)
        } else {
            (String::new(), String::new(), String::new())
        };
        rows.push(vec![
            name, dtype,
            total.to_string(), non_null.to_string(), null_count.to_string(),
            mean_s, min_s, max_s,
        ]);
    }
    (headers, rows)
}

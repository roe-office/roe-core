//! Polars 数据透视表引擎 (v0.53)
use polars::prelude::*;
use polars::prelude::BooleanChunked;
extern crate encoding_rs;
use serde::{Deserialize, Serialize};

pub mod xlsx_io;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PivotConfig {
    pub rows: Vec<String>,
    pub cols: Vec<String>,
    pub values: Vec<String>,
    /// "sum" | "mean" | "count" | "min" | "max"
    pub agg: String,
    /// 筛选条件：{"字段名": ["值1","值2"]}，多值OR，多字段AND
    #[serde(default)]
    pub filters: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PivotResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

pub fn df_from_csv_bytes(bytes: &[u8]) -> PolarsResult<DataFrame> {
    // 尝试UTF-8，失败则用encoding_rs转码（GBK/Latin1等）
    let utf8_bytes: std::borrow::Cow<[u8]> = if std::str::from_utf8(bytes).is_ok() {
        std::borrow::Cow::Borrowed(bytes)
    } else {
        // 先尝试GBK
        let (decoded, _, had_errors) = encoding_rs::GBK.decode(bytes);
        if !had_errors {
            std::borrow::Cow::Owned(decoded.into_owned().into_bytes())
        } else {
            // 降级到Latin1（无损）
            let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
            std::borrow::Cow::Owned(decoded.into_owned().into_bytes())
        }
    };
    CsvReadOptions::default()
        .with_has_header(true)
        .into_reader_with_file_handle(std::io::Cursor::new(utf8_bytes.as_ref()))
        .finish()
}

pub fn run_pivot(df: &DataFrame, cfg: &PivotConfig) -> PolarsResult<DataFrame> {
    // 先按 filters 过滤 DataFrame
    let df = if cfg.filters.is_empty() {
        df.clone()
    } else {
        let mut mask = polars::prelude::BooleanChunked::from_slice("m".into(), &vec![true; df.height()]);
        for (field, allowed_vals) in &cfg.filters {
            if allowed_vals.is_empty() { continue; }
            if let Ok(col) = df.column(field) {
                let col_str = col.cast(&polars::prelude::DataType::String)?;
                let col_ca = col_str.str()?;
                let field_mask = col_ca.iter().map(|v| {
                    v.map(|s| allowed_vals.iter().any(|av| av == s)).unwrap_or(false)
                }).collect::<BooleanChunked>();
                mask = mask & field_mask;
            }
        }
        df.filter(&mask)?
    };
    if cfg.cols.is_empty() {
        run_groupby(&df, cfg)
    } else {
        run_manual_pivot(&df, cfg)
    }
}

/// 手动 pivot：groupby → 宽表展开
fn run_manual_pivot(df: &DataFrame, cfg: &PivotConfig) -> PolarsResult<DataFrame> {
    if cfg.values.is_empty() {
        return run_groupby(df, cfg);
    }
    if cfg.rows.is_empty() {
        if cfg.cols.is_empty() {
            return run_groupby(df, cfg);
        }
        return run_col_only_pivot(df, cfg);
    }

    let multi_val = cfg.values.len() > 1;

    // 多列字段时把所有列字段值用"|"拼接为复合key
    let composite_col = "__pivot_col_key__";
    let df = if cfg.cols.len() == 1 {
        let col_name = &cfg.cols[0];
        df.clone().lazy()
            .with_column(col(col_name.as_str()).cast(DataType::String).alias(composite_col))
            .collect()?
    } else {
        let col_exprs: Vec<Expr> = cfg.cols.iter()
            .map(|c| col(c.as_str()).cast(DataType::String))
            .collect();
        let concat_expr = concat_str(col_exprs, "|", true);
        df.clone().lazy()
            .with_column(concat_expr.alias(composite_col))
            .collect()?
    };

    // col_vals: 所有列组合，字母序排序
    let col_series_all = df.column(composite_col)?.as_series()
        .ok_or_else(|| PolarsError::ComputeError("composite_col not a Series".into()))?;
    let mut col_vals: Vec<String> = Vec::new();
    let mut col_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for v in col_series_all.iter() {
        let s = format_anyvalue(&v);
        if col_seen.insert(s.clone()) { col_vals.push(s); }
    }
    col_vals.sort();

    // 复合 row key
    let composite_row_key = "__pivot_row_key__";
    let df_with_row_key = if cfg.rows.len() > 1 {
        let row_exprs: Vec<Expr> = cfg.rows.iter()
            .map(|r| col(r.as_str()).cast(DataType::String))
            .collect();
        let concat_row = concat_str(row_exprs, "|", true);
        df.clone().lazy()
            .with_column(concat_row.alias(composite_row_key))
            .collect()?
    } else {
        df.clone().lazy()
            .with_column(col(cfg.rows[0].as_str()).cast(DataType::String).alias(composite_row_key))
            .collect()?
    };
    let row_key_series = df_with_row_key.column(composite_row_key)?.as_series()
        .ok_or_else(|| PolarsError::ComputeError("row_key not a Series".into()))?;
    let mut seen = std::collections::HashSet::new();
    let mut row_keys: Vec<String> = Vec::new();
    for v in row_key_series.iter() {
        let s = format_anyvalue(&v);
        if seen.insert(s.clone()) { row_keys.push(s); }
    }

    // 行字段列
    let mut result_cols: Vec<Column> = Vec::new();
    for (ri, rf) in cfg.rows.iter().enumerate() {
        let vals: Vec<String> = row_keys.iter().map(|rk| {
            rk.split('|').nth(ri).unwrap_or("").to_string()
        }).collect();
        result_cols.push(Column::new(rf.as_str().into(),
            vals.iter().map(|s| s.as_str()).collect::<Vec<_>>()));
    }

    // 预先对每个 value_field 做 groupby，存入 Vec
    struct GroupedVal {
        val_series: Series,
        col_series: Series,
        grk_series: Series,
        height: usize,
    }
    let mut grouped_vals: Vec<(String, GroupedVal)> = Vec::new();
    for value_col_name in &cfg.values {
        let mut group_keys: Vec<Expr> = cfg.rows.iter().map(|r| col(r.as_str())).collect();
        group_keys.push(col(composite_col));
        let agg_expr = make_agg_expr(value_col_name, &cfg.agg, value_col_name);
        let grouped = df.clone().lazy()
            .group_by(group_keys)
            .agg([agg_expr])
            .sort([cfg.rows[0].as_str()], SortMultipleOptions::default())
            .collect()?;
        let val_series = grouped.column(value_col_name)?.as_series()
            .ok_or_else(|| PolarsError::ComputeError("value_col not a Series".into()))?;
        let col_series = grouped.column(composite_col)?.as_series()
            .ok_or_else(|| PolarsError::ComputeError("composite_col grouped not a Series".into()))?;
        let grouped_with_rk = if cfg.rows.len() > 1 {
            let row_exprs: Vec<Expr> = cfg.rows.iter()
                .map(|r| col(r.as_str()).cast(DataType::String))
                .collect();
            let concat_row = concat_str(row_exprs, "|", true);
            grouped.clone().lazy()
                .with_column(concat_row.alias(composite_row_key))
                .collect()?
        } else {
            grouped.clone().lazy()
                .with_column(col(cfg.rows[0].as_str()).cast(DataType::String).alias(composite_row_key))
                .collect()?
        };
        let grk_series = grouped_with_rk.column(composite_row_key)?.as_series()
            .ok_or_else(|| PolarsError::ComputeError("grk not a Series".into()))?;
        let h = grouped_with_rk.height();
        grouped_vals.push((value_col_name.clone(), GroupedVal {
            val_series: val_series.clone(), col_series: col_series.clone(), grk_series: grk_series.clone(), height: h,
        }));
    }

    // 外层 col_vals，内层 values — 交错排列（与原生LibreOffice一致）
    for cv in &col_vals {
        for (value_col_name, gv) in &grouped_vals {
            let col_name = if multi_val {
                format!("{} - {}", cv, value_col_name)
            } else {
                cv.clone()
            };
            let mut values_for_col: Vec<Option<f64>> = Vec::with_capacity(row_keys.len());
            for rk in &row_keys {
                let mut found: Option<f64> = None;
                for i in 0..gv.height {
                    let rv  = format_anyvalue(&gv.grk_series.get(i)?);
                    let ccv = format_anyvalue(&gv.col_series.get(i)?);
                    if rv == *rk && ccv == *cv {
                        found = anyvalue_to_f64(&gv.val_series.get(i)?);
                        break;
                    }
                }
                values_for_col.push(found);
            }
            result_cols.push(Column::new(col_name.as_str().into(), values_for_col));
        }
    }

    DataFrame::new_infer_height(result_cols)
}

/// 纯列字段pivot：groupby(cols) agg(values)，结果转置为单行宽表
/// 输出格式：列名=col值, 行只有一行（总计）
fn run_col_only_pivot(df: &DataFrame, cfg: &PivotConfig) -> PolarsResult<DataFrame> {
    let composite_col = "__pivot_col_key__";
    // 多列字段时拼接复合key
    let df = if cfg.cols.len() == 1 {
        df.clone().lazy()
            .with_column(col(cfg.cols[0].as_str()).cast(DataType::String).alias(composite_col))
            .collect()?
    } else {
        let col_exprs: Vec<Expr> = cfg.cols.iter()
            .map(|c| col(c.as_str()).cast(DataType::String))
            .collect();
        df.clone().lazy()
            .with_column(concat_str(col_exprs, "|", true).alias(composite_col))
            .collect()?
    };

    let agg_exprs: Vec<Expr> = cfg.values.iter()
        .map(|v| make_agg_expr(v, &cfg.agg, v))
        .collect();

    let grouped = df.clone().lazy()
        .group_by([col(composite_col)])
        .agg(agg_exprs)
        .sort([composite_col], SortMultipleOptions::default())
        .collect()?;

    // 取复合col唯一值（已排序）
    let col_series = grouped.column(composite_col)?.as_series()
        .ok_or_else(|| PolarsError::ComputeError("composite_col not series".into()))?;
    let col_vals: Vec<String> = col_series.iter().map(|v| format_anyvalue(&v)).collect();

    // 构建宽表：每个col组合×每个value字段 = 一列，只有一行
    let multi_val = cfg.values.len() > 1;
    let mut result_cols: Vec<Column> = Vec::new();
    for val_field in &cfg.values {
        let val_series = grouped.column(val_field)?.as_series()
            .ok_or_else(|| PolarsError::ComputeError("val not series".into()))?;
        for (i, cv) in col_vals.iter().enumerate() {
            let col_name = if multi_val {
                format!("{} - {}", cv, val_field)
            } else {
                cv.clone()
            };
            let v = anyvalue_to_f64(&val_series.get(i)?);
            result_cols.push(Column::new(col_name.as_str().into(), vec![v]));
        }
    }
    DataFrame::new_infer_height(result_cols)
}

fn run_groupby(df: &DataFrame, cfg: &PivotConfig) -> PolarsResult<DataFrame> {
    if cfg.rows.is_empty() {
        return run_global_agg(df, cfg);
    }
    let group_by: Vec<Expr> = cfg.rows.iter().map(|c| col(c.as_str())).collect();
    let agg_exprs: Vec<Expr> = if cfg.values.is_empty() {
        vec![col(cfg.rows[0].as_str()).count().alias("count")]
    } else {
        cfg.values.iter().map(|v| make_agg_expr(v, &cfg.agg, v)).collect()
    };
    df.clone().lazy()
        .group_by(group_by)
        .agg(agg_exprs)
        .sort(
            cfg.rows.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            SortMultipleOptions::default(),
        )
        .collect()
}

fn run_global_agg(df: &DataFrame, cfg: &PivotConfig) -> PolarsResult<DataFrame> {
    let agg_exprs: Vec<Expr> = if cfg.values.is_empty() {
        vec![lit(df.height() as i64).alias("count")]
    } else {
        cfg.values.iter().map(|v| make_agg_expr(v, &cfg.agg, v)).collect()
    };
    df.clone().lazy().select(agg_exprs).collect()
}

fn make_agg_expr(col_name: &str, agg: &str, alias_name: &str) -> Expr {
    match agg {
        "mean"  => col(col_name).mean().alias(alias_name),
        "count" => col(col_name).count().alias(alias_name),
        "min"   => col(col_name).min().alias(alias_name),
        "max"   => col(col_name).max().alias(alias_name),
        _       => col(col_name).sum().alias(alias_name),
    }
}

fn anyvalue_to_f64(v: &AnyValue) -> Option<f64> {
    match v {
        AnyValue::Float64(f) => Some(*f),
        AnyValue::Float32(f) => Some(*f as f64),
        AnyValue::Int32(i)   => Some(*i as f64),
        AnyValue::Int64(i)   => Some(*i as f64),
        AnyValue::UInt32(i)  => Some(*i as f64),
        AnyValue::UInt64(i)  => Some(*i as f64),
        _                    => None,
    }
}

pub fn df_to_result(df: &DataFrame) -> PolarsResult<PivotResult> {
    let columns: Vec<String> = df.get_column_names()
        .iter().map(|s| s.to_string()).collect();
    let height = df.height();
    let mut rows = Vec::with_capacity(height);
    for i in 0..height {
        let row: Vec<String> = df.columns().iter()
            .map(|series: &Column| {
                match series.get(i) {
                    Ok(v) => format_anyvalue(&v),
                    Err(_) => String::new(),
                }
            }).collect();
        rows.push(row);
    }
    Ok(PivotResult { columns, rows })
}

fn format_anyvalue(v: &AnyValue) -> String {
    match v {
        AnyValue::Null           => String::new(),
        AnyValue::Float64(f)     => if *f == f.floor() { format!("{:.0}", f) } else { format!("{:.2}", f) },
        AnyValue::Float32(f)     => { let f = *f as f64; if f == f.floor() { format!("{:.0}", f) } else { format!("{:.2}", f) } },
        AnyValue::String(s)      => s.to_string(),
        AnyValue::StringOwned(s) => s.to_string(),
        other                    => format!("{}", other),
    }
}

pub fn pivot_from_csv_json(csv_bytes: &[u8], config_json: &str) -> Result<String, String> {
    let cfg: PivotConfig = serde_json::from_str(config_json)
        .map_err(|e| format!("config parse error: {e}"))?;
    let df = df_from_csv_bytes(csv_bytes)
        .map_err(|e| format!("csv read error: {e}"))?;
    let pivoted = run_pivot(&df, &cfg)
        .map_err(|e| format!("pivot error: {e}"))?;
    let result = df_to_result(&pivoted)
        .map_err(|e| format!("result error: {e}"))?;
    serde_json::to_string(&result)
        .map_err(|e| format!("json error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_pivot() {
        let csv = b"Region,Product,Sales\nEast,Apple,100\nEast,Banana,200\nWest,Apple,150\nWest,Banana,50\n";
        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec!["Product".into()],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let df = df_from_csv_bytes(csv).unwrap();
        let pivoted = run_pivot(&df, &cfg).unwrap();
        let result = df_to_result(&pivoted).unwrap();
        println!("columns: {:?}", result.columns);
        println!("rows: {:?}", result.rows);
        assert!(result.columns.contains(&"Apple".to_string()));
        assert!(result.columns.contains(&"Banana".to_string()));
    }

    #[test]
    fn test_groupby_no_cols() {
        let csv = b"Region,Product,Sales\nEast,Apple,100\nEast,Banana,200\nWest,Apple,150\nWest,Banana,50\n";
        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec![],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let df = df_from_csv_bytes(csv).unwrap();
        let pivoted = run_pivot(&df, &cfg).unwrap();
        let result = df_to_result(&pivoted).unwrap();
        println!("groupby columns: {:?}", result.columns);
        println!("groupby rows: {:?}", result.rows);
        assert!(result.columns.contains(&"Region".to_string()));
        assert!(result.columns.contains(&"Sales".to_string()));
    }
}

/// SQL查询：接收CSV字节和SQL语句，返回JSON结果
/// table_name: 注册的表名（默认"data"）
/// sql: SQL查询语句，例如 "SELECT region, SUM(sales) FROM data GROUP BY region"
pub fn run_sql_on_df(df: polars::prelude::DataFrame, table_name: &str, sql: &str) -> Result<String, String> {
    use polars::sql::SQLContext;
    let mut ctx = SQLContext::new();
    ctx.register(table_name, df.lazy());
    let result_df = ctx.execute(sql)
        .map_err(|e| format!("SQL执行失败: {}", e))?
        .collect()
        .map_err(|e| format!("收集结果失败: {}", e))?;
    let columns: Vec<String> = result_df.get_column_names()
        .iter().map(|s| s.to_string()).collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for i in 0..result_df.height() {
        let row: Vec<String> = result_df.columns()
            .iter()
            .map(|col: &polars::prelude::Column| -> String {
                col.get(i).map(|v: polars::prelude::AnyValue| match v {
                    polars::prelude::AnyValue::Null => String::new(),
                    other => format!("{}", other),
                }).unwrap_or_default()
            }).collect();
        rows.push(row);
    }
    let result = serde_json::json!({ "columns": columns, "rows": rows });
    Ok(result.to_string())
}

pub fn run_sql_on_csv(csv_bytes: &[u8], table_name: &str, sql: &str) -> Result<String, String> {
    use polars::sql::SQLContext;

    let df = df_from_csv_bytes(csv_bytes).map_err(|e| format!("CSV解析失败: {}", e))?;

    let mut ctx = SQLContext::new();
    ctx.register(table_name, df.lazy());

    let result_df = ctx.execute(sql)
        .map_err(|e| format!("SQL执行失败: {}", e))?
        .collect()
        .map_err(|e| format!("收集结果失败: {}", e))?;

    // 转为PivotResult格式JSON
    let columns: Vec<String> = result_df.get_column_names()
        .iter().map(|s| s.to_string()).collect();

    let mut rows: Vec<Vec<String>> = Vec::new();
    for i in 0..result_df.height() {
        let row: Vec<String> = result_df.columns()
            .iter()
            .map(|col: &Column| -> String {
                match col.as_series() {
                    Some(s) => s.get(i).map(|v: polars::prelude::AnyValue| match v {
                        polars::prelude::AnyValue::Null => String::new(),
                        other => format!("{}", other),
                    }).unwrap_or_default(),
                    None => col.get(i).map(|v: polars::prelude::AnyValue| match v {
                        polars::prelude::AnyValue::Null => String::new(),
                        other => format!("{}", other),
                    }).unwrap_or_default(),
                }
            })
            .collect();
        rows.push(row);
    }

    let result = PivotResult { columns, rows };
    serde_json::to_string(&result).map_err(|e| format!("JSON序列化失败: {}", e))
}

/// 获取某列的所有唯一值，返回 JSON 数组字符串
pub fn get_unique_values(csv_bytes: &[u8], field: &str) -> Result<String, String> {
    let df = df_from_csv_bytes(csv_bytes)
        .map_err(|e| format!("csv read error: {e}"))?;
    let col = df.column(field)
        .map_err(|_| format!("field '{}' not found", field))?;
    let series = col.as_series()
        .ok_or_else(|| format!("field '{}' is not a series", field))?;
    let unique = series.unique()
        .map_err(|e| format!("unique error: {e}"))?;
    let vals: Vec<String> = unique.iter()
        .map(|v: polars::prelude::AnyValue| v.to_string().trim_matches('"').to_string())
        .collect();
    serde_json::to_string(&vals)
        .map_err(|e| format!("json error: {e}"))
}

/// 返回CSV的结构信息JSON（列名+类型+统计），不含明文内容
/// 供 AI 索引使用，符合隐私原则
pub fn df_info_from_csv(csv_bytes: &[u8]) -> Result<String, String> {
    let df = df_from_csv_bytes(csv_bytes)
        .map_err(|e| format!("CSV解析失败: {}", e))?;
    let row_count = df.height();
    let schema = df.schema();
    let cols: Vec<String> = schema.iter().map(|(name, dtype)| {
        let type_str = match dtype {
            DataType::Float64 | DataType::Float32 => "float",
            DataType::Int32 | DataType::Int64 |
            DataType::UInt32 | DataType::UInt64 => "int",
            DataType::Boolean => "bool",
            DataType::Date | DataType::Datetime(_, _) => "datetime",
            _ => "string",
        };
        let stats = if type_str == "float" || type_str == "int" {
            if let Ok(col) = df.column(name.as_str()) {
                if let Some(s) = col.as_series() {
                    let min = s.min::<f64>().ok().flatten()
                        .map(|v| format!(",\"min\":{:.2}", v))
                        .unwrap_or_default();
                    let max = s.max::<f64>().ok().flatten()
                        .map(|v| format!(",\"max\":{:.2}", v))
                        .unwrap_or_default();
                    let null_c = s.null_count();
                    format!(",\"null_count\":{}{}{}", null_c, min, max)
                } else { String::new() }
            } else { String::new() }
        } else if type_str == "string" {
            if let Ok(col) = df.column(name.as_str()) {
                if let Some(s) = col.as_series() {
                    let unique_n = s.n_unique().unwrap_or(0);
                    format!(",\"unique\":{}", unique_n)
                } else { String::new() }
            } else { String::new() }
        } else { String::new() };
        format!("{{\"name\":\"{name}\",\"type\":\"{type_str}\"{stats}}}")
    }).collect();
    Ok(format!("{{\"rows\":{row_count},\"columns\":[{}]}}", cols.join(",")))
}

pub fn df_info_from_df(df: &DataFrame) -> Result<String, String> {
    df_info_from_csv_inner(df)
}

fn df_info_from_csv_inner(df: &DataFrame) -> Result<String, String> {
    let row_count = df.height();
    let schema = df.schema();
    let cols: Vec<String> = schema.iter().map(|(name, dtype)| {
        let type_str = match dtype {
            DataType::Float64 | DataType::Float32 => "float",
            DataType::Int32 | DataType::Int64 |
            DataType::UInt32 | DataType::UInt64 => "int",
            DataType::Boolean => "bool",
            DataType::Date | DataType::Datetime(_, _) => "datetime",
            _ => "string",
        };
        let stats = if type_str == "float" || type_str == "int" {
            if let Ok(col) = df.column(name.as_str()) {
                if let Some(s) = col.as_series() {
                    let min_v = s.min::<f64>().ok().flatten()
                        .map(|v| format!(",\"min\":{:.2}", v))
                        .unwrap_or_default();
                    let max_v = s.max::<f64>().ok().flatten()
                        .map(|v| format!(",\"max\":{:.2}", v))
                        .unwrap_or_default();
                    let null_c = s.null_count();
                    format!(",\"null_count\":{}{}{}", null_c, min_v, max_v)
                } else { String::new() }
            } else { String::new() }
        } else if type_str == "string" {
            if let Ok(col) = df.column(name.as_str()) {
                if let Some(s) = col.as_series() {
                    let unique_n = s.n_unique().unwrap_or(0);
                    format!(",\"unique\":{}", unique_n)
                } else { String::new() }
            } else { String::new() }
        } else { String::new() };
        format!("{{\"name\":\"{}\",\"type\":\"{}\"{}}}", name, type_str, stats)
    }).collect();
    Ok(format!("{{\"rows\":{},\"columns\":[{}]}}", row_count, cols.join(",")))
}

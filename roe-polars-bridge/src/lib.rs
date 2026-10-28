//! roe-polars-bridge：Workbook ↔ Polars DataFrame 桥接
//!
//! - Sheet <-> DataFrame 双向转换（第一行为表头）
//! - 透视表 / SQL 查询结果直接构造为新 Sheet 追加到 Workbook
//!   （替代 office-rs 的 FODS 影子副本 + XML 标签注入方案，类型安全）
//! - calamine 大文件直读旁路：df_from_xlsx_file / run_sql_on_dataframe，
//!   不进入 ROE 内存模型，用于大体积外部文件分析
//!
//! 后续（TODO）：Rhai 绑定 DataFrame 操作 API（df 分析功能，工作量较大，留待后续任务）

use polars::prelude::*;
use smallvec::{smallvec, SmallVec};

use roe_model::calc::{CellBlock, CellValue, PivotFieldDef, PivotHeaderNode, PivotMeta, Sheet, SparseColumn, SparseGrid, Workbook};

pub mod pivot_engine;
pub use pivot_engine::{PivotConfig, PivotResult, run_pivot};

#[derive(thiserror::Error, Debug)]
pub enum BridgeError {
    #[error("polars error: {0}")]
    Polars(#[from] PolarsError),
    #[error("{0}")]
    Other(String),
}

// ════════════════════════════════════════════════════════════
// SparseGrid 辅助
// ════════════════════════════════════════════════════════════

/// 计算 SparseGrid 的实际尺寸（最大行数, 最大列数）
fn grid_dimensions(grid: &SparseGrid) -> (u32, u32) {
    let mut max_row = 0u32;
    let mut max_col = 0u32;
    for (&col_idx, col) in &grid.cols {
        max_col = max_col.max(col_idx + 1);
        for block in &col.blocks {
            let end = block.start_row + block.values.len() as u32;
            max_row = max_row.max(end);
        }
    }
    (max_row, max_col)
}

fn get_cell(grid: &SparseGrid, row: u32, col: u32) -> CellValue {
    match grid.cols.get(&col) {
        Some(column) => {
            for block in &column.blocks {
                if row >= block.start_row && row < block.start_row + block.values.len() as u32 {
                    return block.values[(row - block.start_row) as usize].clone();
                }
            }
            CellValue::Empty
        }
        None => CellValue::Empty,
    }
}

fn cell_to_opt_string(v: &CellValue) -> Option<String> {
    match v {
        CellValue::Empty => None,
        CellValue::Number(n) => Some(if n.fract() == 0.0 {
            format!("{}", *n as i64)
        } else {
            n.to_string()
        }),
        CellValue::Text(s) => Some(s.clone()),
        // ROOT-078: 数据聚合场景只关心文字内容、不关心格式，拼接全部
        // run的文本返回，语义等价于"忽略富文本格式、只要纯文字"。
        CellValue::RichText(runs) => {
            Some(runs.iter().map(|r| r.text.as_str()).collect::<String>())
        }
        CellValue::Formula { source, .. } => Some(source.clone()),
        CellValue::Bool(b) => Some(b.to_string()),
        CellValue::Error(_) => None,
    }
}

fn anyvalue_to_cell(v: &AnyValue) -> CellValue {
    match v {
        AnyValue::Null => CellValue::Empty,
        AnyValue::Boolean(b) => CellValue::Bool(*b),
        AnyValue::String(s) => CellValue::Text(s.to_string()),
        AnyValue::StringOwned(s) => CellValue::Text(s.to_string()),
        AnyValue::Float64(f) => CellValue::Number(*f),
        AnyValue::Float32(f) => CellValue::Number(*f as f64),
        AnyValue::Int64(i) => CellValue::Number(*i as f64),
        AnyValue::Int32(i) => CellValue::Number(*i as f64),
        AnyValue::UInt64(i) => CellValue::Number(*i as f64),
        AnyValue::UInt32(i) => CellValue::Number(*i as f64),
        other => CellValue::Text(format!("{other}")),
    }
}

fn anyvalue_to_json(v: &AnyValue) -> serde_json::Value {
    match v {
        AnyValue::Null => serde_json::Value::Null,
        AnyValue::Boolean(b) => serde_json::Value::Bool(*b),
        AnyValue::String(s) => serde_json::Value::String(s.to_string()),
        AnyValue::StringOwned(s) => serde_json::Value::String(s.to_string()),
        AnyValue::Float64(f) => serde_json::json!(f),
        AnyValue::Float32(f) => serde_json::json!(f),
        AnyValue::Int64(i) => serde_json::json!(i),
        AnyValue::Int32(i) => serde_json::json!(i),
        AnyValue::UInt64(i) => serde_json::json!(i),
        AnyValue::UInt32(i) => serde_json::json!(i),
        other => serde_json::Value::String(format!("{other}")),
    }
}

// ════════════════════════════════════════════════════════════
// df_preview / df_describe：DataFrame -> JSON（AI 友好）
// ════════════════════════════════════════════════════════════

/// DataFrame 预览：前 n_rows 行，返回 JSON
/// 格式：{"columns": [...], "rows": [[...], ...], "total_rows": N}
pub fn df_preview(df: &DataFrame, n_rows: usize) -> Result<String, BridgeError> {
    let columns: Vec<String> = df
        .get_column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let n = n_rows.min(df.height());
    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        let mut row = Vec::with_capacity(columns.len());
        for col in df.columns() {
            let av = col.get(i).unwrap_or(AnyValue::Null);
            row.push(anyvalue_to_json(&av));
        }
        rows.push(row);
    }

    let result = serde_json::json!({
        "columns": columns,
        "rows": rows,
        "total_rows": df.height(),
    });
    Ok(result.to_string())
}

/// DataFrame 结构信息：行数 + 每列 (name, type, null_count, min/max 或 unique)
/// 格式与 office-rs pivot-engine::df_info_from_df 保持一致（已生产验证）
pub fn df_info(df: &DataFrame) -> Result<String, BridgeError> {
    let row_count = df.height();
    let schema = df.schema();

    let mut cols_json = Vec::new();
    for (name, dtype) in schema.iter() {
        let type_str = match dtype {
            DataType::Float64 | DataType::Float32 => "float",
            DataType::Int32 | DataType::Int64 | DataType::UInt32 | DataType::UInt64 => "int",
            DataType::Boolean => "bool",
            DataType::Date | DataType::Datetime(_, _) => "datetime",
            _ => "string",
        };

        let mut col_obj = serde_json::json!({
            "name": name.to_string(),
            "type": type_str,
        });

        if type_str == "float" || type_str == "int" {
            if let Ok(col) = df.column(name.as_str()) {
                if let Some(s) = col.as_series() {
                    col_obj["null_count"] = serde_json::json!(s.null_count());
                    if let Ok(Some(min_v)) = s.min::<f64>() {
                        col_obj["min"] = serde_json::json!(min_v);
                    }
                    if let Ok(Some(max_v)) = s.max::<f64>() {
                        col_obj["max"] = serde_json::json!(max_v);
                    }
                }
            }
        } else if type_str == "string" {
            if let Ok(col) = df.column(name.as_str()) {
                if let Some(s) = col.as_series() {
                    col_obj["unique"] = serde_json::json!(s.n_unique().unwrap_or(0));
                }
            }
        }

        cols_json.push(col_obj);
    }

    let result = serde_json::json!({
        "rows": row_count,
        "columns": cols_json,
    });
    Ok(result.to_string())
}

/// 对 workbook.sheets[sheet_idx] 做预览，返回 JSON
pub fn sheet_preview(workbook: &Workbook, sheet_idx: usize, n_rows: usize) -> Result<String, BridgeError> {
    let sheet = workbook
        .sheets
        .get(sheet_idx)
        .ok_or_else(|| BridgeError::Other(format!("sheet index {sheet_idx} out of range")))?;
    let df = sheet_to_dataframe(sheet)?;
    df_preview(&df, n_rows)
}

/// 对 workbook.sheets[sheet_idx] 做结构信息摘要，返回 JSON
pub fn sheet_info(workbook: &Workbook, sheet_idx: usize) -> Result<String, BridgeError> {
    let sheet = workbook
        .sheets
        .get(sheet_idx)
        .ok_or_else(|| BridgeError::Other(format!("sheet index {sheet_idx} out of range")))?;
    let df = sheet_to_dataframe(sheet)?;
    df_info(&df)
}

// ════════════════════════════════════════════════════════════
// Sheet -> DataFrame
// ════════════════════════════════════════════════════════════

/// Sheet -> DataFrame，第一行作为表头（列名）；剩余行为数据。
/// 每列按内容自动判定为数值列（全部 Number/Empty）或文本列（其余情况，混合类型转文本）。
pub fn sheet_to_dataframe(sheet: &Sheet) -> Result<DataFrame, BridgeError> {
    let (height, width) = grid_dimensions(&sheet.cells);

    if width == 0 {
        return Ok(DataFrame::empty());
    }

    let mut columns: Vec<Column> = Vec::with_capacity(width as usize);
    for c in 0..width {
        let header = match get_cell(&sheet.cells, 0, c) {
            CellValue::Text(s) if !s.is_empty() => s,
            _ => format!("col{}", c + 1),
        };

        let cells: Vec<CellValue> = (1..height).map(|r| get_cell(&sheet.cells, r, c)).collect();
        let all_numeric = cells
            .iter()
            .all(|v| matches!(v, CellValue::Number(_) | CellValue::Empty));

        let series: Series = if all_numeric {
            let data: Vec<Option<f64>> = cells
                .iter()
                .map(|v| match v {
                    CellValue::Number(n) => Some(*n),
                    _ => None,
                })
                .collect();
            Series::new(header.as_str().into(), data)
        } else {
            let data: Vec<Option<String>> = cells.iter().map(cell_to_opt_string).collect();
            Series::new(header.as_str().into(), data)
        };
        columns.push(series.into());
    }

    let data_height = height.saturating_sub(1) as usize;
    Ok(DataFrame::new(data_height, columns)?)
}

// ════════════════════════════════════════════════════════════
// 透视表数据区聚合计算（现场计算，不依赖xlsx静态快照）
// ════════════════════════════════════════════════════════════

/// 单个值字段的聚合方式
#[derive(Debug, Clone)]
pub struct PivotValueFieldSpec {
    pub source_column: String,
    pub agg: String, // "sum" | "mean" | "count" | "min" | "max"
}

/// 从原始明细 DataFrame 现场聚合计算透视表数据区数值。
///
/// row_fields / col_fields：字段名列表（外→内顺序，对应 PivotMeta.row_field_names / col_field_names）
/// value_fields：值字段定义列表
///
/// 输出：PivotDataMatrix，key 为 (行叶子路径, 列叶子路径, 值字段序号)。
/// 小计/总计通过额外的 group_by 调用单独计算，分别写入路径中对应层级为空字符串的 key
/// （约定：这一层是小计/总计的标记方式，渲染层查值时需要按此约定构造 key）。
///
/// TODO（小计功能，设计已完成，见 ROE_Subtotal_Design_v1.md，暂缓实现）：
/// 小计 = 路径长度为 N-1 的聚合（N 是该方向字段总层数），与总计（路径长度为0）
/// 是同一思路的推广。接入时在此函数现有"按完整路径分组"基础上，为每个
/// 非叶子父节点额外做一次"按去掉最后一层的路径分组"的 group_by，写入同一个
/// PivotDataMatrix。注意：GrandTotal 的聚合不能把小计行也算进去重复计算。
pub fn compute_pivot_data_matrix(
    df: &DataFrame,
    row_fields: &[String],
    col_fields: &[String],
    value_fields: &[PivotValueFieldSpec],
) -> Result<roe_model::calc::PivotDataMatrix, BridgeError> {
    let mut matrix = roe_model::calc::PivotDataMatrix::default();

    let mut group_cols: Vec<String> = Vec::new();
    group_cols.extend(row_fields.iter().cloned());
    group_cols.extend(col_fields.iter().cloned());

    if group_cols.is_empty() {
        // 无行无列字段：所有值字段直接对整表聚合，路径均为空
        for (vidx, vf) in value_fields.iter().enumerate() {
            if let Some(v) = aggregate_column(df, &vf.source_column, &vf.agg)? {
                matrix.insert(vec![], vec![], vidx, v);
            }
        }
        return Ok(matrix);
    }

    let lf = df.clone().lazy();
    let group_exprs: Vec<Expr> = group_cols.iter().map(|c| col(c.as_str())).collect();

    for (vidx, vf) in value_fields.iter().enumerate() {
        let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
        let grouped = lf.clone()
            .group_by(group_exprs.clone())
            .agg([agg_expr])
            .collect()?;

        let n_rows = grouped.height();
        for i in 0..n_rows {
            let mut row_path: Vec<String> = Vec::with_capacity(row_fields.len());
            let mut col_path: Vec<String> = Vec::with_capacity(col_fields.len());
            for (fidx, fname) in row_fields.iter().enumerate() {
                let _ = fidx;
                let s = grouped.column(fname)?;
                row_path.push(anyvalue_to_display_string(&s.get(i)?));
            }
            for fname in col_fields.iter() {
                let s = grouped.column(fname)?;
                col_path.push(anyvalue_to_display_string(&s.get(i)?));
            }
            let result_col_name = format!("{}_agg", vf.source_column);
            let val_series = grouped.column(&result_col_name)
                .or_else(|_| grouped.column(&vf.source_column))?;
            if let AnyValue::Null = val_series.get(i)? {
                continue;
            }
            let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
            matrix.insert(row_path, col_path, vidx, val);
        }
    }

    // v151新增：行方向小计聚合（仅最外层，见 pivot_v2_insert_row_subtotals 注释）。
    // 分组字段只用 row_fields 第一层 + 全部 col_fields，实现"该外层分组下所有子路径求和"。
    // 写入 matrix 时 row_path 补齐到 row_fields.len() 长度：第0位=真实分组值，其余层补空字符串，
    // 与树渲染层（tile.rs 第1015行左右）小计节点查值时的 path 构造方式保持一致约定。
    if !row_fields.is_empty() {
        let subtotal_group_cols: Vec<String> = {
            let mut v = vec![row_fields[0].clone()];
            v.extend(col_fields.iter().cloned());
            v
        };
        let subtotal_group_exprs: Vec<Expr> = subtotal_group_cols.iter().map(|c| col(c.as_str())).collect();

        for (vidx, vf) in value_fields.iter().enumerate() {
            let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
            let grouped = lf.clone()
                .group_by(subtotal_group_exprs.clone())
                .agg([agg_expr])
                .collect()?;

            let n_rows = grouped.height();
            for i in 0..n_rows {
                let outer_val = {
                    let s = grouped.column(&row_fields[0])?;
                    anyvalue_to_display_string(&s.get(i)?)
                };
                let mut row_path: Vec<String> = vec![String::new(); row_fields.len()];
                row_path[0] = outer_val;

                let mut col_path: Vec<String> = Vec::with_capacity(col_fields.len());
                for fname in col_fields.iter() {
                    let s = grouped.column(fname)?;
                    col_path.push(anyvalue_to_display_string(&s.get(i)?));
                }

                let result_col_name = format!("{}_agg", vf.source_column);
                let val_series = grouped.column(&result_col_name)
                    .or_else(|_| grouped.column(&vf.source_column))?;
                if let AnyValue::Null = val_series.get(i)? {
                    continue;
                }
                let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
                matrix.insert(row_path, col_path, vidx, val);
            }
        }
    }

    // v152：列方向小计聚合（仅最外层，对称于上面的行方向小计分支）。
    // 分组字段只用 col_fields 第一层 + 全部 row_fields，实现"该外层列分组下
    // 所有子路径求和"。写入 matrix 时 col_path 补齐到 col_fields.len() 长度：
    // 第0位=真实分组值，其余层补空字符串；row_path 照常填满完整路径。
    if !col_fields.is_empty() {
        let col_subtotal_group_cols: Vec<String> = {
            let mut v = vec![col_fields[0].clone()];
            v.extend(row_fields.iter().cloned());
            v
        };
        let col_subtotal_group_exprs: Vec<Expr> = col_subtotal_group_cols.iter().map(|c| col(c.as_str())).collect();

        for (vidx, vf) in value_fields.iter().enumerate() {
            let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
            let grouped = lf.clone()
                .group_by(col_subtotal_group_exprs.clone())
                .agg([agg_expr])
                .collect()?;

            let n_rows = grouped.height();
            for i in 0..n_rows {
                let outer_val = {
                    let s = grouped.column(&col_fields[0])?;
                    anyvalue_to_display_string(&s.get(i)?)
                };
                let mut col_path: Vec<String> = vec![String::new(); col_fields.len()];
                col_path[0] = outer_val;

                let mut row_path: Vec<String> = Vec::with_capacity(row_fields.len());
                for fname in row_fields.iter() {
                    let s = grouped.column(fname)?;
                    row_path.push(anyvalue_to_display_string(&s.get(i)?));
                }

                let result_col_name = format!("{}_agg", vf.source_column);
                let val_series = grouped.column(&result_col_name)
                    .or_else(|_| grouped.column(&vf.source_column))?;
                if let AnyValue::Null = val_series.get(i)? {
                    continue;
                }
                let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
                matrix.insert(row_path, col_path, vidx, val);
            }
        }
    }

    // v152：行列小计交汇点（仅当 row_fields 和 col_fields 都非空时才存在这个格子）。
    // 分组字段 = row_fields[0] + col_fields[0]（两头都只取最外层）。这是唯一
    // 一个"行小计"分支和"列小计"分支都不会生成的 key 组合（前者 col_path 是
    // 完整路径，后者 row_path 是完整路径，唯独两者同时半空的组合没人写）。
    // 正确性要求：这个值必须同时等于"该行小计横向求和"和"该列小计纵向求和"
    // （用户用真实数据截图验证过的关系，见 v152 交接记录），因为两条聚合路径
    // 都是从原始明细 df 现场 group_by 出来的，数学上必然相等，不需要额外交叉验证代码，
    // 但单元测试应显式断言这一点。
    if !row_fields.is_empty() && !col_fields.is_empty() {
        let corner_group_cols = vec![row_fields[0].clone(), col_fields[0].clone()];
        let corner_group_exprs: Vec<Expr> = corner_group_cols.iter().map(|c| col(c.as_str())).collect();

        for (vidx, vf) in value_fields.iter().enumerate() {
            let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
            let grouped = lf.clone()
                .group_by(corner_group_exprs.clone())
                .agg([agg_expr])
                .collect()?;

            let n_rows = grouped.height();
            for i in 0..n_rows {
                let row_outer_val = {
                    let s = grouped.column(&row_fields[0])?;
                    anyvalue_to_display_string(&s.get(i)?)
                };
                let col_outer_val = {
                    let s = grouped.column(&col_fields[0])?;
                    anyvalue_to_display_string(&s.get(i)?)
                };
                let mut row_path: Vec<String> = vec![String::new(); row_fields.len()];
                row_path[0] = row_outer_val;
                let mut col_path: Vec<String> = vec![String::new(); col_fields.len()];
                col_path[0] = col_outer_val;

                let result_col_name = format!("{}_agg", vf.source_column);
                let val_series = grouped.column(&result_col_name)
                    .or_else(|_| grouped.column(&vf.source_column))?;
                if let AnyValue::Null = val_series.get(i)? {
                    continue;
                }
                let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
                matrix.insert(row_path, col_path, vidx, val);
            }
        }
    }

    // ROOT-063: row_fields 中间层前缀聚合（col_fields 为空场景专用）。
    // 背景：xlsx rowItems 里，每一条 <i> 记录不论其声明路径长度是否达到
    // row_fields.len()（最深层），都对应一个独立的、需要显示自身聚合值的
    // 物理行（不是"小计"，是 OOXML 源数据自己声明的数据行，语义上与叶子
    // 行平级）。此前只有完整路径（上面的循环）被写入矩阵，中间层前缀路径
    // 查询会 miss，导致渲染层这些行数值空白。
    //
    // 范围限定：仅当 col_fields 为空时才做这个前缀聚合。已用 xlsx 静态
    // 缓存值（sheet1.xml 的 <v> 标签，Excel/OnlyOffice 官方计算结果）
    // 交叉验证过：col_fields 非空场景下，行方向中间层本来就不显示数值
    // （Excel 官方行为如此，不是 bug），故不需要也不应该为 col_fields
    // 非空场景做这个聚合，避免引入不必要的计算开销和潜在的错误显示。
    //
    // 算法：自底向上级联聚合（而非对原始明细表重复扫描 N 次）。上面的
    // 循环已经算出了最细粒度（完整 row_fields）的聚合结果 `grouped`；
    // 这里从该结果出发，依次去掉最后一层字段、对剩余字段重新分组聚合，
    // 数据量已经是聚合后的小数据集，开销可忽略。
    //
    // 重要限制：此级联聚合仅对满足代数闭包性质的聚合函数正确
    // （sum/count/min/max：agg(agg(x)) == agg(x)）。"mean" 不满足
    // 此性质（多组子平均值的平均 != 全部原始数据的平均），故 mean
    // 类型的 value_field 不走级联聚合，改为对原始明细表按对应前缀
    // 重新 group_by（退化为非级联，但保证结果正确）。
    if row_fields.len() >= 2 && col_fields.is_empty() {
        for (vidx, vf) in value_fields.iter().enumerate() {
            let is_mean = vf.agg == "mean";
            // prefix_len 从 row_fields.len()-1 递减到 1（完整路径=len()，已在上面处理；
            // 0层即总计，交由后面的 GrandTotal 逻辑处理，此处不重复）
            for prefix_len in (1..row_fields.len()).rev() {
                let prefix_fields = &row_fields[..prefix_len];
                let prefix_group_exprs: Vec<Expr> = prefix_fields.iter().map(|c| col(c.as_str())).collect();
                let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);

                let grouped = if is_mean {
                    // mean 不可级联，直接对原始明细表按前缀重新分组
                    lf.clone()
                        .group_by(prefix_group_exprs.clone())
                        .agg([agg_expr])
                        .collect()?
                } else {
                    // 可级联聚合函数：基于已有的完整叶子层聚合结果（本轮 vidx 对应的
                    // 那份 `grouped`，来自上面完整路径聚合循环的最后一次迭代结果）
                    // 重新分组，避免重新扫描原始明细表。
                    // 注意：上面循环里 `grouped` 变量的作用域已结束，这里需要重新拿到
                    // 完整路径聚合结果；为保持实现简单、避免额外的生命周期管理，
                    // 直接从原始 lf 按当前 vf 的完整 row_fields 重新聚合一次作为级联
                    // 聚合的输入源（该聚合本身开销与上面完整路径循环相同，非重复
                    // 全表扫描的 N 倍增长，只是多了 O(row_fields.len()) 次对"已聚合
                    // 结果"的再分组，这部分数据量小，可忽略）。
                    let leaf_group_exprs: Vec<Expr> = row_fields.iter().map(|c| col(c.as_str())).collect();
                    let leaf_agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
                    let leaf_grouped = lf.clone()
                        .group_by(leaf_group_exprs)
                        .agg([leaf_agg_expr])
                        .collect()?;
                    let result_col_name = format!("{}_agg", vf.source_column);
                    let reagg_expr = match vf.agg.as_str() {
                        "count" => col(&result_col_name).sum().alias(&result_col_name),
                        "min" => col(&result_col_name).min().alias(&result_col_name),
                        "max" => col(&result_col_name).max().alias(&result_col_name),
                        _ => col(&result_col_name).sum().alias(&result_col_name),
                    };
                    leaf_grouped.lazy()
                        .group_by(prefix_group_exprs.clone())
                        .agg([reagg_expr])
                        .collect()?
                };

                let n_rows = grouped.height();
                for i in 0..n_rows {
                    let mut row_path: Vec<String> = Vec::with_capacity(prefix_len);
                    for fname in prefix_fields.iter() {
                        let s = grouped.column(fname)?;
                        row_path.push(anyvalue_to_display_string(&s.get(i)?));
                    }
                    let result_col_name = format!("{}_agg", vf.source_column);
                    let val_series = grouped.column(&result_col_name)
                        .or_else(|_| grouped.column(&vf.source_column))?;
                    if let AnyValue::Null = val_series.get(i)? {
                        continue;
                    }
                    let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
                    matrix.insert(row_path, vec![], vidx, val);
                }
            }
        }
    }

    // RowTotal（col_path=空，跨列汇总）：只要 row_fields 非空就需要，
    // 不要求 col_fields 也非空（例如只有row没有col的场景，RowTotal就是竖向总计本身）
    if !row_fields.is_empty() {
        let row_group_exprs: Vec<Expr> = row_fields.iter().map(|c| col(c.as_str())).collect();
        for (vidx, vf) in value_fields.iter().enumerate() {
            let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
            let grouped = lf.clone()
                .group_by(row_group_exprs.clone())
                .agg([agg_expr])
                .collect()?;
            let n_rows = grouped.height();
            for i in 0..n_rows {
                let mut row_path: Vec<String> = Vec::with_capacity(row_fields.len());
                for fname in row_fields.iter() {
                    let s = grouped.column(fname)?;
                    row_path.push(anyvalue_to_display_string(&s.get(i)?));
                }
                let result_col_name = format!("{}_agg", vf.source_column);
                let val_series = grouped.column(&result_col_name)
                    .or_else(|_| grouped.column(&vf.source_column))?;
                if let AnyValue::Null = val_series.get(i)? {
                    continue;
                }
                let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
                matrix.insert(row_path, vec![], vidx, val);
            }
        }
    }

    // ColTotal（row_path=空，跨行汇总）：只要 col_fields 非空就需要，
    // 不要求 row_fields 也非空（例如只有col没有row的场景，ColTotal就是横向总计本身）
    if !col_fields.is_empty() {
        let col_group_exprs: Vec<Expr> = col_fields.iter().map(|c| col(c.as_str())).collect();
        for (vidx, vf) in value_fields.iter().enumerate() {
            let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
            let grouped = lf.clone()
                .group_by(col_group_exprs.clone())
                .agg([agg_expr])
                .collect()?;
            let n_rows = grouped.height();
            for i in 0..n_rows {
                let mut col_path: Vec<String> = Vec::with_capacity(col_fields.len());
                for fname in col_fields.iter() {
                    let s = grouped.column(fname)?;
                    col_path.push(anyvalue_to_display_string(&s.get(i)?));
                }
                let result_col_name = format!("{}_agg", vf.source_column);
                let val_series = grouped.column(&result_col_name)
                    .or_else(|_| grouped.column(&vf.source_column))?;
                if let AnyValue::Null = val_series.get(i)? {
                    continue;
                }
                let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
                matrix.insert(vec![], col_path, vidx, val);
            }
        }
    }

    // GrandTotal：只要 row_fields 或 col_fields 至少一个非空就需要
    // （两者都空的场景已在函数开头 group_cols.is_empty() 分支处理过，这里不会重复）
    if !row_fields.is_empty() || !col_fields.is_empty() {
        for (vidx, vf) in value_fields.iter().enumerate() {
            if let Some(v) = aggregate_column(df, &vf.source_column, &vf.agg)? {
                matrix.insert(vec![], vec![], vidx, v);
            }
        }
    }

    // v152：总计行 × 列小计 的交汇点（row_path=[]，col_path=[列小计外层值,"",...]）。
    // 之前只写了 row_path=[] 配完整col_path（ColTotal分支）和 row_path=col_path=[]（GrandTotal单点），
    // 缺了"总计跟列小计交汇"这一格——分组字段只用 col_fields[0]（不含任何row字段，
    // 相当于在总计口径下按最外层列分组）。仅当 col_fields 非空时才存在这个格子。
    if !col_fields.is_empty() {
        let col_field_group_exprs: Vec<Expr> = vec![col(col_fields[0].as_str())];
        for (vidx, vf) in value_fields.iter().enumerate() {
            let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
            let grouped = lf.clone()
                .group_by(col_field_group_exprs.clone())
                .agg([agg_expr])
                .collect()?;
            let n_rows = grouped.height();
            for i in 0..n_rows {
                let outer_val = {
                    let s = grouped.column(&col_fields[0])?;
                    anyvalue_to_display_string(&s.get(i)?)
                };
                let mut col_path: Vec<String> = vec![String::new(); col_fields.len()];
                col_path[0] = outer_val;

                let result_col_name = format!("{}_agg", vf.source_column);
                let val_series = grouped.column(&result_col_name)
                    .or_else(|_| grouped.column(&vf.source_column))?;
                if let AnyValue::Null = val_series.get(i)? {
                    continue;
                }
                let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
                matrix.insert(vec![], col_path, vidx, val);
            }
        }
    }

    // v152：行小计 × 总计列 的交汇点（row_path=[行小计外层值,"",...]，col_path=[]）。
    // 对称于上面的"总计 × 列小计"，分组字段只用 row_fields[0]。仅当 row_fields 非空时才存在。
    if !row_fields.is_empty() {
        let row_field_group_exprs: Vec<Expr> = vec![col(row_fields[0].as_str())];
        for (vidx, vf) in value_fields.iter().enumerate() {
            let agg_expr = build_agg_expr(&vf.source_column, &vf.agg);
            let grouped = lf.clone()
                .group_by(row_field_group_exprs.clone())
                .agg([agg_expr])
                .collect()?;
            let n_rows = grouped.height();
            for i in 0..n_rows {
                let outer_val = {
                    let s = grouped.column(&row_fields[0])?;
                    anyvalue_to_display_string(&s.get(i)?)
                };
                let mut row_path: Vec<String> = vec![String::new(); row_fields.len()];
                row_path[0] = outer_val;

                let result_col_name = format!("{}_agg", vf.source_column);
                let val_series = grouped.column(&result_col_name)
                    .or_else(|_| grouped.column(&vf.source_column))?;
                if let AnyValue::Null = val_series.get(i)? {
                    continue;
                }
                let val: f64 = val_series.get(i)?.try_extract::<f64>().unwrap_or(0.0);
                matrix.insert(row_path, vec![], vidx, val);
            }
        }
    }

    Ok(matrix)
}

fn build_agg_expr(source_column: &str, agg: &str) -> Expr {
    let c = col(source_column);
    let result_name = format!("{}_agg", source_column);
    match agg {
        "mean" => c.mean().alias(result_name),
        "count" => c.count().alias(result_name),
        "min" => c.min().alias(result_name),
        "max" => c.max().alias(result_name),
        _ => c.sum().alias(result_name),
    }
}

fn aggregate_column(df: &DataFrame, source_column: &str, agg: &str) -> Result<Option<f64>, BridgeError> {
    let col = df.column(source_column)?;
    let s = col.as_materialized_series();
    let v = match agg {
        "mean" => s.mean(),
        "count" => Some(s.len() as f64),
        "min" => s.min::<f64>().ok().flatten(),
        "max" => s.max::<f64>().ok().flatten(),
        _ => s.sum::<f64>().ok(),
    };
    Ok(v)
}

fn anyvalue_to_display_string(v: &AnyValue) -> String {
    match v {
        AnyValue::Null => String::new(),
        AnyValue::String(s) => s.to_string(),
        AnyValue::StringOwned(s) => s.to_string(),
        other => {
            // ROOT-059修复：sheet_to_dataframe 里数字列统一构造为 f64 Series，
            // group_by 聚合后 AnyValue 变体是 Float64，Display 格式化整数值时
            // 会带上多余的 ".0" 后缀（如 "1.0"），与 col_tree/row_tree 节点
            // 从 xlsx sharedItems <n v="1"> 原样摘出的字符串 "1" 不一致，
            // 导致 PivotDataMatrix 按 (row_path, col_path) 查值时 key 对不上，
            // 渲染出的数据区域大片空白。这里对能提取为 f64 的整数值做特殊处理，
            // 去掉多余的小数点尾巴；非整数值（如 3.5）保留原始格式化结果不受影响。
            if let Ok(f) = other.try_extract::<f64>() {
                if f.fract() == 0.0 && f.is_finite() {
                    format!("{}", f as i64)
                } else {
                    format!("{}", other)
                }
            } else {
                format!("{}", other)
            }
        }
    }
}

// ════════════════════════════════════════════════════════════
// DataFrame -> Sheet
// ════════════════════════════════════════════════════════════

/// DataFrame -> 新 Sheet，第一行写入列名作为表头
pub fn dataframe_to_sheet(name: String, df: &DataFrame) -> Result<Sheet, BridgeError> {
    let height = df.height();
    let mut grid = SparseGrid::default();

    for (c, col) in df.columns().iter().enumerate() {
        let mut values: SmallVec<[CellValue; 8]> = smallvec![];
        values.push(CellValue::Text(col.name().to_string()));
        for i in 0..height {
            let av = col.get(i).unwrap_or(AnyValue::Null);
            values.push(anyvalue_to_cell(&av));
        }
        grid.cols.insert(
            c as u32,
            SparseColumn {
                blocks: smallvec![CellBlock { start_row: 0, values }],
            },
        );
    }

    Ok(Sheet {
        col_style: vec![],
        tables: vec![],
        name,
        cells: grid,
        col_widths: Vec::new(),
        row_heights: Vec::new(),
        frozen_rows: 0,
        frozen_cols: 0,
        date1904: false, // ROOT-041: 测试/桥接代码构造的Sheet，默认非1904日期系统
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
    })
}

// ════════════════════════════════════════════════════════════
// 透视表：Sheet -> Polars 透视 -> 新 Sheet 插入 Workbook
// ════════════════════════════════════════════════════════════

/// 把一条完整路径插入树（若路径已存在则复用节点，只在末端追加）
fn pivot_v2_insert_path(root: &mut PivotHeaderNode, path: &[String]) {
    if path.is_empty() {
        return;
    }
    let value = &path[0];
    let pos = root.children.iter().position(|c| &c.value == value);
    let child = if let Some(idx) = pos {
        &mut root.children[idx]
    } else {
        let node = PivotHeaderNode::new(value.clone());
        root.children.push(node);
        root.children.last_mut().unwrap()
    };
    pivot_v2_insert_path(child, &path[1..]);
}

/// v151新增：为row_tree的最外层分组节点追加小计节点（本版本范围：仅最外层，仅行方向）。
/// 小计节点 value = "{父节点值} Total"，与明细子节点作为兄弟节点并列插入，
/// 排在该分组的所有子节点之后（对应 subtotalTop="0"，固定显示在底部）。
/// 只处理"有子节点"的最外层节点——若某个分组下没有子节点（异常数据），跳过不加小计。
fn pivot_v2_insert_row_subtotals(row_tree: &mut PivotHeaderNode) {
    for outer_node in row_tree.children.iter_mut() {
        if outer_node.children.is_empty() {
            continue;
        }
        let subtotal_label = format!("{} Total", outer_node.value);
        let subtotal_node = PivotHeaderNode {
            value: subtotal_label,
            is_subtotal: true,
            ..Default::default()
        };
        outer_node.children.push(subtotal_node);
    }
}

/// v152：列方向小计（阶段2），对称于 pivot_v2_insert_row_subtotals。
/// 仅处理 col_tree 最外层：为每个有子节点的外层列分组，在其 children 末尾
/// 追加一个小计节点（value = "{父节点值} Total"，is_subtotal = true）。
/// 没有子节点的分组（异常数据）跳过不加。约定与行方向完全一致，故直接
/// 复用同一套 PivotHeaderNode 结构，无需额外字段区分行/列小计。
fn pivot_v2_insert_col_subtotals(col_tree: &mut PivotHeaderNode) {
    for outer_node in col_tree.children.iter_mut() {
        if outer_node.children.is_empty() {
            continue;
        }
        let subtotal_label = format!("{} Total", outer_node.value);
        let subtotal_node = PivotHeaderNode {
            value: subtotal_label,
            is_subtotal: true,
            ..Default::default()
        };
        outer_node.children.push(subtotal_node);
    }
}

/// 获取 DataFrame 某列的去重取值（字符串化，排序）
fn pivot_v2_unique_sorted(df: &DataFrame, field: &str) -> Result<Vec<String>, BridgeError> {
    let series = df.column(field)?.as_materialized_series();
    let unique = series.unique()?;
    let mut vals: Vec<String> = Vec::with_capacity(unique.len());
    for i in 0..unique.len() {
        vals.push(anyvalue_to_display_string(&unique.get(i)?));
    }
    vals.sort();
    Ok(vals)
}

/// 笛卡尔积展开多个字段的取值组合，生成所有叶子路径（外→内顺序）
#[allow(dead_code)]
fn pivot_v2_cartesian_paths(df: &DataFrame, field_names: &[String]) -> Result<Vec<Vec<String>>, BridgeError> {
    pivot_v2_cartesian_paths_sorted(df, field_names, &[])
}

/// 带排序版本:sort 中该字段标记为 false(降序)时,反转其取值顺序;缺省=升序。
fn pivot_v2_cartesian_paths_sorted(
    df: &DataFrame,
    field_names: &[String],
    sort: &[(String, bool)],
) -> Result<Vec<Vec<String>>, BridgeError> {
    let mut paths: Vec<Vec<String>> = vec![vec![]];
    for fname in field_names {
        let mut values = pivot_v2_unique_sorted(df, fname)?;
        if sort.iter().any(|(n, asc)| n == fname && !*asc) {
            values.reverse();
        }
        let mut new_paths: Vec<Vec<String>> = Vec::with_capacity(paths.len() * values.len().max(1));
        for p in &paths {
            for v in &values {
                let mut np = p.clone();
                np.push(v.clone());
                new_paths.push(np);
            }
        }
        paths = new_paths;
    }
    Ok(paths)
}

/// 从原始明细数据现场构建完整的树状 PivotMeta，并作为新 Sheet 追加到 workbook。
/// 与 pivot_sheet_into_workbook（旧链路，压平输出）并存，不影响旧链路。
pub fn pivot_sheet_into_workbook_v2(
    workbook: &mut Workbook,
    sheet_idx: usize,
    cfg: &PivotConfig,
    result_sheet_name: String,
) -> Result<usize, BridgeError> {
    pivot_sheet_into_workbook_v2_ex(workbook, sheet_idx, cfg, result_sheet_name, &[])
}

/// v2 的带排序版本。sort: (字段名, 是否升序),仅对行/列字段生效,缺省=升序。
pub fn pivot_sheet_into_workbook_v2_ex(
    workbook: &mut Workbook,
    sheet_idx: usize,
    cfg: &PivotConfig,
    result_sheet_name: String,
    sort: &[(String, bool)],
) -> Result<usize, BridgeError> {
    let sheet = workbook
        .sheets
        .get(sheet_idx)
        .ok_or_else(|| BridgeError::Other(format!("sheet index {sheet_idx} out of range")))?;
    let df = sheet_to_dataframe(sheet)?;

    let df = if cfg.filters.is_empty() {
        df
    } else {
        let mut mask: Option<BooleanChunked> = None;
        for (field, allowed_vals) in &cfg.filters {
            if allowed_vals.is_empty() { continue; } // 空列表=该字段仅作筛选字段,不限制行
            let series = df.column(field)?.as_materialized_series();
            let col_mask: BooleanChunked = (0..series.len())
                .map(|i| {
                    series.get(i).ok().map(|av| {
                        allowed_vals.iter().any(|v| v == &anyvalue_to_display_string(&av))
                    }).unwrap_or(false)
                })
                .collect();
            mask = Some(match mask {
                Some(m) => &m & &col_mask,
                None => col_mask,
            });
        }
        match mask {
            Some(m) => df.filter(&m)?,
            None => df,
        }
    };

    let row_field_names = cfg.rows.clone();
    let col_field_names = cfg.cols.clone();
    let value_fields: Vec<PivotValueFieldSpec> = cfg.values.iter()
        .map(|name| PivotValueFieldSpec { source_column: name.clone(), agg: cfg.agg.clone() })
        .collect();
    let n_values = value_fields.len().max(1);

    let mut row_tree = PivotHeaderNode::new("");
    for path in pivot_v2_cartesian_paths_sorted(&df, &row_field_names, sort)? {
        pivot_v2_insert_path(&mut row_tree, &path);
    }
    // col_tree 只包含 col_field_names 对应的字段值层级，不手动插入值字段层节点，
    // 与旧的 xlsx 解析路径（roe-parser::ooxml::pivot::parse_pivot_table）保持一致。
    // 多值字段的展开完全交给 PivotMeta::compute_spans() 在渲染前统一处理
    // （它会对叶子节点乘以 n_values），因为 tile.rs 的渲染逻辑固定假设
    // col_tree 存的是"未展开值字段层"的树，自己在渲染时用 n_values 重新展开坐标。
    let mut col_tree = PivotHeaderNode::new("");
    let col_leaf_paths = pivot_v2_cartesian_paths_sorted(&df, &col_field_names, sort)?;
    for path in &col_leaf_paths {
        pivot_v2_insert_path(&mut col_tree, path);
    }

    // v151：仅行方向、仅最外层的小计节点插入（见 pivot_v2_insert_row_subtotals 注释）
    pivot_v2_insert_row_subtotals(&mut row_tree);
    // v152：列方向、仅最外层的小计节点插入（见 pivot_v2_insert_col_subtotals 注释）
    pivot_v2_insert_col_subtotals(&mut col_tree);

    let data = compute_pivot_data_matrix(&df, &row_field_names, &col_field_names, &value_fields)?;

    let filter_fields: Vec<PivotFieldDef> = cfg.filters.keys()
        .map(|k| PivotFieldDef::new(k.clone()))
        .collect();
    let filter_row_count = filter_fields.len() as u32;
    let col_header_rows = col_field_names.len() as u32
        + if n_values > 1 { 1 } else { 0 };
    let data_row_start = filter_row_count + 1 + col_header_rows;
    let data_col_start = (row_field_names.len() as u32).max(1);

    let agg_suffix = match cfg.agg.as_str() {
        "sum" => "Sum",
        "mean" | "avg" | "average" => "Average",
        "count" => "Count",
        "min" => "Min",
        "max" => "Max",
        other => other,
    };
    // 归一成 ROE 内部聚合命名,供 xlsx 写出(subtotal_attr)使用
    let agg_norm: String = match cfg.agg.as_str() {
        "mean" | "avg" | "average" => "mean".to_string(),
        other => other.to_string(),
    };
    let value_field_defs: Vec<PivotFieldDef> = cfg.values.iter()
        .map(|name| {
            let mut def = PivotFieldDef::new(format!("{}的{}", name, agg_suffix));
            def.source_field = Some(name.clone());
            def.agg = Some(agg_norm.clone());
            def
        })
        .collect();

    let mut meta = PivotMeta {
        source_sheet: sheet.name.clone(),
        agg: cfg.agg.clone(),
        filter_fields,
        row_field_names,
        col_field_names,
        value_fields: value_field_defs,
        base_row: 2, // 固定空2行，不再随filter_row_count变化
        base_col: 0,
        last_row: 0,
        last_col: 0,
        col_tree,
        row_tree,
        data_row_start,
        data_col_start,
        row_grand_totals: true,
        col_grand_totals: true,
        data,
        ..Default::default()
    };
    meta.compute_spans();

    let row_leaf_span = meta.row_tree.cached_span.max(1);
    let col_leaf_span = meta.col_tree.cached_span.max(1);
    meta.last_row = meta.data_row_start + row_leaf_span - 1;
    meta.last_col = meta.data_col_start + col_leaf_span - 1;

    let mut result_sheet = Sheet::default();
    result_sheet.name = result_sheet_name;
    result_sheet.pivot = Some(meta);
    result_sheet.default_col_width = 48.99;
    result_sheet.default_row_height = 14.25;
    // 物化:把透视结果写进 cells(公式栏/get_cell/AI 可读);渲染由 cells_rect 遮罩避免叠字
    roe_model::pivot_layout::materialize_pivot(&mut result_sheet);

    workbook.sheets.push(result_sheet);
    Ok(workbook.sheets.len() - 1)
}

/// 就地刷新透视表:按 sheet.pivot 里的配置(行/列/值/field_filters/sort)重算,
/// 保留 base_row/base_col/source_ref/name/style_name/layout_mode/页字段顺序,重新物化。
/// 失败时不修改目标 sheet。
pub fn refresh_pivot_in_place(workbook: &mut Workbook, sheet_idx: usize) -> Result<(), BridgeError> {
    let old = workbook
        .sheets
        .get(sheet_idx)
        .and_then(|s| s.pivot.clone())
        .ok_or_else(|| BridgeError::Other(format!("sheet {sheet_idx} 不是透视表")))?;

    let source_idx = workbook
        .sheets
        .iter()
        .position(|s| s.name == old.source_sheet)
        .ok_or_else(|| BridgeError::Other(format!("数据源 sheet 未找到: {}", old.source_sheet)))?;

    let mut values: Vec<String> = Vec::with_capacity(old.value_fields.len());
    for f in &old.value_fields {
        match &f.source_field {
            Some(sf) => values.push(sf.clone()),
            None => {
                return Err(BridgeError::Other(format!(
                    "值字段 {} 缺少 source_field,无法刷新",
                    f.name
                )))
            }
        }
    }

    // 页字段无条目时给空列表(=只作筛选字段,不限制行);再叠加 field_filters
    let mut filters: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for f in &old.filter_fields {
        filters.insert(f.name.clone(), Vec::new());
    }
    for (k, v) in &old.field_filters {
        filters.insert(k.clone(), v.clone());
    }

    let cfg = PivotConfig {
        rows: old.row_field_names.clone(),
        cols: old.col_field_names.clone(),
        values,
        agg: old.agg.clone(),
        filters,
    };

    let tmp_idx = pivot_sheet_into_workbook_v2_ex(
        workbook,
        source_idx,
        &cfg,
        "__pivot_refresh_tmp__".to_string(),
        &old.sort,
    )?;
    let taken = workbook.sheets[tmp_idx].pivot.take();
    workbook.sheets.remove(tmp_idx);
    let mut new_meta = taken.ok_or_else(|| BridgeError::Other("刷新失败:临时 sheet 无 pivot".into()))?;

    // 回填需要保留的字段
    new_meta.base_row = old.base_row;
    new_meta.base_col = old.base_col;
    new_meta.source_ref = old.source_ref.clone();
    new_meta.name = old.name.clone();
    new_meta.style_name = old.style_name.clone();
    new_meta.layout_mode = old.layout_mode.clone();
    new_meta.field_filters = old.field_filters.clone();
    new_meta.sort = old.sort.clone();
    // materialize 靠 cells_rect 清旧区域,必须换回旧的
    new_meta.cells_rect = old.cells_rect;

    // 页字段:按旧顺序重建,selected_values 与 field_filters 同步
    new_meta.filter_fields = old
        .filter_fields
        .iter()
        .map(|f| {
            let mut d = f.clone();
            d.selected_values = new_meta.field_filters.get(&f.name).cloned().unwrap_or_default();
            d
        })
        .collect();

    // v2 按 cfg.filters.len() 算 data_row_start,这里按真实页字段个数重算
    let n_values = new_meta.value_fields.len().max(1);
    let col_header_rows = new_meta.col_field_names.len() as u32 + if n_values > 1 { 1 } else { 0 };
    new_meta.data_row_start = new_meta.filter_fields.len() as u32 + 1 + col_header_rows;
    let row_leaf_span = new_meta.row_tree.cached_span.max(1);
    new_meta.last_row = new_meta.data_row_start + row_leaf_span - 1;

    let target = workbook
        .sheets
        .get_mut(sheet_idx)
        .ok_or_else(|| BridgeError::Other(format!("sheet index {sheet_idx} out of range")))?;
    target.pivot = Some(new_meta);
    roe_model::pivot_layout::materialize_pivot(target);
    Ok(())
}

/// 就地整体设置透视表的行/列/值/页字段并刷新。
/// 字段必须存在于数据源列名且不得跨区域重复;不再使用的字段,其 field_filters/sort 一并清除。
/// 保留已有值字段/页字段定义与 base_row/name/style 等;失败时恢复旧 meta。
pub fn set_pivot_config_in_place(
    workbook: &mut Workbook,
    sheet_idx: usize,
    rows: Vec<String>,
    cols: Vec<String>,
    values: Vec<String>,
    filters: Vec<String>,
) -> Result<(), BridgeError> {
    let old = workbook
        .sheets
        .get(sheet_idx)
        .and_then(|s| s.pivot.clone())
        .ok_or_else(|| BridgeError::Other(format!("sheet {sheet_idx} 不是透视表")))?;
    let source_idx = workbook
        .sheets
        .iter()
        .position(|s| s.name == old.source_sheet)
        .ok_or_else(|| BridgeError::Other(format!("数据源 sheet 未找到: {}", old.source_sheet)))?;
    let df = sheet_to_dataframe(&workbook.sheets[source_idx])?;
    let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for f in rows.iter().chain(cols.iter()).chain(values.iter()).chain(filters.iter()) {
        if !names.contains(f) {
            return Err(BridgeError::Other(format!("字段 {f} 不在数据源列中")));
        }
        if !seen.insert(f.clone()) {
            return Err(BridgeError::Other(format!("字段 {f} 重复出现在多个区域")));
        }
    }

    let agg_norm: String = match old.agg.as_str() {
        "" => "sum".to_string(),
        "mean" | "avg" | "average" => "mean".to_string(),
        other => other.to_string(),
    };
    let mut m = old.clone();
    m.agg = agg_norm.clone();
    m.value_fields = values
        .iter()
        .map(|v| {
            old.value_fields
                .iter()
                .find(|d| d.source_field.as_deref() == Some(v.as_str()))
                .cloned()
                .unwrap_or_else(|| {
                    let mut d = PivotFieldDef::new(v.clone());
                    d.source_field = Some(v.clone());
                    d.agg = Some(agg_norm.clone());
                    d
                })
        })
        .collect();
    m.filter_fields = filters
        .iter()
        .map(|f| {
            old.filter_fields
                .iter()
                .find(|d| &d.name == f)
                .cloned()
                .unwrap_or_else(|| PivotFieldDef::new(f.clone()))
        })
        .collect();
    let used: std::collections::HashSet<&String> =
        rows.iter().chain(cols.iter()).chain(filters.iter()).collect();
    m.field_filters.retain(|k, _| used.contains(k));
    m.sort.retain(|(n, _)| rows.contains(n) || cols.contains(n));
    m.row_field_names = rows;
    m.col_field_names = cols;

    workbook.sheets[sheet_idx].pivot = Some(m);
    if let Err(e) = refresh_pivot_in_place(workbook, sheet_idx) {
        workbook.sheets[sheet_idx].pivot = Some(old);
        return Err(e);
    }
    Ok(())
}

/// 返回透视表当前配置(只读):
/// {"rows":[..],"cols":[..],"values":[..],"filters":[..],"source_idx":N,"fields":[数据源全部列名]}
/// values 为值字段的真实源列名;数据源 sheet 找不到时 source_idx=-1、fields 为空。
pub fn pivot_config_value(workbook: &Workbook, sheet_idx: usize) -> Result<serde_json::Value, BridgeError> {
    let sheet = workbook
        .sheets
        .get(sheet_idx)
        .ok_or_else(|| BridgeError::Other(format!("sheet_idx={sheet_idx} 越界")))?;
    let pivot = sheet
        .pivot
        .as_ref()
        .ok_or_else(|| BridgeError::Other(format!("sheet_idx={sheet_idx} 无透视表元数据")))?;
    let source_idx = workbook.sheets.iter().position(|s| s.name == pivot.source_sheet);
    let fields: Vec<String> = match source_idx {
        Some(i) => {
            let df = sheet_to_dataframe(&workbook.sheets[i])?;
            let names: Vec<String> = df.get_column_names().iter().map(|n| n.to_string()).collect();
            names
        }
        None => Vec::new(),
    };
    let values: Vec<String> = pivot
        .value_fields
        .iter()
        .map(|f| f.source_field.clone().unwrap_or_else(|| f.name.clone()))
        .collect();
    let filters: Vec<String> = pivot.filter_fields.iter().map(|f| f.name.clone()).collect();
    Ok(serde_json::json!({
        "rows": pivot.row_field_names,
        "cols": pivot.col_field_names,
        "values": values,
        "filters": filters,
        "source_idx": source_idx.map(|i| i as i64).unwrap_or(-1),
        "fields": fields,
    }))
}

/// 就地设置透视表某字段的筛选值并刷新。values 为空=清除筛选。
/// 字段须为页/行/列字段。失败时 meta 回滚。
pub fn pivot_set_filter_in_place(
    workbook: &mut Workbook,
    sheet_idx: usize,
    field: &str,
    values: Vec<String>,
) -> Result<(), BridgeError> {
    let sheet = workbook
        .sheets
        .get_mut(sheet_idx)
        .ok_or_else(|| BridgeError::Other(format!("sheet_idx={sheet_idx} 越界")))?;
    let meta = sheet
        .pivot
        .as_mut()
        .ok_or_else(|| BridgeError::Other(format!("sheet_idx={sheet_idx} 无透视表元数据")))?;
    let known = meta.filter_fields.iter().any(|f| f.name == field)
        || meta.row_field_names.iter().any(|n| n == field)
        || meta.col_field_names.iter().any(|n| n == field);
    if !known {
        return Err(BridgeError::Other(format!("字段 {field} 不是透视表的页/行/列字段")));
    }
    let old = meta.clone();
    if values.is_empty() {
        meta.field_filters.remove(field);
    } else {
        meta.field_filters.insert(field.to_string(), values);
    }
    if let Err(e) = refresh_pivot_in_place(workbook, sheet_idx) {
        workbook.sheets[sheet_idx].pivot = Some(old);
        return Err(e);
    }
    Ok(())
}

/// 就地设置透视表某行/列字段的排序并刷新。asc=true 升序(默认,删除排序条目),false 降序。
/// 字段须为行/列字段(页字段会被拒绝)。失败时 meta 回滚。
pub fn pivot_set_sort_in_place(
    workbook: &mut Workbook,
    sheet_idx: usize,
    field: &str,
    asc: bool,
) -> Result<(), BridgeError> {
    let sheet = workbook
        .sheets
        .get_mut(sheet_idx)
        .ok_or_else(|| BridgeError::Other(format!("sheet_idx={sheet_idx} 越界")))?;
    let meta = sheet
        .pivot
        .as_mut()
        .ok_or_else(|| BridgeError::Other(format!("sheet_idx={sheet_idx} 无透视表元数据")))?;
    let known = meta.row_field_names.iter().any(|n| n == field)
        || meta.col_field_names.iter().any(|n| n == field);
    if !known {
        return Err(BridgeError::Other(format!("字段 {field} 不是透视表的行/列字段")));
    }
    let old = meta.clone();
    meta.sort.retain(|(n, _)| n.as_str() != field);
    if !asc {
        meta.sort.push((field.to_string(), false));
    }
    if let Err(e) = refresh_pivot_in_place(workbook, sheet_idx) {
        workbook.sheets[sheet_idx].pivot = Some(old);
        return Err(e);
    }
    Ok(())
}

// ════════════════════════════════════════════════════════════
// SQL 查询：Sheet -> Polars SQL -> 新 Sheet 插入 Workbook
// ════════════════════════════════════════════════════════════

/// 对 workbook.sheets[sheet_idx] 注册为 table_name 表，执行 SQL（支持 polars 0.53
/// 窗口函数 / CTE 等高级查询），结果作为名为 result_sheet_name 的新 Sheet 追加到
/// workbook，返回新 Sheet 索引。
pub fn sql_sheet_into_workbook(
    workbook: &mut Workbook,
    sheet_idx: usize,
    table_name: &str,
    sql: &str,
    result_sheet_name: String,
) -> Result<usize, BridgeError> {
    let sheet = workbook
        .sheets
        .get(sheet_idx)
        .ok_or_else(|| BridgeError::Other(format!("sheet index {sheet_idx} out of range")))?;

    let df = sheet_to_dataframe(sheet)?;
    let result_df = run_sql_on_dataframe(df, table_name, sql)?;
    let result_sheet = dataframe_to_sheet(result_sheet_name, &result_df)?;

    workbook.sheets.push(result_sheet);
    Ok(workbook.sheets.len() - 1)
}

/// 对任意 DataFrame 执行 SQL 查询，返回结果 DataFrame
pub fn run_sql_on_dataframe(df: DataFrame, table_name: &str, sql: &str) -> Result<DataFrame, BridgeError> {
    use polars::sql::SQLContext;
    let mut ctx = SQLContext::new();
    ctx.register(table_name, df.lazy());
    let result = ctx
        .execute(sql)
        .map_err(|e| BridgeError::Other(format!("SQL 执行失败: {e}")))?
        .collect()?;
    Ok(result)
}

/// 多表 SQL:把若干数据集注册为表,执行 SQL,返回结果 DataFrame(可 JOIN)。
/// DataFrame 的 clone 只复制 Arc 列引用,不复制数据;以 LazyFrame 注册,过滤/投影可下推。
pub fn run_sql_on_datasets(tables: Vec<(String, DataFrame)>, sql: &str) -> Result<DataFrame, BridgeError> {
    run_sql_on_lazy_datasets(tables.into_iter().map(|(n, d)| (n, d.lazy())).collect(), sql)
}

/// 多表 SQL(惰性版):数据集以 LazyFrame 直接注册,大表不会被预先物化。
pub fn run_sql_on_lazy_datasets(tables: Vec<(String, LazyFrame)>, sql: &str) -> Result<DataFrame, BridgeError> {
    use polars::sql::SQLContext;
    let mut ctx = SQLContext::new();
    for (name, lf) in tables {
        ctx.register(&name, lf);
    }
    let result = ctx
        .execute(sql)
        .map_err(|e| BridgeError::Other(format!("SQL 执行失败: {e}")))?
        .collect()?;
    Ok(result)
}

// ════════════════════════════════════════════════════════════
// 大文件旁路：calamine 直读 xlsx/xls/ods -> DataFrame（不进入内存模型）
// ════════════════════════════════════════════════════════════

/// 直接从外部 xlsx/xls/ods 文件读取第一个 sheet 为 DataFrame，
/// 用于大体积文件的分析 / SQL 查询，绕过 ROE 内存模型，不参与持久化。
pub fn df_from_xlsx_file(path: &str) -> Result<DataFrame, BridgeError> {
    pivot_engine::xlsx_io::df_from_xlsx(path).map_err(BridgeError::Other)
}

// ════════════════════════════════════════════════════════════
// CSV 导入(百万~千万行):直接进 DataFrame,不经过 Sheet 格子
// ════════════════════════════════════════════════════════════

/// 判断编码:BOM 优先;文件头是合法 UTF-8(允许末尾截断)按 UTF-8;否则按 GBK。
fn sniff_csv_encoding(head: &[u8]) -> &'static encoding_rs::Encoding {
    if let Some((enc, _)) = encoding_rs::Encoding::for_bom(head) {
        return enc;
    }
    match std::str::from_utf8(head) {
        Ok(_) => encoding_rs::UTF_8,
        Err(e) if e.error_len().is_none() => encoding_rs::UTF_8,
        Err(_) => encoding_rs::GBK,
    }
}

/// 按首行统计候选分隔符出现次数,取最多者;都没有则逗号。
fn detect_csv_separator(head: &[u8]) -> u8 {
    let end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    let line = &head[..end];
    let mut best = (b',', 0usize);
    for &c in &[b',', b'\t', b';', b'|'] {
        let n = line.iter().filter(|&&b| b == c).count();
        if n > best.1 {
            best = (c, n);
        }
    }
    best.0
}

/// 流式转码到临时 UTF-8 文件,返回临时文件路径(调用方负责删除)。
fn transcode_csv_to_utf8_tmp(path: &str, enc: &'static encoding_rs::Encoding) -> Result<String, BridgeError> {
    use std::io::{Read, Write};
    let io = |e: std::io::Error| BridgeError::Other(format!("CSV 转码失败: {e}"));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("roe_csv_{}_{}.csv", std::process::id(), nanos));
    let mut inp = std::fs::File::open(path).map_err(io)?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp).map_err(io)?);
    let mut dec = enc.new_decoder_with_bom_removal();
    let mut buf: Vec<u8> = std::iter::repeat(0u8).take(1 << 20).collect();
    let mut s = String::new();
    loop {
        let n = inp.read(&mut buf).map_err(io)?;
        let last = n == 0;
        s.clear();
        s.reserve(dec.max_utf8_buffer_length(n).unwrap_or(n * 3 + 16));
        let _ = dec.decode_to_string(&buf[..n], &mut s, last);
        out.write_all(s.as_bytes()).map_err(io)?;
        if last {
            break;
        }
    }
    out.flush().map_err(io)?;
    Ok(tmp.to_string_lossy().to_string())
}

/// 读取一个 UTF-8 编码的 CSV 文件为 DataFrame。
fn read_utf8_csv(path: &str, delimiter: Option<char>, has_header: bool) -> Result<DataFrame, BridgeError> {
    use polars::prelude::SerReader as _;
    use std::io::Read;
    let io = |e: std::io::Error| BridgeError::Other(format!("读取 CSV 失败: {e}"));
    let sep = match delimiter {
        Some(c) if c.is_ascii() => c as u8,
        _ => {
            let mut head = Vec::new();
            std::fs::File::open(path).map_err(io)?.take(8192).read_to_end(&mut head).map_err(io)?;
            detect_csv_separator(&head)
        }
    };
    let file = std::fs::File::open(path).map_err(io)?;
    let df = polars::prelude::CsvReadOptions::default()
        .with_has_header(has_header)
        .with_infer_schema_length(Some(100_000))
        .with_parse_options(
            polars::prelude::CsvParseOptions::default()
                .with_separator(sep)
                .with_truncate_ragged_lines(true),
        )
        .into_reader_with_file_handle(file)
        .finish()?;
    Ok(df)
}

/// 从 CSV 文件读取 DataFrame。delimiter=None 时自动探测;编码自动识别(UTF-8/BOM/UTF-16/GBK)。
pub fn df_from_csv_file(path: &str, delimiter: Option<char>, has_header: bool) -> Result<DataFrame, BridgeError> {
    use std::io::Read;
    let io = |e: std::io::Error| BridgeError::Other(format!("读取 CSV 失败: {e}"));
    let mut head = Vec::new();
    std::fs::File::open(path).map_err(io)?.take(65536).read_to_end(&mut head).map_err(io)?;
    let has_bom = encoding_rs::Encoding::for_bom(&head).is_some();
    let enc = sniff_csv_encoding(&head);

    if enc == encoding_rs::UTF_8 {
        match read_utf8_csv(path, delimiter, has_header) {
            Ok(df) => return Ok(df),
            Err(e) => {
                // 文件头全是 ASCII、后面才出现 GBK 的情况:直读失败时按 GBK 重试一次
                if has_bom {
                    return Err(e);
                }
            }
        }
        return transcode_then_read(path, encoding_rs::GBK, delimiter, has_header);
    }
    transcode_then_read(path, enc, delimiter, has_header)
}

fn transcode_then_read(
    path: &str,
    enc: &'static encoding_rs::Encoding,
    delimiter: Option<char>,
    has_header: bool,
) -> Result<DataFrame, BridgeError> {
    let tmp = transcode_csv_to_utf8_tmp(path, enc)?;
    let r = read_utf8_csv(&tmp, delimiter, has_header);
    let _ = std::fs::remove_file(&tmp);
    r
}

// ════════════════════════════════════════════════════════════
// CSV → IPC 缓存:导入一次,之后只 scan IPC(列式、可按需取行)
// ════════════════════════════════════════════════════════════

/// 缓存格式版本;改变写入参数/格式时递增,使旧缓存自然失效。
const CSV_CACHE_VERSION: u32 = 1;

/// FNV-1a 64 位:跨版本、跨平台稳定(DefaultHasher 不保证稳定)。
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 计算缓存文件路径。键 = 规范化路径 + mtime(ns) + size + 分隔符 + 表头 + 版本。
/// 源文件被修改(mtime 或 size 变)后键变化,旧缓存不再命中(清理策略另行处理)。
pub fn csv_cache_path(
    path: &str,
    delimiter: Option<char>,
    has_header: bool,
    cache_dir: &std::path::Path,
) -> Result<std::path::PathBuf, BridgeError> {
    let io = |e: std::io::Error| BridgeError::Other(format!("读取 CSV 元数据失败: {e}"));
    let meta = std::fs::metadata(path).map_err(io)?;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path));
    let key = format!(
        "{}|{}|{}|{:?}|{}|v{}",
        canon.to_string_lossy(),
        mtime_ns,
        meta.len(),
        delimiter,
        has_header,
        CSV_CACHE_VERSION
    );
    Ok(cache_dir.join(format!("{:016x}.arrow", fnv1a64(&key))))
}

/// 把 CSV 导入为 IPC 缓存文件,返回缓存文件路径。命中缓存则不重新解析。
/// 当前实现先经 df_from_csv_file 物化再落盘(导入峰值内存与旧路径相同),
/// 之后的使用方可 scan_ipc_cache 得到惰性引用;流式导入留待后续。
pub fn csv_to_ipc_cache(
    path: &str,
    delimiter: Option<char>,
    has_header: bool,
    cache_dir: &std::path::Path,
) -> Result<String, BridgeError> {
    use polars::prelude::{IpcWriter, SerWriter};
    let io = |e: std::io::Error| BridgeError::Other(format!("写 IPC 缓存失败: {e}"));
    std::fs::create_dir_all(cache_dir).map_err(io)?;
    let target = csv_cache_path(path, delimiter, has_header, cache_dir)?;
    if target.exists() {
        return Ok(target.to_string_lossy().to_string());
    }
    let mut df = df_from_csv_file(path, delimiter, has_header)?;
    let tmp = target.with_extension(format!("tmp{}", std::process::id()));
    {
        let file = std::fs::File::create(&tmp).map_err(io)?;
        let mut w = std::io::BufWriter::new(file);
        IpcWriter::new(&mut w).finish(&mut df)?;
        std::io::Write::flush(&mut w).map_err(io)?;
    }
    std::fs::rename(&tmp, &target).map_err(io)?;
    Ok(target.to_string_lossy().to_string())
}

/// 数据集概览(类似 df.info() + df.head()):行数、列数、每列真实类型/空值数
/// (数值列另含 min/max/mean)、前 head_n 行。行数/空值/统计量合并为一次惰性聚合。
/// 返回 {"rows","cols","columns":[{"name","dtype","nulls","min"?,"max"?,"mean"?}],"head":{"columns","rows","offset"?}}
pub fn lazy_overview(lf: LazyFrame, head_n: u32) -> Result<String, BridgeError> {
    let mut schema_lf = lf.clone();
    let schema = schema_lf.collect_schema().map_err(BridgeError::Polars)?;
    let fields: Vec<(String, DataType)> =
        schema.iter().map(|(n, d)| (n.to_string(), d.clone())).collect();
    let is_num = |d: &DataType| {
        matches!(
            d,
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64
                | DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64
                | DataType::Float32 | DataType::Float64
        )
    };
    let mut exprs = vec![polars::prelude::len().alias("__rows")];
    for (i, (name, dt)) in fields.iter().enumerate() {
        exprs.push(col(name.as_str()).null_count().alias(format!("n{i}")));
        if is_num(dt) {
            exprs.push(col(name.as_str()).min().alias(format!("min{i}")));
            exprs.push(col(name.as_str()).max().alias(format!("max{i}")));
            exprs.push(col(name.as_str()).mean().alias(format!("mean{i}")));
        }
    }
    let agg = lf.clone().select(exprs).collect()?;
    let get = |k: &str| -> serde_json::Value {
        agg.column(k)
            .ok()
            .and_then(|c| c.get(0).ok())
            .map(|av| anyvalue_to_json(&av))
            .unwrap_or(serde_json::Value::Null)
    };
    let rows = get("__rows").as_u64().unwrap_or(0);
    let mut columns = Vec::with_capacity(fields.len());
    for (i, (name, dt)) in fields.iter().enumerate() {
        let mut o = serde_json::json!({
            "name": name,
            "dtype": format!("{dt}"),
            "nulls": get(&format!("n{i}")),
        });
        if is_num(dt) {
            o["min"] = get(&format!("min{i}"));
            o["max"] = get(&format!("max{i}"));
            o["mean"] = get(&format!("mean{i}"));
        }
        columns.push(o);
    }
    let page = lf.slice(0, head_n).collect()?;
    let head_json = df_preview(&page, head_n as usize)?;
    let mut head: serde_json::Value = serde_json::from_str(&head_json)
        .map_err(|e| BridgeError::Other(format!("概览 JSON 解析失败: {e}")))?;
    if let Some(o) = head.as_object_mut() { o.remove("total_rows"); }
    Ok(serde_json::json!({
        "rows": rows,
        "cols": fields.len(),
        "columns": columns,
        "head": head,
    }).to_string())
}

/// 惰性计算行数(select len() 聚合,不物化全表)。
pub fn lazy_row_count(lf: LazyFrame) -> Result<u64, BridgeError> {
    use polars::prelude::len;
    let df = lf.select([len().alias("n")]).collect()?;
    let v = df.column("n")?.get(0)?;
    Ok(v.extract::<u64>().unwrap_or(0))
}

/// 文件加载结果:CSV/Parquet 为惰性引用,xlsx 只能物化。
#[derive(Clone)]
pub enum LoadedSource {
    Lazy(LazyFrame),
    Mem(DataFrame),
}

impl LoadedSource {
    pub fn into_lazy(self) -> LazyFrame {
        match self {
            LoadedSource::Lazy(lf) => lf,
            LoadedSource::Mem(df) => df.lazy(),
        }
    }
}

/// 按类型打开外部文件。kind: "csv"(经 IPC 缓存后惰性 scan) | "parquet"(文件或目录) | "xlsx"(物化,第一个 sheet)。
pub fn load_file_source(
    path: &str,
    kind: &str,
    delimiter: Option<char>,
    has_header: bool,
    cache_dir: &std::path::Path,
) -> Result<LoadedSource, BridgeError> {
    match kind {
        "csv" => {
            let ipc = csv_to_ipc_cache(path, delimiter, has_header, cache_dir)?;
            Ok(LoadedSource::Lazy(scan_ipc_cache(&ipc)?))
        }
        "parquet" => Ok(LoadedSource::Lazy(scan_parquet_dir(path)?)),
        "xlsx" => Ok(LoadedSource::Mem(df_from_xlsx_file(path)?)),
        other => Err(BridgeError::Other(format!("不支持的文件类型: {other}"))),
    }
}

/// 以惰性方式打开 IPC 缓存文件。
pub fn scan_ipc_cache(ipc_path: &str) -> Result<LazyFrame, BridgeError> {
    use polars::prelude::{IpcScanOptions, PlRefPath, UnifiedScanArgs};
    let lf = LazyFrame::scan_ipc(
        PlRefPath::new(ipc_path),
        IpcScanOptions::default(),
        UnifiedScanArgs::default(),
    )?;
    Ok(lf)
}

/// 惰性扫描单个 parquet 文件(如 Hive warehouse 里的数据文件)。
/// 构造时即解析 schema(只读 parquet footer 元数据,不读数据页,不物化),
/// 使文件不存在/损坏等错误当场暴露,而不是拖到 collect 时才报。
pub fn scan_parquet_file(path: &str) -> Result<LazyFrame, BridgeError> {
    use polars::prelude::{PlRefPath, ScanArgsParquet};
    let mut lf = LazyFrame::scan_parquet(PlRefPath::new(path), ScanArgsParquet::default())?;
    lf.collect_schema()?;
    Ok(lf)
}

/// 惰性扫描 parquet 文件或目录(Hive 表目录)。
/// - 接受 `file:` / `file://` 前缀(metastore 的 LOCATION 形态);其它 scheme(hdfs:// 等)报错。
/// - 目录:只取直接子文件,跳过 `.`/`_` 开头(.crc、_SUCCESS),按文件名排序,逐个 scan 后 concat。
///   不递归子目录(分区表暂不支持)。
pub fn scan_parquet_dir(path: &str) -> Result<LazyFrame, BridgeError> {
    use polars::prelude::{concat, UnionArgs};
    let p = path.strip_prefix("file://").or_else(|| path.strip_prefix("file:")).unwrap_or(path);
    if p.contains("://") {
        return Err(BridgeError::Other(format!("暂不支持非本地路径: {p}")));
    }
    let meta = std::fs::metadata(p).map_err(|e| BridgeError::Other(format!("{p}: {e}")))?;
    if meta.is_file() {
        return scan_parquet_file(p);
    }
    let mut files: Vec<String> = Vec::new();
    for ent in std::fs::read_dir(p).map_err(|e| BridgeError::Other(format!("{p}: {e}")))? {
        let ent = ent.map_err(|e| BridgeError::Other(e.to_string()))?;
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.starts_with('_') { continue; }
        if ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            files.push(ent.path().to_string_lossy().to_string());
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(BridgeError::Other(format!("目录下没有数据文件: {p}")));
    }
    let lfs = files.iter().map(|f| scan_parquet_file(f)).collect::<Result<Vec<_>, _>>()?;
    Ok(concat(lfs, UnionArgs::default())?)
}

#[cfg(test)]
mod csv_cache_tests {
    use super::*;

    fn tmp_path(tag: &str, ext: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("roe_cachetest_{}_{}.{}", std::process::id(), tag, ext))
    }

    fn cache_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("roe_cachedir_{}_{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn utf8_csv_roundtrip_via_ipc_cache() {
        let src = tmp_path("u8", "csv");
        std::fs::write(&src, "id,name\n1,alice\n2,bob\n").unwrap();
        let dir = cache_dir("u8");
        let ipc = csv_to_ipc_cache(src.to_str().unwrap(), None, true, &dir).unwrap();
        let df = scan_ipc_cache(&ipc).unwrap().collect().unwrap();
        assert_eq!(df.height(), 2);
        assert_eq!(df.get_column_names().len(), 2);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gbk_csv_roundtrip_via_ipc_cache() {
        let (bytes, _, _) = encoding_rs::GBK.encode("姓名,金额\n张三,10\n李四,20\n");
        let src = tmp_path("gbk", "csv");
        std::fs::write(&src, &*bytes).unwrap();
        let dir = cache_dir("gbk");
        let ipc = csv_to_ipc_cache(src.to_str().unwrap(), None, true, &dir).unwrap();
        let df = scan_ipc_cache(&ipc).unwrap().collect().unwrap();
        assert_eq!(df.height(), 2);
        let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
        assert_eq!(names, vec!["姓名".to_string(), "金额".to_string()]);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_hit_and_invalidate_on_change() {
        let src = tmp_path("hit", "csv");
        std::fs::write(&src, "a\n1\n2\n").unwrap();
        let dir = cache_dir("hit");
        let p1 = csv_to_ipc_cache(src.to_str().unwrap(), None, true, &dir).unwrap();
        let p2 = csv_to_ipc_cache(src.to_str().unwrap(), None, true, &dir).unwrap();
        assert_eq!(p1, p2, "同一源文件应命中同一缓存");
        // 不同表头参数 → 不同缓存
        let p_nh = csv_to_ipc_cache(src.to_str().unwrap(), None, false, &dir).unwrap();
        assert_ne!(p1, p_nh);
        // 内容变化(size 变) → 新缓存
        std::fs::write(&src, "a\n1\n2\n3\n4\n").unwrap();
        let p3 = csv_to_ipc_cache(src.to_str().unwrap(), None, true, &dir).unwrap();
        assert_ne!(p1, p3, "源文件变化后应生成新缓存");
        assert_eq!(scan_ipc_cache(&p3).unwrap().collect().unwrap().height(), 4);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod csv_import_tests {
    use super::*;

    fn write_tmp(tag: &str, bytes: &[u8]) -> String {
        let p = std::env::temp_dir().join(format!("roe_csvtest_{}_{}.csv", std::process::id(), tag));
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().to_string()
    }

    fn col_names(df: &DataFrame) -> Vec<String> {
        df.get_column_names().iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn csv_utf8_comma_infers_int() {
        let p = write_tmp("utf8", b"name,amt\nA,1\nB,2\n");
        let df = df_from_csv_file(&p, None, true).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(df.height(), 2);
        assert_eq!(df.width(), 2);
        assert!(df.columns()[1].dtype().is_integer());
    }

    #[test]
    fn csv_utf8_bom() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(b"name,amt\nA,1\n");
        let p = write_tmp("bom", &data);
        let df = df_from_csv_file(&p, None, true).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(col_names(&df)[0], "name");
    }

    #[test]
    fn csv_gbk() {
        let (bytes, _, _) = encoding_rs::GBK.encode("姓名,金额\n张三,10\n李四,20\n");
        let p = write_tmp("gbk", &bytes);
        let df = df_from_csv_file(&p, None, true).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(col_names(&df), vec!["姓名".to_string(), "金额".to_string()]);
        assert_eq!(df.height(), 2);
    }

    #[test]
    fn csv_semicolon_and_tab_autodetect() {
        let p = write_tmp("semi", b"a;b;c\n1;2;3\n");
        let df = df_from_csv_file(&p, None, true).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(df.width(), 3);
        let p = write_tmp("tab", b"a\tb\n1\t2\n");
        let df = df_from_csv_file(&p, None, true).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(df.width(), 2);
    }

    #[test]
    fn csv_explicit_delimiter() {
        let p = write_tmp("pipe", b"a|b\n1|2\n");
        let df = df_from_csv_file(&p, Some('|'), true).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(df.width(), 2);
    }

    #[test]
    fn csv_no_header() {
        let p = write_tmp("nohdr", b"1,2\n3,4\n");
        let df = df_from_csv_file(&p, None, false).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(df.height(), 2);
    }
}

// ════════════════════════════════════════════════════════════
// 测试
// ════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use smallvec::smallvec as sv;

    fn make_test_sheet() -> Sheet {
        // Region | Product | Sales
        // East   | Apple   | 100
        // East   | Banana  | 200
        // West   | Apple   | 150
        // West   | Banana  | 50
        let mut grid = SparseGrid::default();
        grid.cols.insert(
            0,
            SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: sv![
                        CellValue::Text("Region".into()),
                        CellValue::Text("East".into()),
                        CellValue::Text("East".into()),
                        CellValue::Text("West".into()),
                        CellValue::Text("West".into()),
                    ],
                }],
            },
        );
        grid.cols.insert(
            1,
            SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: sv![
                        CellValue::Text("Product".into()),
                        CellValue::Text("Apple".into()),
                        CellValue::Text("Banana".into()),
                        CellValue::Text("Apple".into()),
                        CellValue::Text("Banana".into()),
                    ],
                }],
            },
        );
        grid.cols.insert(
            2,
            SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: sv![
                        CellValue::Text("Sales".into()),
                        CellValue::Number(100.0),
                        CellValue::Number(200.0),
                        CellValue::Number(150.0),
                        CellValue::Number(50.0),
                    ],
                }],
            },
        );
        Sheet {
            col_style: vec![],
            tables: vec![],
            name: "data".into(),
            cells: grid,
            col_widths: vec![],
            row_heights: vec![],
            frozen_rows: 0,
            frozen_cols: 0,
        date1904: false, // ROOT-041: 测试/桥接代码构造的Sheet，默认非1904日期系统
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
            floating_images: vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            floating_charts: vec![],
            floating_stat_charts: vec![],
            default_col_width: 0.0,
            default_row_height: 0.0,
        }
    }

    fn make_test_sheet_2val() -> Sheet {
        // Region | Product | Sales | Quantity
        // East   | Apple   | 100   | 10
        // East   | Banana  | 200   | 20
        // West   | Apple   | 150   | 15
        // West   | Banana  | 50    | 5
        let mut grid = SparseGrid::default();
        grid.cols.insert(
            0,
            SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: sv![
                        CellValue::Text("Region".into()),
                        CellValue::Text("East".into()),
                        CellValue::Text("East".into()),
                        CellValue::Text("West".into()),
                        CellValue::Text("West".into()),
                    ],
                }],
            },
        );
        grid.cols.insert(
            1,
            SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: sv![
                        CellValue::Text("Product".into()),
                        CellValue::Text("Apple".into()),
                        CellValue::Text("Banana".into()),
                        CellValue::Text("Apple".into()),
                        CellValue::Text("Banana".into()),
                    ],
                }],
            },
        );
        grid.cols.insert(
            2,
            SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: sv![
                        CellValue::Text("Sales".into()),
                        CellValue::Number(100.0),
                        CellValue::Number(200.0),
                        CellValue::Number(150.0),
                        CellValue::Number(50.0),
                    ],
                }],
            },
        );
        grid.cols.insert(
            3,
            SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: sv![
                        CellValue::Text("Quantity".into()),
                        CellValue::Number(10.0),
                        CellValue::Number(20.0),
                        CellValue::Number(15.0),
                        CellValue::Number(5.0),
                    ],
                }],
            },
        );
        Sheet {
            col_style: vec![],
            tables: vec![],
            name: "data".into(),
            cells: grid,
            col_widths: vec![],
            row_heights: vec![],
            frozen_rows: 0,
            frozen_cols: 0,
        date1904: false, // ROOT-041: 测试/桥接代码构造的Sheet，默认非1904日期系统
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
            floating_images: vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            floating_charts: vec![],
            floating_stat_charts: vec![],
            default_col_width: 0.0,
            default_row_height: 0.0,
        }
    }

    fn make_workbook() -> Workbook {
        Workbook {
            sheets: vec![make_test_sheet()],
            named_ranges: Default::default(),
            named_formulas: HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn test_sheet_to_dataframe_basic() {
        let sheet = make_test_sheet();
        let df = sheet_to_dataframe(&sheet).unwrap();
        assert_eq!(df.height(), 4);
        let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
        assert_eq!(names, vec!["Region", "Product", "Sales"]);
    }

    #[test]
    fn test_pivot_into_workbook_v2() {
        let mut wb = make_workbook();
        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec!["Product".into()],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PivotV2".into()).unwrap();
        assert_eq!(wb.sheets[idx].name, "PivotV2");
        let meta = wb.sheets[idx].pivot.as_ref().expect("pivot meta should be set");

        assert_eq!(meta.row_field_names, vec!["Region".to_string()]);
        assert_eq!(meta.col_field_names, vec!["Product".to_string()]);
        assert_eq!(meta.value_fields.len(), 1);

        // row_tree: 虚拟根 -> East, West (2个叶子)
        assert_eq!(meta.row_tree.children.len(), 2);
        let mut row_values: Vec<String> = meta.row_tree.children.iter().map(|c| c.value.clone()).collect();
        row_values.sort();
        assert_eq!(row_values, vec!["East".to_string(), "West".to_string()]);

        // col_tree: 虚拟根 -> Apple, Banana (2个叶子，单值字段无额外层)
        assert_eq!(meta.col_tree.children.len(), 2);
        let mut col_values: Vec<String> = meta.col_tree.children.iter().map(|c| c.value.clone()).collect();
        col_values.sort();
        assert_eq!(col_values, vec!["Apple".to_string(), "Banana".to_string()]);

        // 数据矩阵核对四个值
        let east_apple = meta.data.get(&["East".to_string()], &["Apple".to_string()], 0);
        let east_banana = meta.data.get(&["East".to_string()], &["Banana".to_string()], 0);
        let west_apple = meta.data.get(&["West".to_string()], &["Apple".to_string()], 0);
        let west_banana = meta.data.get(&["West".to_string()], &["Banana".to_string()], 0);
        assert_eq!(east_apple, Some(100.0));
        assert_eq!(east_banana, Some(200.0));
        assert_eq!(west_apple, Some(150.0));
        assert_eq!(west_banana, Some(50.0));

        // spans: 单字段单值场景，叶子 span 都应为1
        assert_eq!(meta.row_tree.cached_span, 2);
        assert_eq!(meta.col_tree.cached_span, 2);
    }

    #[test]
    fn test_pivot_into_workbook_v2_filter_fields_populated() {
        let mut wb = make_workbook();
        let mut filters = std::collections::HashMap::new();
        filters.insert("Region".to_string(), vec!["East".to_string()]);
        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec!["Product".into()],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters,
        };
        let idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PivotFilterTest".into()).unwrap();
        let meta = wb.sheets[idx].pivot.as_ref().expect("pivot meta should be set");
        assert_eq!(meta.filter_fields.len(), 1, "filter_fields 应该被填充，不应为空");
        assert_eq!(meta.filter_fields[0].name, "Region");
    }

    #[test]
    fn test_pivot_v2_ex_sort_desc() {
        let mut wb = make_workbook();
        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec!["Product".into()],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let sort = vec![("Region".to_string(), false)];
        let idx = pivot_sheet_into_workbook_v2_ex(&mut wb, 0, &cfg, "PivotSort".into(), &sort).unwrap();
        let meta = wb.sheets[idx].pivot.as_ref().unwrap();
        let rows: Vec<String> = meta.row_tree.children.iter().map(|c| c.value.clone()).collect();
        assert_eq!(rows, vec!["West".to_string(), "East".to_string()]);
        let cols: Vec<String> = meta.col_tree.children.iter().map(|c| c.value.clone()).collect();
        assert_eq!(cols, vec!["Apple".to_string(), "Banana".to_string()]);
        assert_eq!(meta.data.get(&["West".to_string()], &["Apple".to_string()], 0), Some(150.0));
    }

    #[test]
    fn test_refresh_pivot_in_place_keeps_meta_and_applies_filter_sort() {
        let mut wb = make_workbook();
        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec!["Product".into()],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PivotRefresh".into()).unwrap();
        {
            let m = wb.sheets[idx].pivot.as_mut().unwrap();
            m.name = Some("PT1".into());
            m.style_name = Some("PivotStyleLight20".into());
            m.source_ref = Some("A1:C5".into());
            m.base_row = 5;
            m.filter_fields = vec![PivotFieldDef::new("Region")];
            m.field_filters.insert("Region".into(), vec!["East".into()]);
            m.sort.push(("Product".into(), false));
        }
        let n_before = wb.sheets.len();
        refresh_pivot_in_place(&mut wb, idx).unwrap();
        assert_eq!(wb.sheets.len(), n_before, "临时 sheet 必须删除");
        assert_eq!(wb.sheets[idx].name, "PivotRefresh");
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        assert_eq!(m.name.as_deref(), Some("PT1"));
        assert_eq!(m.style_name.as_deref(), Some("PivotStyleLight20"));
        assert_eq!(m.source_ref.as_deref(), Some("A1:C5"));
        assert_eq!(m.base_row, 5);
        // 筛选生效:只剩 East
        let rows: Vec<String> = m.row_tree.children.iter().map(|c| c.value.clone()).collect();
        assert_eq!(rows, vec!["East".to_string()]);
        // 列降序
        let cols: Vec<String> = m.col_tree.children.iter().map(|c| c.value.clone()).collect();
        assert_eq!(cols, vec!["Banana".to_string(), "Apple".to_string()]);
        assert_eq!(m.data.get(&["East".to_string()], &["Apple".to_string()], 0), Some(100.0));
        // 页字段保留且同步 selected_values;data_row_start = 1 个页字段 + 1 + 1 个列头层
        assert_eq!(m.filter_fields.len(), 1);
        assert_eq!(m.filter_fields[0].name, "Region");
        assert_eq!(m.filter_fields[0].selected_values, vec!["East".to_string()]);
        assert_eq!(m.data_row_start, 3);
        assert!(m.cells_rect.is_some(), "刷新后应重新物化");
    }

    #[test]
    fn test_refresh_pivot_in_place_rejects_non_pivot_sheet() {
        let mut wb = make_workbook();
        assert!(refresh_pivot_in_place(&mut wb, 0).is_err());
    }

    fn base_pivot(wb: &mut Workbook) -> usize {
        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec!["Product".into()],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        pivot_sheet_into_workbook_v2(wb, 0, &cfg, "PivotCfg".into()).unwrap()
    }

    #[test]
    fn test_set_pivot_config_swaps_rows_and_cols() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        set_pivot_config_in_place(
            &mut wb, idx,
            vec!["Product".into()], vec!["Region".into()], vec!["Sales".into()], vec![],
        ).unwrap();
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        let rows: Vec<String> = m.row_tree.children.iter().map(|c| c.value.clone()).collect();
        assert_eq!(rows, vec!["Apple".to_string(), "Banana".to_string()]);
        assert_eq!(m.data.get(&["Apple".to_string()], &["East".to_string()], 0), Some(100.0));
        assert_eq!(m.value_fields[0].source_field.as_deref(), Some("Sales"));
    }

    #[test]
    fn test_set_pivot_config_prunes_unused_field_filters_and_sort() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        {
            let m = wb.sheets[idx].pivot.as_mut().unwrap();
            m.field_filters.insert("Region".into(), vec!["East".into()]);
            m.sort.push(("Region".into(), false));
        }
        set_pivot_config_in_place(
            &mut wb, idx,
            vec!["Product".into()], vec![], vec!["Sales".into()], vec![],
        ).unwrap();
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        assert!(m.field_filters.is_empty());
        assert!(m.sort.is_empty());
        assert_eq!(m.row_tree.children.len(), 2, "Region 不再使用,不应继续过滤行");
    }

    #[test]
    fn test_set_pivot_config_page_field() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        set_pivot_config_in_place(
            &mut wb, idx,
            vec!["Product".into()], vec![], vec!["Sales".into()], vec!["Region".into()],
        ).unwrap();
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        assert_eq!(m.filter_fields.len(), 1);
        assert_eq!(m.filter_fields[0].name, "Region");
        assert!(m.filter_fields[0].selected_values.is_empty());
        assert_eq!(m.data_row_start, 2);
    }

    #[test]
    fn test_set_pivot_config_rejects_bad_input_and_keeps_meta() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        assert!(set_pivot_config_in_place(
            &mut wb, idx, vec!["Nope".into()], vec![], vec!["Sales".into()], vec![]).is_err());
        assert!(set_pivot_config_in_place(
            &mut wb, idx, vec!["Region".into()], vec!["Region".into()], vec!["Sales".into()], vec![]).is_err());
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        assert_eq!(m.row_field_names, vec!["Region".to_string()]);
        assert_eq!(m.col_field_names, vec!["Product".to_string()]);
        assert!(set_pivot_config_in_place(&mut wb, 0, vec![], vec![], vec![], vec![]).is_err());
    }

    #[test]
    fn test_pivot_config_value_basic() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        let v = pivot_config_value(&wb, idx).unwrap();
        assert_eq!(v["rows"], serde_json::json!(["Region"]));
        assert_eq!(v["cols"], serde_json::json!(["Product"]));
        assert_eq!(v["values"], serde_json::json!(["Sales"]));
        assert_eq!(v["filters"], serde_json::json!([]));
        assert_eq!(v["source_idx"], serde_json::json!(0));
        assert_eq!(v["fields"], serde_json::json!(["Region", "Product", "Sales"]));
        assert!(pivot_config_value(&wb, 0).is_err(), "非透视 sheet 应报错");
        assert!(pivot_config_value(&wb, 99).is_err(), "越界应报错");
    }

    #[test]
    fn test_pivot_set_filter_in_place_filter_row_field_and_clear() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        pivot_set_filter_in_place(&mut wb, idx, "Region", vec!["East".into()]).unwrap();
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        let rows: Vec<String> = m.row_tree.children.iter().map(|c| c.value.clone()).collect();
        assert_eq!(rows, vec!["East".to_string()]);
        assert!(m.field_filters.contains_key("Region"));
        pivot_set_filter_in_place(&mut wb, idx, "Region", vec![]).unwrap();
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        assert!(m.field_filters.is_empty());
        assert_eq!(m.row_tree.children.len(), 2);
    }

    #[test]
    fn test_pivot_set_filter_in_place_rejects_bad_input_and_keeps_meta() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        assert!(pivot_set_filter_in_place(&mut wb, idx, "Sales", vec!["1".into()]).is_err(), "值字段不可筛选");
        assert!(pivot_set_filter_in_place(&mut wb, idx, "Nope", vec!["x".into()]).is_err());
        assert!(wb.sheets[idx].pivot.as_ref().unwrap().field_filters.is_empty());
        assert!(pivot_set_filter_in_place(&mut wb, 0, "Region", vec![]).is_err(), "非透视 sheet");
        assert!(pivot_set_filter_in_place(&mut wb, 99, "Region", vec![]).is_err(), "越界");
    }

    #[test]
    fn test_pivot_set_sort_in_place_desc_then_asc() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        pivot_set_sort_in_place(&mut wb, idx, "Product", false).unwrap();
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        let cols: Vec<String> = m.col_tree.children.iter().map(|c| c.value.clone()).collect();
        assert_eq!(cols, vec!["Banana".to_string(), "Apple".to_string()]);
        assert_eq!(m.sort.len(), 1);
        pivot_set_sort_in_place(&mut wb, idx, "Product", true).unwrap();
        let m = wb.sheets[idx].pivot.as_ref().unwrap();
        let cols: Vec<String> = m.col_tree.children.iter().map(|c| c.value.clone()).collect();
        assert_eq!(cols, vec!["Apple".to_string(), "Banana".to_string()]);
        assert!(m.sort.is_empty(), "升序应删除排序条目");
    }

    #[test]
    fn test_pivot_set_sort_in_place_rejects_page_and_bad_input() {
        let mut wb = make_workbook();
        let idx = base_pivot(&mut wb);
        set_pivot_config_in_place(
            &mut wb, idx,
            vec!["Product".into()], vec![], vec!["Sales".into()], vec!["Region".into()],
        ).unwrap();
        assert!(pivot_set_sort_in_place(&mut wb, idx, "Region", false).is_err(), "页字段不可排序");
        assert!(pivot_set_sort_in_place(&mut wb, idx, "Sales", false).is_err(), "值字段不可排序");
        assert!(wb.sheets[idx].pivot.as_ref().unwrap().sort.is_empty());
        assert!(pivot_set_sort_in_place(&mut wb, 0, "Product", false).is_err(), "非透视 sheet");
        assert!(pivot_set_sort_in_place(&mut wb, 99, "Product", false).is_err(), "越界");
    }

    #[test]
    fn test_pivot_into_workbook_v2_multivalue() {
        let wb_sheet = make_test_sheet_2val();
        let mut wb = Workbook {
            sheets: vec![wb_sheet],
            named_ranges: Default::default(),
            named_formulas: HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec!["Product".into()],
            values: vec!["Sales".into(), "Quantity".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PivotV2MultiVal".into()).unwrap();
        let meta = wb.sheets[idx].pivot.as_ref().expect("pivot meta should be set");

        assert_eq!(meta.value_fields.len(), 2);

        // row_tree 不受多值字段影响：East, West 两个叶子
        assert_eq!(meta.row_tree.children.len(), 2);
        assert_eq!(meta.row_tree.cached_span, 2);

        // col_tree: Apple/Banana 是叶子节点，不手动插入值字段层节点
        // （与 roe-parser::ooxml::pivot 的旧路径保持一致，多值展开完全交给
        // PivotMeta::compute_spans() 在渲染前统一处理，tile.rs 依赖这个假设）
        assert_eq!(meta.col_tree.children.len(), 2);
        for child in &meta.col_tree.children {
            assert_eq!(child.children.len(), 0, "col_tree叶子不应包含手动插入的值字段子节点");
        }
        // col_tree 叶子span：2个Product取值 x 2个值字段 = 4（compute_spans内部用n_values相乘）
        assert_eq!(meta.col_tree.cached_span, 4);

        // 数据矩阵核对：value_idx 0=Sales, 1=Quantity（顺序对应 cfg.values）
        let east_apple_sales = meta.data.get(&["East".to_string()], &["Apple".to_string()], 0);
        let east_apple_qty = meta.data.get(&["East".to_string()], &["Apple".to_string()], 1);
        let west_banana_sales = meta.data.get(&["West".to_string()], &["Banana".to_string()], 0);
        let west_banana_qty = meta.data.get(&["West".to_string()], &["Banana".to_string()], 1);
        assert_eq!(east_apple_sales, Some(100.0));
        assert_eq!(east_apple_qty, Some(10.0));
        assert_eq!(west_banana_sales, Some(50.0));
        assert_eq!(west_banana_qty, Some(5.0));

        // 布局参数：多值场景 col_header_rows 应比字段数多1（"值"标题行）
        assert_eq!(meta.col_header_rows(), 2); // 1个col字段(Product) + 1个"值"标题层
    }

    #[test]
    fn test_pivot_v2_materializes_cells_readable() {
        let wb_sheet = make_test_sheet_2val();
        let mut wb = Workbook {
            sheets: vec![wb_sheet],
            named_ranges: Default::default(),
            named_formulas: HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let cfg = PivotConfig {
            rows: vec!["Region".into(), "Product".into()],
            cols: vec![],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PivotMat".into()).unwrap();
        let sh = &wb.sheets[idx];
        assert!(sh.pivot.as_ref().unwrap().cells_rect.is_some());
        // 行表头
        assert_eq!(sh.cells.get(3, 0).to_display_string(), "East");
        assert_eq!(sh.cells.get(4, 1).to_display_string(), "Apple");
        assert_eq!(sh.cells.get(6, 0).to_display_string(), "East Total");
        assert_eq!(sh.cells.get(10, 0).to_display_string(), "West Total");
        // 数据与小计
        assert!(matches!(sh.cells.get(4, 2), CellValue::Number(v) if v == 100.0));
        assert!(matches!(sh.cells.get(6, 2), CellValue::Number(v) if v == 300.0));
        // 总计
        assert_eq!(sh.cells.get(11, 0).to_display_string(), "总计");
        assert!(matches!(sh.cells.get(11, 2), CellValue::Number(v) if v == 500.0));
    }

    #[test]
    fn test_pivot_subtotal_row() {
        // v151新增：验证行方向小计（仅最外层）。
        // 数据：Region(East/West) x Product(Apple/Banana) x Sales，2层row_fields，无col_fields。
        // East: Apple=100, Banana=200 → East小计应为300
        // West: Apple=150, Banana=50  → West小计应为200
        let wb_sheet = make_test_sheet_2val();
        let mut wb = Workbook {
            sheets: vec![wb_sheet],
            named_ranges: Default::default(),
            named_formulas: HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let cfg = PivotConfig {
            rows: vec!["Region".into(), "Product".into()],
            cols: vec![],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PivotSubtotalTest".into()).unwrap();
        let meta = wb.sheets[idx].pivot.as_ref().expect("pivot meta should be set");

        // row_tree: East/West两个最外层节点，各自应有2个明细子节点(Apple/Banana) + 1个小计节点 = 3
        assert_eq!(meta.row_tree.children.len(), 2);
        for outer in &meta.row_tree.children {
            assert_eq!(outer.children.len(), 3, "每个外层分组应有2明细+1小计=3个子节点");
            let subtotal_children: Vec<_> = outer.children.iter().filter(|c| c.is_subtotal).collect();
            assert_eq!(subtotal_children.len(), 1, "应恰好有1个小计子节点");
            let subtotal_node = subtotal_children[0];
            assert_eq!(subtotal_node.value, format!("{} Total", outer.value));
            // 小计节点应排在最后（子节点末尾追加）
            assert!(outer.children.last().unwrap().is_subtotal, "小计节点应位于子节点末尾");
        }

        // 数据核对：明细值
        let east_apple = meta.data.get(&["East".to_string(), "Apple".to_string()], &[], 0);
        let east_banana = meta.data.get(&["East".to_string(), "Banana".to_string()], &[], 0);
        let west_apple = meta.data.get(&["West".to_string(), "Apple".to_string()], &[], 0);
        let west_banana = meta.data.get(&["West".to_string(), "Banana".to_string()], &[], 0);
        assert_eq!(east_apple, Some(100.0));
        assert_eq!(east_banana, Some(200.0));
        assert_eq!(west_apple, Some(150.0));
        assert_eq!(west_banana, Some(50.0));

        // 核心断言：小计 = 该分组下子项之和（不是巧合的复制）
        let east_subtotal = meta.data.get(&["East".to_string(), "".to_string()], &[], 0);
        let west_subtotal = meta.data.get(&["West".to_string(), "".to_string()], &[], 0);
        assert_eq!(east_subtotal, Some(300.0), "East小计应为100+200=300");
        assert_eq!(west_subtotal, Some(200.0), "West小计应为150+50=200");

        // 总计不应被小计污染重复计算：应仍为原始明细之和 100+200+150+50=500，不是500+300+200
        let grand_total = meta.data.get(&[], &[], 0);
        assert_eq!(grand_total, Some(500.0), "总计不应把小计行也算进去重复计算");
    }

    /// v152补测试（技术债1）：构造 2层行字段(Category/Item) x 2层列字段
    /// (Region/Channel) 的16行明细数据，验证列方向小计、行列小计交汇点、
    /// 总计缺口修复（总计x列小计、行小计x总计）四类v152新增分支。
    /// 数据为等差数列10..160，手算方便，核对标准迁移自
    /// render_pivot_subtotal_corner.rs 的"横向求和=纵向求和=交汇点直接查值"。
    fn make_test_sheet_2row2col() -> Sheet {
        // Category | Item | Region | Channel | Sales
        // 服装 | 上衣 | 东 | 线上 | 10
        // 服装 | 上衣 | 东 | 线下 | 20
        // 服装 | 上衣 | 西 | 线上 | 30
        // 服装 | 上衣 | 西 | 线下 | 40
        // 服装 | 裤子 | 东 | 线上 | 50
        // 服装 | 裤子 | 东 | 线下 | 60
        // 服装 | 裤子 | 西 | 线上 | 70
        // 服装 | 裤子 | 西 | 线下 | 80
        // 电子 | 手机 | 东 | 线上 | 90
        // 电子 | 手机 | 东 | 线下 | 100
        // 电子 | 手机 | 西 | 线上 | 110
        // 电子 | 手机 | 西 | 线下 | 120
        // 电子 | 耳机 | 东 | 线上 | 130
        // 电子 | 耳机 | 东 | 线下 | 140
        // 电子 | 耳机 | 西 | 线上 | 150
        // 电子 | 耳机 | 西 | 线下 | 160
        let categories = ["服装", "服装", "服装", "服装", "服装", "服装", "服装", "服装",
                           "电子", "电子", "电子", "电子", "电子", "电子", "电子", "电子"];
        let items = ["上衣", "上衣", "上衣", "上衣", "裤子", "裤子", "裤子", "裤子",
                     "手机", "手机", "手机", "手机", "耳机", "耳机", "耳机", "耳机"];
        let regions = ["东", "东", "西", "西", "东", "东", "西", "西",
                       "东", "东", "西", "西", "东", "东", "西", "西"];
        let channels = ["线上", "线下", "线上", "线下", "线上", "线下", "线上", "线下",
                        "线上", "线下", "线上", "线下", "线上", "线下", "线上", "线下"];
        let sales: [f64; 16] = [10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0,
                                 90.0, 100.0, 110.0, 120.0, 130.0, 140.0, 150.0, 160.0];

        let mut grid = SparseGrid::default();
        let mut cat_values = sv![CellValue::Text("Category".into())];
        let mut item_values = sv![CellValue::Text("Item".into())];
        let mut region_values = sv![CellValue::Text("Region".into())];
        let mut channel_values = sv![CellValue::Text("Channel".into())];
        let mut sales_values = sv![CellValue::Text("Sales".into())];
        for i in 0..16 {
            cat_values.push(CellValue::Text(categories[i].into()));
            item_values.push(CellValue::Text(items[i].into()));
            region_values.push(CellValue::Text(regions[i].into()));
            channel_values.push(CellValue::Text(channels[i].into()));
            sales_values.push(CellValue::Number(sales[i]));
        }
        grid.cols.insert(0, SparseColumn { blocks: sv![CellBlock { start_row: 0, values: cat_values }] });
        grid.cols.insert(1, SparseColumn { blocks: sv![CellBlock { start_row: 0, values: item_values }] });
        grid.cols.insert(2, SparseColumn { blocks: sv![CellBlock { start_row: 0, values: region_values }] });
        grid.cols.insert(3, SparseColumn { blocks: sv![CellBlock { start_row: 0, values: channel_values }] });
        grid.cols.insert(4, SparseColumn { blocks: sv![CellBlock { start_row: 0, values: sales_values }] });

        Sheet {
            col_style: vec![],
            tables: vec![],
            name: "data".into(),
            cells: grid,
            col_widths: vec![],
            row_heights: vec![],
            frozen_rows: 0,
            frozen_cols: 0,
            date1904: false,
            zoom_scale: None,
            show_grid_lines: None,
            col_hidden: vec![],
            row_hidden: vec![],
            col_outline_level: vec![],
            row_outline_level: vec![],
            sparkline_groups: vec![],
            cell_formats: std::collections::HashMap::new(),
            cond_formats: vec![],
            data_validations: vec![],
            auto_filter: None,
            merged_cells: vec![],
            pivot: None,
            floating_images: vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            floating_charts: vec![],
            floating_stat_charts: vec![],
            default_col_width: 0.0,
            default_row_height: 0.0,
        }
    }

    #[test]
    fn test_pivot_subtotal_col_and_corner() {
        // v152补测试（技术债1）：迁移 render_pivot_subtotal_corner.rs 的验证逻辑
        // 为正式 #[test]。数据集见 make_test_sheet_2row2col 顶部注释。
        let sheet = make_test_sheet_2row2col();
        let mut wb = Workbook {
            sheets: vec![sheet],
            named_ranges: Default::default(),
            named_formulas: HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let cfg = PivotConfig {
            rows: vec!["Category".into(), "Item".into()],
            cols: vec!["Region".into(), "Channel".into()],
            values: vec!["Sales".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };
        let idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PivotColCornerTest".into()).unwrap();
        let meta = wb.sheets[idx].pivot.as_ref().expect("pivot meta should be set");

        // --- 列方向小计验证（阶段2主线）---
        // col_tree 最外层应有2个分组(东/西)，各自2个明细(线上/线下)+1个小计=3
        assert_eq!(meta.col_tree.children.len(), 2);
        for outer in &meta.col_tree.children {
            assert_eq!(outer.children.len(), 3, "每个外层列分组应有2明细+1小计=3个子节点");
            let subtotal_children: Vec<_> = outer.children.iter().filter(|c| c.is_subtotal).collect();
            assert_eq!(subtotal_children.len(), 1, "应恰好有1个列小计子节点");
            assert!(outer.children.last().unwrap().is_subtotal, "列小计节点应位于子节点末尾");
        }

        // 东列小计 = 服装东(10+20+50+60) + 电子东(90+100+130+140) = 140+460 = 600
        // 西列小计 = 服装西(30+40+70+80) + 电子西(110+120+150+160) = 220+540 = 760
        let dong_col_subtotal_clothing = meta.data.get(
            &["服装".to_string(), "上衣".to_string()],
            &["东".to_string(), "".to_string()],
            0,
        );
        assert_eq!(dong_col_subtotal_clothing, Some(30.0), "服装上衣在东列小计=10+20=30");

        // --- 行小计横向求和 = 列小计纵向求和 = 交汇点直接查值（核心正确性标准）---
        for (cat, region) in [("服装", "东"), ("服装", "西"), ("电子", "东"), ("电子", "西")] {
            let row_subtotal_path = vec![cat.to_string(), String::new()];
            let col_subtotal_path = vec![region.to_string(), String::new()];

            // 横向：该Category行小计，在该Region的线上+线下两列上求和
            let mut horizontal_sum = 0.0;
            for channel in ["线上", "线下"] {
                let col_path = vec![region.to_string(), channel.to_string()];
                if let Some(v) = meta.data.get(&row_subtotal_path, &col_path, 0) {
                    horizontal_sum += v;
                }
            }

            // 纵向：该Region列小计，在该Category的所有Item行上求和
            let items_for_cat: [&str; 2] = if cat == "服装" { ["上衣", "裤子"] } else { ["手机", "耳机"] };
            let mut vertical_sum = 0.0;
            for item in items_for_cat {
                let row_path = vec![cat.to_string(), item.to_string()];
                if let Some(v) = meta.data.get(&row_path, &col_subtotal_path, 0) {
                    vertical_sum += v;
                }
            }

            // 交汇点本身
            let corner = meta.data.get(&row_subtotal_path, &col_subtotal_path, 0);

            assert!(
                (horizontal_sum - vertical_sum).abs() < 0.001,
                "{cat}x{region}: 横向求和{horizontal_sum}应等于纵向求和{vertical_sum}"
            );
            assert_eq!(
                corner,
                Some(horizontal_sum),
                "{cat}x{region}: 交汇点直接查值应等于横向/纵向求和"
            );
        }

        // 手算校验：服装x东交汇点 = 10+20+50+60 = 140
        let fz_dong = meta.data.get(
            &["服装".to_string(), "".to_string()],
            &["东".to_string(), "".to_string()],
            0,
        );
        assert_eq!(fz_dong, Some(140.0), "服装Total x 东Total 应为10+20+50+60=140");

        // 电子x西交汇点 = 110+120+150+160 = 540
        let dz_xi = meta.data.get(
            &["电子".to_string(), "".to_string()],
            &["西".to_string(), "".to_string()],
            0,
        );
        assert_eq!(dz_xi, Some(540.0), "电子Total x 西Total 应为110+120+150+160=540");

        // --- 总计缺口修复验证：总计x列小计、行小计x总计 ---
        // 总计x东列小计 = 所有Category在东列上的和 = 服装东140 + 电子东460 = 600
        let grand_x_dong = meta.data.get(&[], &["东".to_string(), "".to_string()], 0);
        assert_eq!(grand_x_dong, Some(600.0), "总计行x东列小计应为600");

        // 服装行小计x总计列 = 服装全部明细之和 = 10+20+30+40+50+60+70+80 = 360
        let fz_x_grand = meta.data.get(&["服装".to_string(), "".to_string()], &[], 0);
        assert_eq!(fz_x_grand, Some(360.0), "服装行小计x总计列应为360");

        // --- 总计不应被小计污染 ---
        // 16行数据等差数列10..160求和 = (10+160)*16/2 = 1360
        let grand_total = meta.data.get(&[], &[], 0);
        assert_eq!(grand_total, Some(1360.0), "总计不应把行/列小计重复计算进去");
    }

    #[test]
    fn test_df_preview() {
        let sheet = make_test_sheet();
        let df = sheet_to_dataframe(&sheet).unwrap();
        let json_str = df_preview(&df, 2).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["columns"], serde_json::json!(["Region", "Product", "Sales"]));
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);
        assert_eq!(v["total_rows"], 4);
    }

    #[test]
    fn test_df_info() {
        let sheet = make_test_sheet();
        let df = sheet_to_dataframe(&sheet).unwrap();
        let json_str = df_info(&df).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["rows"], 4);
        let cols = v["columns"].as_array().unwrap();
        assert_eq!(cols.len(), 3);

        // Sales 列应该是 float 类型，有 min/max/null_count
        let sales = cols.iter().find(|c| c["name"] == "Sales").unwrap();
        assert_eq!(sales["type"], "float");
        assert_eq!(sales["null_count"], 0);
        assert_eq!(sales["min"], 50.0);
        assert_eq!(sales["max"], 200.0);

        // Region 列应该是 string 类型，有 unique
        let region = cols.iter().find(|c| c["name"] == "Region").unwrap();
        assert_eq!(region["type"], "string");
        assert_eq!(region["unique"], 2);
    }

    #[test]
    fn test_sheet_preview_and_describe() {
        let wb = make_workbook();
        let preview = sheet_preview(&wb, 0, 10).unwrap();
        let v: serde_json::Value = serde_json::from_str(&preview).unwrap();
        assert_eq!(v["total_rows"], 4);

        let info = sheet_info(&wb, 0).unwrap();
        let v: serde_json::Value = serde_json::from_str(&info).unwrap();
        assert_eq!(v["rows"], 4);
        assert_eq!(v["columns"].as_array().unwrap().len(), 3);

        // 越界 sheet_idx 应报错
        assert!(sheet_preview(&wb, 99, 10).is_err());
        assert!(sheet_info(&wb, 99).is_err());
    }

    #[test]
    fn test_sql_into_workbook() {
        let mut wb = make_workbook();
        let idx = sql_sheet_into_workbook(
            &mut wb,
            0,
            "data",
            "SELECT Region, SUM(Sales) AS total FROM data GROUP BY Region ORDER BY Region",
            "SqlResult".into(),
        )
        .unwrap();

        let df = sheet_to_dataframe(&wb.sheets[idx]).unwrap();
        assert_eq!(df.height(), 2);
        let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
        assert!(names.contains(&"total".to_string()));
    }
}

// ════════════════════════════════════════════════════════════
// df_unique_values：某列的唯一值列表（供 AI 探查枚举列）
// ════════════════════════════════════════════════════════════

/// 返回 DataFrame 中指定列的唯一值列表，JSON 数组格式
/// 格式：["val1", "val2", ...]（统一转字符串）
pub fn df_unique_values(df: &DataFrame, field: &str) -> Result<String, BridgeError> {
    let col = df.column(field).map_err(|e| BridgeError::Other(format!("列不存在: {e}")))?;
    let series = col.as_series()
        .ok_or_else(|| BridgeError::Other("无法转换为 Series".into()))?;
    let unique = series.unique().map_err(BridgeError::Polars)?;
    let mut vals: Vec<serde_json::Value> = Vec::with_capacity(unique.len());
    for i in 0..unique.len() {
        let av = unique.get(i).unwrap_or(AnyValue::Null);
        vals.push(anyvalue_to_json(&av));
    }
    Ok(serde_json::to_string(&vals).unwrap())
}

/// LazyFrame schema 信息（不 collect，只看列名和类型）
/// 格式：{"columns": [{"name": "...", "type": "float|int|bool|datetime|string"}, ...]}
pub fn lazy_schema_info(lf: &mut LazyFrame) -> Result<String, BridgeError> {
    let schema = lf.collect_schema().map_err(BridgeError::Polars)?;
    let cols: Vec<serde_json::Value> = schema.iter().map(|(name, dtype): (&PlSmallStr, &DataType)| {
        let type_str = match dtype {
            DataType::Float64 | DataType::Float32 => "float",
            DataType::Int32 | DataType::Int64 | DataType::UInt32 | DataType::UInt64 => "int",
            DataType::Boolean => "bool",
            DataType::Date | DataType::Datetime(_, _) => "datetime",
            _ => "string",
        };
        serde_json::json!({"name": name.to_string(), "type": type_str})
    }).collect();
    Ok(serde_json::json!({"columns": cols}).to_string())
}


/// 惰性分页:只物化 [offset, offset+limit) 一页,total_rows 用惰性计数(不物化全表)。
/// 返回格式与 df_preview 相同,另加 "offset"。
pub fn lazy_page(lf: LazyFrame, offset: i64, limit: u32) -> Result<String, BridgeError> {
    let total_df = lf
        .clone()
        .select([polars::prelude::len().alias("n")])
        .collect()?;
    let total: u64 = match total_df.column("n")?.get(0)? {
        AnyValue::UInt32(v) => v as u64,
        AnyValue::UInt64(v) => v,
        _ => 0,
    };
    let off = offset.max(0);
    let page = lf.slice(off, limit).collect()?;
    let json = df_preview(&page, limit as usize)?;
    let mut v: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| BridgeError::Other(format!("分页 JSON 解析失败: {e}")))?;
    v["total_rows"] = serde_json::json!(total);
    v["offset"] = serde_json::json!(off);
    Ok(v.to_string())
}

#[cfg(test)]
mod cjk_sql_tests {
    use super::*;

    fn run(sql: &str) -> Result<DataFrame, BridgeError> {
        let df = df!("姓名" => ["甲", "乙", "丙"], "年龄" => [20i64, 15, 30]).unwrap();
        run_sql_on_datasets(vec![("用户表".to_string(), df)], sql)
    }

    #[test]
    fn cjk_table_and_column_quoted() {
        assert_eq!(run(r#"SELECT * FROM "用户表""#).unwrap().height(), 3);
        assert_eq!(run(r#"SELECT "姓名" FROM "用户表" WHERE "年龄" > 18"#).unwrap().height(), 2);
    }

    #[test]
    fn cjk_table_and_column_unquoted() {
        assert_eq!(run("SELECT * FROM 用户表").unwrap().height(), 3);
        assert_eq!(run("SELECT 姓名 FROM 用户表 WHERE 年龄 > 18").unwrap().height(), 2);
    }

    #[test]
    fn cjk_self_join_with_alias() {
        let out = run("SELECT u.姓名 FROM 用户表 u JOIN 用户表 v ON u.姓名 = v.姓名").unwrap();
        assert_eq!(out.height(), 3);
    }
}

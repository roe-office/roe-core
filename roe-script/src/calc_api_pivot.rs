//! 透视表 Rhai 接口的实现层。逻辑全部在 roe-polars-bridge,这里只做 Workbook 级薄封装,
//! 便于脱离 Rhai 单测。注册行写在 calc_api.rs 的宏块里(meta 脚本只扫描那里)。
use roe_model::calc::Workbook;

/// 取透视表配置 JSON;失败返回 "err:..."。
pub(crate) fn get_pivot_config_impl(wb: &Workbook, sheet_idx: i64) -> String {
    if sheet_idx < 0 {
        return format!("err:get_pivot_config: sheet_idx={sheet_idx} 越界");
    }
    match roe_polars_bridge::pivot_config_value(wb, sheet_idx as usize) {
        Ok(v) => v.to_string(),
        Err(e) => format!("err:get_pivot_config: {e}"),
    }
}

/// 透视区域左上、右下角(绝对坐标),用于标脏;非透视 sheet 返回空。
fn pivot_corners(wb: &Workbook, sheet_idx: usize) -> Vec<(u32, u32)> {
    match wb.sheets.get(sheet_idx).and_then(|s| s.pivot.as_ref()) {
        Some(m) => vec![(m.base_row, m.base_col), (m.last_row, m.last_col)],
        None => Vec::new(),
    }
}

fn check_idx(func: &str, sheet_idx: i64) -> Result<usize, String> {
    if sheet_idx < 0 {
        return Err(format!("{func}: sheet_idx={sheet_idx} 越界"));
    }
    Ok(sheet_idx as usize)
}

fn str_list(v: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    match v.get(key) {
        None => Ok(Vec::new()),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(|s| s.to_string()).ok_or_else(|| format!("{key} 必须是字符串数组")))
            .collect(),
        Some(_) => Err(format!("{key} 必须是字符串数组")),
    }
}

/// 就地整体设置行/列/值/页字段。config_json: {"rows":[],"cols":[],"values":[],"filters":[]}。
/// 未知键直接报错(防止键名写错被当成空列表)。成功返回需要标脏的单元格(改动前后区域的首尾角)。
pub(crate) fn set_config_impl(
    wb: &mut Workbook,
    sheet_idx: i64,
    config_json: &str,
) -> Result<Vec<(u32, u32)>, String> {
    let idx = check_idx("pivot_set_config", sheet_idx)?;
    let v: serde_json::Value = serde_json::from_str(config_json)
        .map_err(|e| format!("pivot_set_config: config_json 解析失败: {e}"))?;
    let obj = v.as_object().ok_or_else(|| "pivot_set_config: config_json 必须是对象".to_string())?;
    for k in obj.keys() {
        if !["rows", "cols", "values", "filters"].contains(&k.as_str()) {
            return Err(format!("pivot_set_config: 未知键 {k}(只允许 rows/cols/values/filters)"));
        }
    }
    let conv = |key: &str| str_list(&v, key).map_err(|m| format!("pivot_set_config: {m}"));
    let (rows, cols, values, filters) = (conv("rows")?, conv("cols")?, conv("values")?, conv("filters")?);
    let mut pts = pivot_corners(wb, idx);
    roe_polars_bridge::set_pivot_config_in_place(wb, idx, rows, cols, values, filters)
        .map_err(|e| format!("pivot_set_config: {e}"))?;
    pts.extend(pivot_corners(wb, idx));
    Ok(pts)
}

/// 设置某字段的筛选值(空数组=清除),字段须为页/行/列字段。
pub(crate) fn set_filter_impl(
    wb: &mut Workbook,
    sheet_idx: i64,
    field: &str,
    values_json: &str,
) -> Result<Vec<(u32, u32)>, String> {
    let idx = check_idx("pivot_set_filter", sheet_idx)?;
    let values: Vec<String> = serde_json::from_str(values_json)
        .map_err(|e| format!("pivot_set_filter: values_json 须为字符串数组: {e}"))?;
    let mut pts = pivot_corners(wb, idx);
    roe_polars_bridge::pivot_set_filter_in_place(wb, idx, field, values)
        .map_err(|e| format!("pivot_set_filter: {e}"))?;
    pts.extend(pivot_corners(wb, idx));
    Ok(pts)
}

/// 设置行/列字段排序,asc=true 升序(默认)。页字段、值字段会被拒绝。
pub(crate) fn set_sort_impl(
    wb: &mut Workbook,
    sheet_idx: i64,
    field: &str,
    asc: bool,
) -> Result<Vec<(u32, u32)>, String> {
    let idx = check_idx("pivot_set_sort", sheet_idx)?;
    let mut pts = pivot_corners(wb, idx);
    roe_polars_bridge::pivot_set_sort_in_place(wb, idx, field, asc)
        .map_err(|e| format!("pivot_set_sort: {e}"))?;
    pts.extend(pivot_corners(wb, idx));
    Ok(pts)
}

fn check_new_sheet_name(wb: &Workbook, name: &str) -> Result<String, String> {
    let name = name.trim().to_string();
    if name.is_empty() { return Err("sheet 名不能为空".into()); }
    if name.chars().count() > 31 { return Err("sheet 名不能超过 31 个字符".into()); }
    if name.chars().any(|c| matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')) {
        return Err("sheet 名不能含 [ ] : * ? / \\".into());
    }
    if wb.sheets.len() >= 255 { return Err("sheet 数量已达上限".into()); }
    if wb.sheets.iter().any(|s| s.name.to_lowercase() == name.to_lowercase()) {
        return Err("sheet 名已存在".into());
    }
    Ok(name)
}

/// 新建透视表 sheet。config_json: {"rows":[],"cols":[],"values":[],"agg":"sum","filters":{字段:[允许值]}}
/// filters 的值为空数组 = 仅作页字段、不限制行(与 pivot_set_config 的 filters 列表形状不同)。
/// 成功返回新 sheet 索引。
pub(crate) fn create_impl(
    wb: &mut Workbook,
    source_idx: i64,
    config_json: &str,
    name: &str,
) -> Result<usize, String> {
    let n = wb.sheets.len();
    if source_idx < 0 || source_idx as usize >= n {
        return Err(format!("pivot_create: source_sheet={source_idx} 越界(当前共 {n} 个 sheet)"));
    }
    let src = source_idx as usize;
    if wb.sheets[src].pivot.is_some() {
        return Err("pivot_create: 数据源不能是透视表 sheet".into());
    }
    let name = check_new_sheet_name(wb, name).map_err(|m| format!("pivot_create: {m}"))?;
    let mut cfg: roe_polars_bridge::PivotConfig = serde_json::from_str(config_json)
        .map_err(|e| format!("pivot_create: config_json 解析失败: {e}"))?;
    cfg.agg = match cfg.agg.as_str() {
        "" | "sum" => "sum".to_string(),
        "mean" | "avg" | "average" => "mean".to_string(),
        "count" | "min" | "max" => cfg.agg.clone(),
        other => return Err(format!("pivot_create: agg={other} 不支持(只允许 sum/mean/count/min/max)")),
    };
    if cfg.values.is_empty() {
        return Err("pivot_create: values 不能为空".into());
    }
    let df = roe_polars_bridge::sheet_to_dataframe(&wb.sheets[src])
        .map_err(|e| format!("pivot_create: {e}"))?;
    let names: Vec<String> = df.get_column_names().iter().map(|x| x.to_string()).collect();
    let mut seen = std::collections::HashSet::new();
    let all = cfg.rows.iter().chain(cfg.cols.iter()).chain(cfg.values.iter()).chain(cfg.filters.keys());
    for f in all {
        if !names.contains(f) {
            return Err(format!("pivot_create: 字段 {f} 不在数据源列中(可用列: {})", names.join("、")));
        }
        if !seen.insert(f.clone()) {
            return Err(format!("pivot_create: 字段 {f} 重复出现在多个区域"));
        }
    }
    roe_polars_bridge::pivot_sheet_into_workbook_v2(wb, src, &cfg, name)
        .map_err(|e| format!("pivot_create: {e}"))
}

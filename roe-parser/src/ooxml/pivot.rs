//! xlsx 透视表解析：pivotTable.xml + pivotCacheDefinition.xml → PivotMeta

use quick_xml::events::Event;
use quick_xml::Reader;
use roe_model::calc::{PivotFieldDef, PivotMeta};
use crate::pivot_expand::{expand_to_tree, CompressedItem};

/// 从 pivotCacheDefinition XML 读取字段名和 sharedItems
/// 返回 (field_names, shared_items)：
///   field_names[i] = 第 i 个字段的名称
///   shared_items[i] = 第 i 个字段的取值字符串列表
pub fn parse_cache_definition(xml: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let (field_names, shared_items, _source_sheet, _source_ref) = parse_cache_definition_full(xml);
    (field_names, shared_items)
}

/// 完整版：额外返回数据源 sheet 名和范围（worksheetSource ref/sheet 属性）
pub fn parse_cache_definition_full(xml: &str) -> (Vec<String>, Vec<Vec<String>>, Option<String>, Option<String>) {
    let mut field_names: Vec<String> = Vec::new();
    let mut shared_items: Vec<Vec<String>> = Vec::new();
    let mut source_sheet: Option<String> = None;
    let mut source_ref: Option<String> = None;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_shared_items = false;
    let mut current: Vec<String> = Vec::new();
    let mut cur_name = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                match e.name().as_ref() {
                    b"worksheetSource" => {
                        source_sheet = e.attributes()
                            .filter_map(|a| a.ok())
                            .find(|a| a.key.as_ref() == b"sheet")
                            .map(|a| String::from_utf8_lossy(&a.value).into_owned());
                        source_ref = e.attributes()
                            .filter_map(|a| a.ok())
                            .find(|a| a.key.as_ref() == b"ref")
                            .map(|a| String::from_utf8_lossy(&a.value).into_owned());
                    }
                    b"cacheField" => {
                        cur_name = e.attributes()
                            .filter_map(|a| a.ok())
                            .find(|a| a.key.as_ref() == b"name")
                            .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                            .unwrap_or_default();
                        current = Vec::new();
                    }
                    b"sharedItems" => { in_shared_items = true; }
                    // ROOT-057修复：sharedItems 子项按值类型用不同标签名
                    // （s=字符串, n=数字, b=布尔, d=日期），此前只处理 <s>，
                    // 纯数字字段（如整数列）的 <n> 标签被忽略，导致该字段
                    // shared_items 为空，resolve_items 查值失败退化为 "[idx]" 占位符。
                    b"s" | b"n" | b"b" | b"d" if in_shared_items => {
                        let v = e.attributes()
                            .filter_map(|a| a.ok())
                            .find(|a| a.key.as_ref() == b"v")
                            .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                            .unwrap_or_default();
                        current.push(v);
                    }
                    // ROOT-062: <m/> 表示该 sharedItems 位置是"空白/missing"值，没有 v 属性。
                    // 此前完全未处理该标签，导致该字段所有 sharedItems 的索引整体错位一位
                    // （例如 <m/><s v="a"/><s v="b"/>... 场景下，真正的空白位置被"a"顶替，
                    // pivotFields.items 里通过 x 索引指向该空白位置时，会错误显示为"a"）。
                    // 修复：push 空字符串占位，保持索引与文件中的物理顺序一致。
                    // 注意：这里必须用空字符串而非"(空白)"字面文案 —— roe-polars-bridge
                    // 的 compute_pivot_data_matrix 里 AnyValue::Null 转 row_path/col_path
                    // 字符串时用的也是 String::new()（见 anyvalue_to_display_string），
                    // 两边 key 必须逐字一致才能在 PivotDataMatrix 里查到值（ROOT-059 同类教训）。
                    // "(空白)"文案改在 tile.rs 渲染绘制文字时按需替换显示，不影响数据匹配。
                    b"m" if in_shared_items => {
                        current.push(String::new());
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                match e.name().as_ref() {
                    b"sharedItems" => { in_shared_items = false; }
                    b"cacheField" => {
                        field_names.push(cur_name.clone());
                        shared_items.push(current.clone());
                        cur_name.clear();
                        current.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
    }
    (field_names, shared_items, source_sheet, source_ref)
}

/// 从 pivotTable XML 解析透视表结构，结合 cache_fields 构建 PivotMeta
/// 解析 xlsx cell ref 范围字符串（如 "A4:B8" 或 "A4"）的左上角坐标，
/// 返回 (row, col)，均为 0-indexed。解析失败返回 None。
fn parse_cell_ref_top_left(ref_str: &str) -> Option<(u32, u32)> {
    let first = ref_str.split(':').next()?;
    let mut col_part = String::new();
    let mut row_part = String::new();
    for ch in first.chars() {
        if ch.is_ascii_alphabetic() {
            col_part.push(ch.to_ascii_uppercase());
        } else if ch.is_ascii_digit() {
            row_part.push(ch);
        }
    }
    if col_part.is_empty() || row_part.is_empty() {
        return None;
    }
    let mut col: u32 = 0;
    for ch in col_part.chars() {
        col = col * 26 + (ch as u32 - 'A' as u32 + 1);
    }
    let col = col.saturating_sub(1);
    let row: u32 = row_part.parse().ok()?;
    let row = row.saturating_sub(1);
    Some((row, col))
}

fn parse_cell_ref_bottom_right(ref_str: &str) -> Option<(u32, u32)> {
    let last = ref_str.split(':').last()?;
    let mut col_part = String::new();
    let mut row_part = String::new();
    for ch in last.chars() {
        if ch.is_ascii_alphabetic() {
            col_part.push(ch.to_ascii_uppercase());
        } else if ch.is_ascii_digit() {
            row_part.push(ch);
        }
    }
    if col_part.is_empty() || row_part.is_empty() {
        return None;
    }
    let mut col: u32 = 0;
    for ch in col_part.chars() {
        col = col * 26 + (ch as u32 - 'A' as u32 + 1);
    }
    let col = col.saturating_sub(1);
    let row: u32 = row_part.parse().ok()?;
    let row = row.saturating_sub(1);
    Some((row, col))
}

/// v156：B路径接线专用 —— 从 pivotTable.xml 提取"透视表配置"（字段轴分类、
/// 值字段聚合方式），不构建 row_tree/col_tree（那部分依赖有bug的
/// expand_to_tree，见v153 3.3节，A路径整体禁用后不再使用）。
///
/// 复用 parse_pivot_table 里 pivotFields/rowFields/colFields/pageFields/
/// dataFields 的解析逻辑，但完全跳过 rowItems/colItems/<i>/<x> 解析——
/// 那部分是树构建专属输入，本函数不需要。
///
/// 返回值供调用方（xlsx.rs）转换为 pivot-engine::PivotConfig，本函数
/// 不直接依赖 pivot-engine（保持 roe-parser 不跨层依赖）。
pub struct PivotConfigExtract {
    pub row_fields: Vec<String>,
    pub col_fields: Vec<String>,
    /// (source_column, agg) —— source_column 优先取 PivotFieldDef 的
    /// source_field 语义（即 field_names[fld]，dataField fld 索引直接
    /// 映射的真实列名），agg 是 xlsx dataField subtotal 原始值
    /// （如 "sum"/"average"/"countNums"），调用方需按 ROOT-057/既有
    /// xlsx.rs 映射表转换为 ROE 内部聚合命名。
    pub value_fields: Vec<(String, String)>,
    /// 筛选字段名列表（不含用户已选中的具体筛选值 —— 项目当前未实现
    /// 透视表筛选功能，本函数只收集字段名，filters 由调用方传空处理）。
    pub filter_fields: Vec<String>,
}

/// 从 pivotTable.xml 读取透视表名称与样式名。
/// 返回 (pivotTableDefinition@name, pivotTableStyleInfo@name),缺失为 None。
pub fn parse_pivot_table_info(xml: &str) -> (Option<String>, Option<String>) {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut table_name: Option<String> = None;
    let mut style_name: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = e.name().as_ref().to_vec();
                let is_def = tag.as_slice() == b"pivotTableDefinition";
                let is_style = tag.as_slice() == b"pivotTableStyleInfo";
                if is_def || is_style {
                    let val = e.attributes()
                        .filter_map(|a| a.ok())
                        .find(|a| a.key.as_ref() == b"name")
                        .map(|a| a.unescape_value()
                            .map(|c| c.into_owned())
                            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned()));
                    if is_def { table_name = val; } else { style_name = val; }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    (table_name, style_name)
}

pub fn extract_pivot_config(xml: &str, field_names: &[String]) -> Option<PivotConfigExtract> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut row_field_indices: Vec<usize> = Vec::new();
    let mut col_field_indices: Vec<usize> = Vec::new();
    let mut page_field_indices: Vec<usize> = Vec::new();
    let mut data_field_names: Vec<(usize, String, String)> = Vec::new(); // (fld index, name, subtotal)

    let mut in_row_fields = false;
    let mut in_col_fields = false;
    let mut in_page_fields = false;
    let mut in_data_fields = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name().as_ref().to_vec();
                let attrs: Vec<_> = e.attributes().filter_map(|a| a.ok()).collect();
                let get = |k: &[u8]| -> String {
                    attrs.iter().find(|a| a.key.as_ref() == k)
                        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                        .unwrap_or_default()
                };

                match name.as_slice() {
                    b"rowFields" => { in_row_fields = true; }
                    b"colFields" => { in_col_fields = true; }
                    b"pageFields" => { in_page_fields = true; }
                    b"field" if in_row_fields => {
                        let x: i32 = get(b"x").parse().unwrap_or(-1);
                        if x >= 0 { row_field_indices.push(x as usize); }
                    }
                    b"field" if in_col_fields => {
                        let x: i32 = get(b"x").parse().unwrap_or(-1);
                        if x >= 0 { col_field_indices.push(x as usize); }
                    }
                    b"pageField" if in_page_fields => {
                        let fld: i32 = get(b"fld").parse().unwrap_or(-1);
                        if fld >= 0 { page_field_indices.push(fld as usize); }
                    }
                    b"dataFields" => { in_data_fields = true; }
                    b"dataField" if in_data_fields => {
                        let name_str = get(b"name");
                        let fld: usize = get(b"fld").parse().unwrap_or(0);
                        let subtotal = get(b"subtotal");
                        let subtotal = if subtotal.is_empty() { "sum".to_string() } else { subtotal };
                        data_field_names.push((fld, name_str, subtotal));
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                match e.name().as_ref() {
                    b"rowFields" => { in_row_fields = false; }
                    b"colFields" => { in_col_fields = false; }
                    b"pageFields" => { in_page_fields = false; }
                    b"dataFields" => { in_data_fields = false; }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    let row_fields: Vec<String> = row_field_indices.iter()
        .map(|&i| field_names.get(i).cloned().unwrap_or_else(|| format!("field_{i}")))
        .collect();
    let col_fields: Vec<String> = col_field_indices.iter()
        .map(|&i| field_names.get(i).cloned().unwrap_or_else(|| format!("field_{i}")))
        .collect();
    let filter_fields: Vec<String> = page_field_indices.iter()
        .map(|&i| field_names.get(i).cloned().unwrap_or_else(|| format!("field_{i}")))
        .collect();
    let value_fields: Vec<(String, String)> = data_field_names.iter()
        .map(|(fld, _name, subtotal)| {
            let source_column = field_names.get(*fld).cloned().unwrap_or_else(|| format!("field_{fld}"));
            (source_column, subtotal.clone())
        })
        .collect();

    if row_fields.is_empty() && col_fields.is_empty() && value_fields.is_empty() {
        return None;
    }

    Some(PivotConfigExtract { row_fields, col_fields, value_fields, filter_fields })
}

pub fn parse_pivot_table(xml: &str, field_names: &[String], cache_fields: &[Vec<String>]) -> Option<PivotMeta> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    // 字段名列表（从 cacheDefinition 的 cacheField name 属性读，这里用索引对应）
    // 我们从 pivotFields 读 axis 来分类，从 dataFields 读 name
    let mut pivot_field_axes: Vec<String> = Vec::new(); // "page"/"row"/"col"/"data"/""
    let mut data_field_names: Vec<(usize, String, String)> = Vec::new(); // (fld index, name, subtotal)
    let mut row_field_indices: Vec<usize> = Vec::new();
    let mut col_field_indices: Vec<usize> = Vec::new();
    let mut page_field_indices: Vec<usize> = Vec::new();

    // rowItems / colItems
    let mut row_items: Vec<CompressedItem> = Vec::new();
    let mut col_items: Vec<CompressedItem> = Vec::new();

    // 解析上下文
    let mut in_pivot_fields = false;
    let mut in_row_fields = false;
    let mut in_col_fields = false;
    let mut in_row_items = false;
    let mut in_col_items = false;
    let mut in_data_fields = false;
    let mut in_page_fields = false;

    // 当前 <i> 解析状态
    let mut cur_item: Option<CompressedItem> = None;
    // pivotFields 里当前字段的 items 顺序表（索引→sharedItems索引）
    let mut cur_field_item_order: Vec<usize> = Vec::new();
    // 各字段的 items 顺序表
    let mut field_item_orders: Vec<Vec<usize>> = Vec::new();
    let mut in_pivot_field_items = false;
    let mut cur_axis = String::new();
    let mut base_row: u32 = 0;
    let mut base_col: u32 = 0;
    let mut last_row: u32 = 0;
    let mut last_col: u32 = 0;
    let mut row_grand_totals: bool = true;
    let mut col_grand_totals: bool = true;
    // ROOT-060: 值字段展开方向，默认false（展开在列方向，即当前已支持的场景）
    let mut data_on_rows: bool = false;

    loop {
        let evt = reader.read_event();
        // ROOT-064: <pivotField .../> 自闭合标签（如 case6 的 C 字段 dataField="1"，
        // 没有 <items> 子元素）不会触发 Event::End，导致该字段永远不会被 push 进
        // field_item_orders，使后续字段的下标整体错位一位（本应是 field_item_orders[3]
        // 的 D 字段顺序表被错误存到了 field_item_orders[2]，导致 resolve_items 查询
        // D 字段时用 field_idx=3 查出界外/错位的内容，case6 三层嵌套场景数值错位的根因）。
        // 修复：显式判断这次事件是否为 Empty，若是且当前标签是 pivotField，在处理完
        // Start 语义后立即补做 End 语义本该做的 push，不依赖不会到来的 End 事件。
        let is_empty_event = matches!(evt, Ok(Event::Empty(_)));
        match evt {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name().as_ref().to_vec();
                let attrs: Vec<_> = e.attributes().filter_map(|a| a.ok()).collect();
                let get = |k: &[u8]| -> String {
                    attrs.iter().find(|a| a.key.as_ref() == k)
                        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                        .unwrap_or_default()
                };

                match name.as_slice() {
                    b"pivotTableDefinition" => {
                        let rgt = get(b"rowGrandTotals");
                        let cgt = get(b"colGrandTotals");
                        if rgt == "0" { row_grand_totals = false; }
                        if cgt == "0" { col_grand_totals = false; }
                        // ROOT-060: dataOnRows属性标记值字段(dataFields)展开方向。
                        // "1"=展开在行方向（此时rowFields里会有field x="-2"占位，
                        // 真实分组字段都在colFields）；缺省或"0"=展开在列方向（默认，
                        // 已有col_header_rows/渲染逻辑按此假设实现）。
                        let dor = get(b"dataOnRows");
                        if dor == "1" { data_on_rows = true; }
                    }
                    b"location" => {
                        let ref_str = get(b"ref");
                        if let Some((r, c)) = parse_cell_ref_top_left(&ref_str) {
                            base_row = r;
                            base_col = c;
                        }
                        if let Some((r, c)) = parse_cell_ref_bottom_right(&ref_str) {
                            last_row = r;
                            last_col = c;
                        }
                    }
                    b"pivotFields" => { in_pivot_fields = true; }
                    b"pivotField" if in_pivot_fields => {
                        cur_axis = get(b"axis");
                        cur_field_item_order = Vec::new();
                        if is_empty_event {
                            // 自闭合 <pivotField .../>：不会有 End 事件，立即补做
                            // push，避免 field_item_orders 少一个元素导致后续
                            // 字段下标整体错位（ROOT-064）。
                            pivot_field_axes.push(cur_axis.clone());
                            field_item_orders.push(cur_field_item_order.clone());
                            cur_axis.clear();
                        }
                    }
                    b"items" if in_pivot_fields => { in_pivot_field_items = true; }
                    b"item" if in_pivot_field_items => {
                        let t = get(b"t");
                        if t.is_empty() {
                            let x: usize = get(b"x").parse().unwrap_or(0);
                            cur_field_item_order.push(x);
                        }
                        // t="default" 是小计占位，跳过
                    }
                    b"rowFields" => { in_row_fields = true; }
                    b"colFields" => { in_col_fields = true; }
                    b"pageFields" => { in_page_fields = true; }
                    b"field" if in_row_fields => {
                        let x: i32 = get(b"x").parse().unwrap_or(-1);
                        if x >= 0 { row_field_indices.push(x as usize); }
                    }
                    b"field" if in_col_fields => {
                        let x: i32 = get(b"x").parse().unwrap_or(-1);
                        if x >= 0 { col_field_indices.push(x as usize); }
                        // x=-2 是数据字段层，忽略（已通过 dataFields 处理）
                    }
                    b"pageField" if in_page_fields => {
                        let fld: i32 = get(b"fld").parse().unwrap_or(-1);
                        if fld >= 0 { page_field_indices.push(fld as usize); }
                    }
                    b"rowItems" => { in_row_items = true; }
                    b"colItems" => { in_col_items = true; }
                    b"i" if in_row_items || in_col_items => {
                        let r: usize = get(b"r").parse().unwrap_or(0);
                        let t = get(b"t");
                        let is_subtotal = t == "default" || t == "grand";
                        cur_item = Some(CompressedItem { r, values: Vec::new(), is_subtotal });
                    }
                    b"x" if cur_item.is_some() => {
                        // ROOT-062(子问题2) + ROOT-063: <x/> 空标签（无 v 属性）在 OOXML 规范里
                        // 分两种语义，必须按 <i> 是否为 subtotal/grand 行区分处理：
                        //   1) 普通数据行（is_subtotal=false）：空 <x/> 按规范默认值 v="0" 处理，
                        //      这是官方允许的省略写法（如 case5 的 <i><x/></i> 表示 A 字段第0项），
                        //      必须 push 索引 "0"，否则会丢失该字段的合法索引，导致
                        //      row_tree/col_tree 少掉整条路径（ROOT-063 case5/6/14 回归的根因）。
                        //   2) subtotal/grand 行（is_subtotal=true，如 t="grand" 的 <i t="grand">
                        //      <x/></i>）：空 <x/> 是纯占位，没有实际索引语义，此前
                        //      get(b"v").parse().unwrap_or(0) 会把它错误退化成索引 "0" 并 push，
                        //      导致 expand_to_tree 把这条纯占位记录当成一条完整数据路径插入，
                        //      凭空多出幽灵节点（ROOT-062 子问题2 case10 的根因）。这种情况必须
                        //      跳过，不 push。
                        let has_v = attrs.iter().any(|a| a.key.as_ref() == b"v");
                        let is_subtotal_row = cur_item.as_ref().map(|it| it.is_subtotal).unwrap_or(false);
                        if has_v {
                            let v: usize = get(b"v").parse().unwrap_or(0);
                            if let Some(ref mut item) = cur_item {
                                item.values.push(v.to_string()); // 暂存索引字符串，后面转换
                            }
                        } else if !is_subtotal_row {
                            // 普通行的空 <x/>，按规范默认索引 0 处理
                            if let Some(ref mut item) = cur_item {
                                item.values.push("0".to_string());
                            }
                        }
                        // else: subtotal/grand 行的空 <x/>，纯占位，跳过不 push
                    }
                    b"dataFields" => { in_data_fields = true; }
                    b"dataField" if in_data_fields => {
                        let name_str = get(b"name");
                        let fld: usize = get(b"fld").parse().unwrap_or(0);
                        let subtotal = get(b"subtotal");
                        let subtotal = if subtotal.is_empty() { "sum".to_string() } else { subtotal };
                        data_field_names.push((fld, name_str, subtotal));
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                match e.name().as_ref() {
                    b"pivotFields" => { in_pivot_fields = false; }
                    b"pivotField" if in_pivot_fields => {
                        pivot_field_axes.push(cur_axis.clone());
                        field_item_orders.push(cur_field_item_order.clone());
                        cur_axis.clear();
                    }
                    b"items" if in_pivot_fields => { in_pivot_field_items = false; }
                    b"rowFields" => { in_row_fields = false; }
                    b"colFields" => { in_col_fields = false; }
                    b"pageFields" => { in_page_fields = false; }
                    b"rowItems" => { in_row_items = false; }
                    b"colItems" => { in_col_items = false; }
                    b"dataFields" => { in_data_fields = false; }
                    b"i" if !in_pivot_fields => {
                        if let Some(item) = cur_item.take() {
                            if in_row_items { row_items.push(item); }
                            else if in_col_items { col_items.push(item); }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    // 把 items 里的索引字符串转换成实际值字符串
    // row/col items 的 values 是 sharedItems 的索引，但要先通过 field_item_orders 间接查
    // field_item_orders[field_idx][order_pos] = sharedItems_idx
    // rowItems 各层对应的字段索引 = row_field_indices[layer]
    let resolve_items = |items: &mut Vec<CompressedItem>, field_indices: &[usize]| {
        let depth = field_indices.len();
        for item in items.iter_mut() {
            for (layer_offset, v_str) in item.values.iter_mut().enumerate() {
                let layer = item.r + layer_offset;
                if layer >= depth { break; }
                let field_idx = field_indices[layer];
                let order_idx: usize = v_str.parse().unwrap_or(0);
                // 通过 field_item_orders 找到 sharedItems 索引
                let shared_idx = field_item_orders.get(field_idx)
                    .and_then(|ord| ord.get(order_idx))
                    .copied()
                    .unwrap_or(order_idx);
                // 从 cache_fields 取实际字符串
                *v_str = cache_fields.get(field_idx)
                    .and_then(|f| f.get(shared_idx))
                    .cloned()
                    .unwrap_or_else(|| format!("[{shared_idx}]"));
            }
        }
    };

    let mut row_items_resolved = row_items;
    let mut col_items_resolved = col_items;
    resolve_items(&mut row_items_resolved, &row_field_indices);
    resolve_items(&mut col_items_resolved, &col_field_indices);

    // 构建树
    let row_tree = expand_to_tree(&row_items_resolved, row_field_indices.len());
    let col_tree = expand_to_tree(&col_items_resolved, col_field_indices.len());

    // 字段名
    let row_field_names: Vec<String> = row_field_indices.iter()
        .map(|&i| field_names.get(i).cloned().unwrap_or_else(|| format!("field_{i}")))
        .collect();
    let col_field_names: Vec<String> = col_field_indices.iter()
        .map(|&i| field_names.get(i).cloned().unwrap_or_else(|| format!("field_{i}")))
        .collect();
    let filter_fields: Vec<PivotFieldDef> = page_field_indices.iter()
        .map(|&i| PivotFieldDef::new(field_names.get(i).cloned().unwrap_or_else(|| format!("field_{i}"))))
        .collect();
    let value_fields: Vec<PivotFieldDef> = data_field_names.iter()
        .map(|(fld, name, subtotal)| {
            let mut def = PivotFieldDef::new(name.clone());
            def.agg = Some(subtotal.clone());
            // ROOT-057: fld 是原始 pivotFields 索引，对应真实数据源列名。
            // 不能从 name（展示标签，如 "Sum of C"/"求和项:C"）反推列名，
            // 格式因 Excel 语言环境而异，字符串解析不可靠。
            def.source_field = field_names.get(*fld).cloned();
            def
        })
        .collect();

    let filter_row_count = filter_fields.len() as u32;
    // ROOT-061: "值"标题行只在存在 col_field_names 时才需要（多值字段横向展开在列头里）。
    // 无 col_fields、仅 value_fields.len()>1 的场景（如 1row2col，无列字段）不应多出这一行，
    // 表头应直接是 "Row Labels | Sum of B | Sum of C" 一行。
    let col_header_rows = col_field_names.len() as u32
        + if value_fields.len() > 1 && !col_field_names.is_empty() { 1 } else { 0 };
    // ROOT-060: dataOnRows=true 时，"值/Values"标题行紧跟数据行（Sum of C/Sum of D），
    // 中间不需要额外空行（"+1空行"是为dataOnCols场景设计，dataOnRows场景会导致多余空行）。
    let data_row_start = if data_on_rows {
        filter_row_count + col_header_rows
    } else {
        filter_row_count + 1 + col_header_rows // +1 空行
    };
    let data_col_start = (row_field_names.len() as u32).max(1); // 至少留1列给角落标题格

    let mut meta = PivotMeta {
        source_sheet: String::new(), // xlsx.rs 调用时填入
        agg: "sum".into(),
        filter_fields,
        row_field_names,
        col_field_names,
        value_fields,
        base_row,
        base_col,
        last_row,
        last_col,
        col_tree,
        row_tree,
        data_row_start,
        data_col_start,
        row_grand_totals,
        col_grand_totals,
        data_on_rows,
        ..Default::default()
    };
    meta.compute_spans();
    Some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cache_definition() {
        let xml = r#"<?xml version="1.0"?>
<pivotCacheDefinition xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<cacheFields count="3">
<cacheField name="Color"><sharedItems count="3"><s v="红"/><s v="蓝"/><s v="绿"/></sharedItems></cacheField>
<cacheField name="Size"><sharedItems count="2"><s v="大"/><s v="小"/></sharedItems></cacheField>
<cacheField name="Sales"><sharedItems containsNumber="1"></sharedItems></cacheField>
</cacheFields>
</pivotCacheDefinition>"#;
        let fields = parse_cache_definition(xml);
        assert_eq!(fields.1[0], vec!["红", "蓝", "绿"]);
        assert_eq!(fields.1[1], vec!["大", "小"]);
        assert_eq!(fields.1[2], Vec::<String>::new());
    }
}

// v154：integration_tests 专测A路径（open_xlsx_bytes触发的静态快照解析），
// A路径已禁用（见 xlsx.rs 对应注释），此模块暂时禁用避免测试失败。
// 原代码见 roe-parser/src/ooxml/pivot.rs.bak_before_disable_pathA_v154
/*
#[cfg(test)]
mod integration_tests {
    fn dump_pivot(label: &str, path: &str) {
        println!("\n========== {} ==========", label);
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => { eprintln!("skip: {} not found", path); return; }
        };
        let doc = crate::RoeDocument::open_xlsx_bytes(&bytes).expect("parse xlsx");
        let wb = match doc { crate::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        let pivot_sheet = match wb.sheets.iter().find(|s| s.pivot.is_some()) {
            Some(s) => s,
            None => { println!("!! no pivot sheet found in {}", path); return; }
        };
        let pivot = pivot_sheet.pivot.as_ref().unwrap();
        println!("filter_fields: {:?}", pivot.filter_fields.iter().map(|f| &f.name).collect::<Vec<_>>());
        println!("row_fields: {:?}", pivot.row_field_names);
        println!("col_fields: {:?}", pivot.col_field_names);
        println!("value_fields: {:?}", pivot.value_fields.iter().map(|f| &f.name).collect::<Vec<_>>());
        println!("data_row_start={} data_col_start={} base_row={} base_col={} data_on_rows={}", pivot.data_row_start, pivot.data_col_start, pivot.base_row, pivot.base_col, pivot.data_on_rows);
        fn dump_node(n: &roe_model::calc::PivotHeaderNode, depth: usize, path: &mut Vec<String>) {
            path.push(n.value.clone());
            println!("{}[{}] path={:?} span={} outline_span={} sub={} ch={}",
                "  ".repeat(depth + 1), n.value, path, n.cached_span, n.outline_span, n.is_subtotal, n.children.len());
            for c in &n.children {
                dump_node(c, depth + 1, path);
            }
            path.pop();
        }
        println!("col_tree children={}", pivot.col_tree.children.len());
        for c in &pivot.col_tree.children {
            let mut path = Vec::new();
            dump_node(c, 0, &mut path);
        }
        println!("row_tree children={}", pivot.row_tree.children.len());
        for r in &pivot.row_tree.children {
            let mut path = Vec::new();
            dump_node(r, 0, &mut path);
        }
    }

    #[test]
    fn test_pivot_matrix() {
        dump_pivot("1row",              "/home/yong/pivot_tests/pivot_1row.xlsx");
        dump_pivot("1col",              "/home/yong/pivot_tests/pivot_1col.xlsx");
        dump_pivot("1row1col",          "/home/yong/pivot_tests/pivot_1row1col.xlsx");
        dump_pivot("2row",              "/home/yong/pivot_tests/pivot_2row.xlsx");
        dump_pivot("2col",              "/home/yong/pivot_tests/pivot_2col.xlsx");
        dump_pivot("2row2col",          "/home/yong/pivot_tests/pivot_2row2col.xlsx");
        dump_pivot("1row1col1filter",   "/home/yong/pivot_tests/pivot_1row1col1filter.xlsx");
        dump_pivot("full_2row2col2val", "/home/yong/pivot_tests/pivot_test.xlsx");
        dump_pivot("ROOT057_case1_DataFieldInRow",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_1_DataFieldInRow_RowLabelColor.xlsx");
        dump_pivot("ROOT057_case3_DataFieldInColumn",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_3_DataFieldInColumn_ColumnLabelColor.xlsx");
        dump_pivot("ROOT060_case8_DataFieldInRow_DataColor",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_8_DataFieldInRow_DataColor.xlsx");
        dump_pivot("v124_case2_2_DataFieldInRow_ColumnLabelColor",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_2_DataFieldInRow_ColumnLabelColor.xlsx");
        dump_pivot("v124_case4_4_DataFieldInColumn_DataColor",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_4_DataFieldInColumn_DataColor.xlsx");
        dump_pivot("v124_case5_5_DataFieldInColumnAndTwoRowFields_DataColor",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_5_DataFieldInColumnAndTwoRowFields_DataColor.xlsx");
        dump_pivot("v124_case6_6_SingleDataFieldInColumn_DataColor",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_6_SingleDataFieldInColumn_DataColor.xlsx");
        dump_pivot("v124_case7_7_TwoRowTwoColumnFields_DataColor",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_7_TwoRowTwoColumnFields_DataColor.xlsx");
        dump_pivot("v124_case9_9_MultipleSelections",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_9_MultipleSelections.xlsx");
        dump_pivot("v124_case10_10_FormatDefinitionNotMatchingPivotTable",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_10_FormatDefinitionNotMatchingPivotTable.xlsx");
        dump_pivot("v124_case11_11_WholeDataColumnSelected",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_11_WholeDataColumnSelected.xlsx");
        dump_pivot("v124_case12_12_WholeLabelColumnSelected",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_12_WholeLabelColumnSelected.xlsx");
        dump_pivot("v124_case13_13_SelectionInLabelAndData",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_13_SelectionInLabelAndData.xlsx");
        dump_pivot("v124_case14_14_TwoRowsDataFieldInColumn_LabelColor",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_14_TwoRowsDataFieldInColumn_LabelColor.xlsx");
        dump_pivot("v124_case15_15_TwoDataFieldColumns_WholeDataColumnSelected",
            "/home/yong/Projects/roe/test_samples/xlsx/sc_xlsx_pivot-table__PivotTableCellFormatsTest_15_TwoDataFieldColumns_WholeDataColumnSelected.xlsx");
        // v151探针：核实小计(is_subtotal)节点是否被正确解析出来
        dump_pivot("PROBE_Pivot1_Row_Grand_Subtotals",
            "/home/yong/roe_subtotal_probe/input/sc_xlsx_pivot__Pivot1_Row_Grand_Subtotals.xlsx");
        dump_pivot("PROBE_Pivot3_Column_Grand_Subtotals",
            "/home/yong/roe_subtotal_probe/input/sc_xlsx_pivot__Pivot3_Column_Grand_Subtotals.xlsx");
    }
}
*/

#[cfg(test)]
mod pivot_info_tests {
    use super::parse_pivot_table_info;

    #[test]
    fn pivot_info_reads_name_and_style() {
        let xml = r#"<?xml version="1.0"?><pivotTableDefinition xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" name="数据透视表1" cacheId="0"><location ref="A4:E9"/><pivotTableStyleInfo name="PivotStyleLight20" showRowHeaders="1"/></pivotTableDefinition>"#;
        let (n, st) = parse_pivot_table_info(xml);
        assert_eq!(n.as_deref(), Some("数据透视表1"));
        assert_eq!(st.as_deref(), Some("PivotStyleLight20"));
    }

    #[test]
    fn pivot_info_missing_is_none() {
        let xml = r#"<pivotTableDefinition cacheId="0"><location ref="A1:B2"/></pivotTableDefinition>"#;
        let (n, st) = parse_pivot_table_info(xml);
        assert_eq!(n, None);
        assert_eq!(st, None);
    }
}

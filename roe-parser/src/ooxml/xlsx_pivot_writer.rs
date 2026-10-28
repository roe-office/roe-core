//! xlsx 透视表部件写出(纯函数,不依赖 Workbook)。
//!
//! 方案 A:不写 pivotCacheRecords;cacheDefinition 置 saveData=0 / refreshOnLoad=1,
//! pivotTable 不写 rowItems/colItems,依赖打开时刷新重算。静态 cells 由 xlsx_writer 照常写出。
//! 轴字段(行/列/筛选)限文本类,数值类轴字段与非法输入一律返回 None,调用方降级为普通表格。

pub struct PivotFieldSpec {
    pub name: String,
    /// 仅轴字段需要:去重后的取值,顺序即 sharedItems 顺序
    pub items: Vec<String>,
    /// 数值字段(值字段)置 true,写成数值型 sharedItems
    pub numeric: bool,
}

pub struct PivotDataSpec {
    /// 展示名,如 "Sales的Sum"
    pub name: String,
    /// 源字段在 fields 中的索引
    pub fld: usize,
    /// ROE 聚合名:sum/mean/count/min/max
    pub agg: String,
}

pub struct PivotWriteSpec {
    pub name: String,
    pub cache_id: u32,
    pub source_sheet: String,
    pub source_ref: String,
    pub fields: Vec<PivotFieldSpec>,
    pub row_fields: Vec<usize>,
    pub col_fields: Vec<usize>,
    pub page_fields: Vec<usize>,
    pub data_fields: Vec<PivotDataSpec>,
    pub data_on_rows: bool,
    /// location ref,如 "A3:E8"
    pub location_ref: String,
    pub style_name: String,
    /// xlsx 属性 rowGrandTotals / colGrandTotals 的值(调用方负责从 PivotMeta 映射)
    pub row_grand_totals: bool,
    pub col_grand_totals: bool,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn spec_valid(sp: &PivotWriteSpec) -> bool {
    let n = sp.fields.len();
    if n == 0 || sp.data_fields.is_empty() {
        return false;
    }
    let axis_ok = |v: &Vec<usize>| v.iter().all(|&i| i < n && !sp.fields[i].numeric);
    if !axis_ok(&sp.row_fields) || !axis_ok(&sp.col_fields) || !axis_ok(&sp.page_fields) {
        return false;
    }
    sp.data_fields.iter().all(|d| d.fld < n)
}

pub fn subtotal_attr(agg: &str) -> Option<&'static str> {
    match agg {
        "mean" => Some("average"),
        "count" => Some("count"),
        "min" => Some("min"),
        "max" => Some("max"),
        _ => None, // sum 为缺省,不写
    }
}

/// xl/pivotCache/pivotCacheDefinitionN.xml(无 records,无 r:id)
pub fn cache_definition_xml(sp: &PivotWriteSpec) -> Option<String> {
    if !spec_valid(sp) {
        return None;
    }
    let mut cf = String::new();
    for f in &sp.fields {
        let si = if f.numeric {
            r#"<sharedItems containsSemiMixedTypes="0" containsString="0" containsNumber="1"/>"#.to_string()
        } else if f.items.is_empty() {
            "<sharedItems/>".to_string()
        } else {
            let mut s = format!(r#"<sharedItems count="{}">"#, f.items.len());
            for it in &f.items {
                s.push_str(&format!(r#"<s v="{}"/>"#, esc(it)));
            }
            s.push_str("</sharedItems>");
            s
        };
        cf.push_str(&format!(r#"<cacheField name="{}" numFmtId="0">{}</cacheField>"#, esc(&f.name), si));
    }
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<pivotCacheDefinition xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" saveData=\"0\" refreshOnLoad=\"1\" createdVersion=\"4\" refreshedVersion=\"4\" minRefreshableVersion=\"3\"><cacheSource type=\"worksheet\"><worksheetSource ref=\"{}\" sheet=\"{}\"/></cacheSource><cacheFields count=\"{}\">{}</cacheFields></pivotCacheDefinition>",
        esc(&sp.source_ref), esc(&sp.source_sheet), sp.fields.len(), cf
    ))
}

/// xl/pivotTables/pivotTableN.xml
pub fn pivot_table_xml(sp: &PivotWriteSpec) -> Option<String> {
    if !spec_valid(sp) {
        return None;
    }
    let n_data = sp.data_fields.len();
    let mut pf = String::new();
    for (idx, f) in sp.fields.iter().enumerate() {
        let axis = if sp.row_fields.contains(&idx) {
            Some("axisRow")
        } else if sp.col_fields.contains(&idx) {
            Some("axisCol")
        } else if sp.page_fields.contains(&idx) {
            Some("axisPage")
        } else {
            None
        };
        let is_data = sp.data_fields.iter().any(|d| d.fld == idx);
        let mut attrs = String::new();
        if let Some(a) = axis {
            attrs.push_str(&format!(r#" axis="{a}""#));
        }
        if is_data {
            attrs.push_str(r#" dataField="1""#);
        }
        attrs.push_str(r#" compact="0" showAll="0""#);
        if axis.is_some() && !f.items.is_empty() {
            let mut items = format!(r#"<items count="{}">"#, f.items.len() + 1);
            for i in 0..f.items.len() {
                items.push_str(&format!(r#"<item x="{i}"/>"#));
            }
            items.push_str(r#"<item t="default"/></items>"#);
            pf.push_str(&format!("<pivotField{attrs}>{items}</pivotField>"));
        } else {
            pf.push_str(&format!("<pivotField{attrs}/>"));
        }
    }
    let multi_val = n_data > 1;
    let mut row_f: Vec<i64> = sp.row_fields.iter().map(|&i| i as i64).collect();
    let mut col_f: Vec<i64> = sp.col_fields.iter().map(|&i| i as i64).collect();
    if multi_val {
        if sp.data_on_rows { row_f.push(-2); } else { col_f.push(-2); }
    }
    let fields_xml = |tag: &str, v: &Vec<i64>| -> String {
        if v.is_empty() {
            return String::new();
        }
        let mut s = format!(r#"<{tag} count="{}">"#, v.len());
        for x in v {
            s.push_str(&format!(r#"<field x="{x}"/>"#));
        }
        s.push_str(&format!("</{tag}>"));
        s
    };
    let mut page_xml = String::new();
    if !sp.page_fields.is_empty() {
        page_xml.push_str(&format!(r#"<pageFields count="{}">"#, sp.page_fields.len()));
        for &i in &sp.page_fields {
            page_xml.push_str(&format!(r#"<pageField fld="{i}" hier="-1"/>"#));
        }
        page_xml.push_str("</pageFields>");
    }
    let mut df = format!(r#"<dataFields count="{n_data}">"#);
    for d in &sp.data_fields {
        let sub = subtotal_attr(&d.agg).map(|s| format!(r#" subtotal="{s}""#)).unwrap_or_default();
        df.push_str(&format!(
            r#"<dataField name="{}" fld="{}"{} baseField="0" baseItem="0"/>"#,
            esc(&d.name), d.fld, sub
        ));
    }
    df.push_str("</dataFields>");
    let levels = sp.col_fields.len() + if multi_val && !sp.data_on_rows { 1 } else { 0 };
    let first_data_row = if levels == 0 { 1 } else { levels + 1 };
    let first_data_col = sp.row_fields.len().max(1);
    let mut totals = String::new();
    if !sp.row_grand_totals {
        totals.push_str(r#" rowGrandTotals="0""#);
    }
    if !sp.col_grand_totals {
        totals.push_str(r#" colGrandTotals="0""#);
    }
    let on_rows = if sp.data_on_rows && multi_val { r#" dataOnRows="1""# } else { "" };
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<pivotTableDefinition xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" name=\"{}\" cacheId=\"{}\" applyNumberFormats=\"0\" applyBorderFormats=\"0\" applyFontFormats=\"0\" applyPatternFormats=\"0\" applyAlignmentFormats=\"0\" applyWidthHeightFormats=\"1\" dataCaption=\"值\" updatedVersion=\"4\" minRefreshableVersion=\"3\" useAutoFormatting=\"1\" itemPrintTitles=\"1\" createdVersion=\"4\" indent=\"0\" compact=\"0\" outline=\"1\" outlineData=\"1\" compactData=\"0\" multipleFieldFilters=\"0\"{}{}><location ref=\"{}\" firstHeaderRow=\"1\" firstDataRow=\"{}\" firstDataCol=\"{}\"/><pivotFields count=\"{}\">{}</pivotFields>{}{}{}{}<pivotTableStyleInfo name=\"{}\" showRowHeaders=\"1\" showColHeaders=\"1\" showRowStripes=\"0\" showColStripes=\"0\" showLastColumn=\"1\"/></pivotTableDefinition>",
        esc(&sp.name), sp.cache_id, totals, on_rows, esc(&sp.location_ref), first_data_row, first_data_col,
        sp.fields.len(), pf, fields_xml("rowFields", &row_f), fields_xml("colFields", &col_f), page_xml, df,
        esc(&sp.style_name)
    ))
}

const REL_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">";

/// xl/pivotTables/_rels/pivotTableN.xml.rels:指向 pivotCacheDefinitionN
pub fn pivot_table_rels_xml(cache_no: usize) -> String {
    format!(
        r#"{REL_HEAD}<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotCacheDefinition" Target="../pivotCache/pivotCacheDefinition{cache_no}.xml"/></Relationships>"#
    )
}

/// sheetN.xml.rels 中追加的一条关系(sheet -> pivotTable)
pub fn sheet_pivot_rel(rid: &str, table_no: usize) -> String {
    format!(
        r#"<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotTable" Target="../pivotTables/pivotTable{table_no}.xml"/>"#
    )
}

/// workbook.xml.rels 中追加的一条关系(workbook -> pivotCacheDefinition)
pub fn workbook_pivot_cache_rel(rid: &str, cache_no: usize) -> String {
    format!(
        r#"<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotCacheDefinition" Target="pivotCache/pivotCacheDefinition{cache_no}.xml"/>"#
    )
}

/// workbook.xml 的 <pivotCaches> 片段;entries = (cacheId, rId)。空则返回空串。
pub fn workbook_pivot_caches_xml(entries: &[(u32, String)]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut s = String::from("<pivotCaches>");
    for (id, rid) in entries {
        s.push_str(&format!(r#"<pivotCache cacheId="{id}" r:id="{rid}"/>"#));
    }
    s.push_str("</pivotCaches>");
    s
}

/// 在 [Content_Types].xml 末尾追加 n 组 pivotTable + pivotCacheDefinition 的 Override。
pub fn append_pivot_content_types(ct: String, n: usize) -> String {
    if n == 0 {
        return ct;
    }
    let mut add = String::new();
    for i in 1..=n {
        add.push_str(&format!(
            r#"<Override PartName="/xl/pivotTables/pivotTable{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.pivotTable+xml"/><Override PartName="/xl/pivotCache/pivotCacheDefinition{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.pivotCacheDefinition+xml"/>"#
        ));
    }
    match ct.rfind("</Types>") {
        Some(pos) => format!("{}{}{}", &ct[..pos], add, &ct[pos..]),
        None => ct,
    }
}

// ─── 从 Sheet/PivotMeta 构造写出规格(所有降级判断集中在此) ───────────────────

use roe_model::calc::{CellValue, Sheet};

/// 0-indexed 列号 -> 字母(0 -> A, 26 -> AA)
pub fn col_letters(c: u32) -> String {
    let mut n = c + 1;
    let mut s = String::new();
    while n > 0 {
        let r = (n - 1) % 26;
        s.insert(0, (b'A' + r as u8) as char);
        n = (n - 1) / 26;
    }
    s
}

fn parse_cell(s: &str) -> Option<(u32, u32)> {
    let letters: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits = &s[letters.len()..];
    if letters.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut col: u32 = 0;
    for ch in letters.chars() {
        col = col.checked_mul(26)?.checked_add(ch.to_ascii_uppercase() as u32 - 'A' as u32 + 1)?;
    }
    let row: u32 = digits.parse().ok()?;
    if row == 0 {
        return None;
    }
    Some((row - 1, col - 1))
}

/// "A1:H201"(允许 $)-> (r1, c1, r2, c2),0-indexed,含端点
pub fn parse_range(s: &str) -> Option<(u32, u32, u32, u32)> {
    let t = s.replace('$', "");
    let (a, b) = t.split_once(':')?;
    let (r1, c1) = parse_cell(a)?;
    let (r2, c2) = parse_cell(b)?;
    if r2 < r1 || c2 < c1 {
        return None;
    }
    Some((r1, c1, r2, c2))
}

fn range_ref(r1: u32, c1: u32, r2: u32, c2: u32) -> String {
    format!("{}{}:{}{}", col_letters(c1), r1 + 1, col_letters(c2), r2 + 1)
}

fn base_value(v: &CellValue) -> &CellValue {
    match v {
        CellValue::Formula { cached, .. } => base_value(cached.as_ref()),
        o => o,
    }
}

#[derive(Default, Clone, Copy)]
struct ColKind {
    has_num: bool,
    has_text: bool,
    has_other: bool,
}

impl ColKind {
    fn numeric(&self) -> bool {
        self.has_num && !self.has_text && !self.has_other
    }
    fn text_only(&self) -> bool {
        !self.has_num && !self.has_other
    }
}

/// 为 sheets[sheet_idx] 的透视表构造写出规格。ordinal 从 1 起,用作 cacheId 与缺省名后缀。
/// 任何一项不满足即返回 None,调用方把该 sheet 当普通表格保存。
pub fn build_spec(sheets: &[Sheet], sheet_idx: usize, ordinal: u32) -> Option<PivotWriteSpec> {
    let sheet = sheets.get(sheet_idx)?;
    let meta = sheet.pivot.as_ref()?;
    let src = sheets.iter().find(|s| s.name == meta.source_sheet)?;
    let (rr1, rc1, rr2, rc2) = meta.cells_rect?;

    let source_ref = match &meta.source_ref {
        Some(r) if !r.is_empty() => r.clone(),
        _ => {
            let (mr, mc) = src.cells.used_bounds()?;
            format!("A1:{}{}", col_letters(mc), mr + 1)
        }
    };
    let (r1, c1, r2, c2) = parse_range(&source_ref)?;
    if r2 <= r1 || c2 - c1 > 1000 {
        return None;
    }

    // 表头:非空、不重名
    let mut headers: Vec<String> = Vec::new();
    for c in c1..=c2 {
        let h = base_value(&src.cells.get(r1, c)).to_display_string();
        if h.trim().is_empty() || headers.contains(&h) {
            return None;
        }
        headers.push(h);
    }

    // 各列类型
    let mut kinds: Vec<ColKind> = Vec::new();
    for c in c1..=c2 {
        let mut k = ColKind::default();
        for r in (r1 + 1)..=r2 {
            match base_value(&src.cells.get(r, c)) {
                CellValue::Empty => {}
                CellValue::Number(_) => k.has_num = true,
                CellValue::Text(_) | CellValue::RichText(_) => k.has_text = true,
                _ => k.has_other = true,
            }
        }
        kinds.push(k);
    }

    let idx_of = |n: &str| headers.iter().position(|h| h == n);
    let row_fields = meta.row_field_names.iter().map(|n| idx_of(n)).collect::<Option<Vec<_>>>()?;
    let col_fields = meta.col_field_names.iter().map(|n| idx_of(n)).collect::<Option<Vec<_>>>()?;
    let page_fields = meta.filter_fields.iter().map(|f| idx_of(&f.name)).collect::<Option<Vec<_>>>()?;

    // 轴字段不得重复使用,且所在列必须是纯文本
    let mut axis_all: Vec<usize> = Vec::new();
    axis_all.extend(&row_fields);
    axis_all.extend(&col_fields);
    axis_all.extend(&page_fields);
    let mut uniq = axis_all.clone();
    uniq.sort_unstable();
    uniq.dedup();
    if uniq.len() != axis_all.len() || axis_all.iter().any(|&i| !kinds[i].text_only()) {
        return None;
    }

    // 值字段
    let mut used_names: Vec<String> = headers.clone();
    let mut data_fields: Vec<PivotDataSpec> = Vec::new();
    for v in &meta.value_fields {
        let fld = idx_of(v.source_field.as_ref()?)?;
        let agg = v.agg.clone().unwrap_or_else(|| meta.agg.clone());
        if agg != "count" && !kinds[fld].numeric() {
            return None;
        }
        let mut name = if v.name.is_empty() { format!("{}的值", headers[fld]) } else { v.name.clone() };
        while used_names.contains(&name) {
            name.push(' ');
        }
        used_names.push(name.clone());
        data_fields.push(PivotDataSpec { name, fld, agg });
    }
    if data_fields.is_empty() {
        return None;
    }

    // 字段规格:轴字段带去重值(源数据首次出现顺序)
    let mut fields: Vec<PivotFieldSpec> = Vec::new();
    for (i, h) in headers.iter().enumerate() {
        let mut items: Vec<String> = Vec::new();
        if axis_all.contains(&i) {
            for r in (r1 + 1)..=r2 {
                let v = base_value(&src.cells.get(r, c1 + i as u32)).to_display_string();
                if !v.is_empty() && !items.contains(&v) {
                    items.push(v);
                }
            }
        }
        fields.push(PivotFieldSpec { name: h.clone(), items, numeric: kinds[i].numeric() });
    }

    Some(PivotWriteSpec {
        name: meta.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("PivotTable{ordinal}")),
        cache_id: ordinal,
        source_sheet: src.name.clone(),
        source_ref: range_ref(r1, c1, r2, c2),
        fields,
        row_fields,
        col_fields,
        page_fields,
        data_fields,
        data_on_rows: meta.data_on_rows,
        location_ref: range_ref(rr1, rc1, rr2, rc2),
        style_name: meta.style_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "PivotStyleLight16".to_string()),
        // PivotMeta 注释:row_grand_totals 对应 xlsx colGrandTotals,反之亦然,此处交叉映射
        row_grand_totals: meta.col_grand_totals,
        col_grand_totals: meta.row_grand_totals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PivotWriteSpec {
        let f = |n: &str, items: &[&str], numeric: bool| PivotFieldSpec {
            name: n.to_string(),
            items: items.iter().map(|s| s.to_string()).collect(),
            numeric,
        };
        PivotWriteSpec {
            name: "数据透视表1".into(),
            cache_id: 1,
            source_sheet: "pivot_test".into(),
            source_ref: "A1:D201".into(),
            fields: vec![
                f("Region", &[], false),
                f("Category", &["食品", "电子", "服装"], false),
                f("Color", &["红", "蓝"], false),
                f("Sales", &[], true),
            ],
            row_fields: vec![1],
            col_fields: vec![2],
            page_fields: vec![],
            data_fields: vec![PivotDataSpec { name: "Sales的Sum".into(), fld: 3, agg: "sum".into() }],
            data_on_rows: false,
            location_ref: "A3:E8".into(),
            style_name: "PivotStyleLight20".into(),
            row_grand_totals: true,
            col_grand_totals: true,
        }
    }

    #[test]
    fn xml_round_trips_through_our_parsers() {
        let sp = sample();
        let cache = cache_definition_xml(&sp).expect("cache");
        let pt = pivot_table_xml(&sp).expect("pt");
        let (names, _items, sheet, rf) = crate::ooxml::pivot::parse_cache_definition_full(&cache);
        assert_eq!(names, vec!["Region", "Category", "Color", "Sales"]);
        assert_eq!(sheet.as_deref(), Some("pivot_test"));
        assert_eq!(rf.as_deref(), Some("A1:D201"));
        let (n, st) = crate::ooxml::pivot::parse_pivot_table_info(&pt);
        assert_eq!(n.as_deref(), Some("数据透视表1"));
        assert_eq!(st.as_deref(), Some("PivotStyleLight20"));
        let cfg = crate::ooxml::pivot::extract_pivot_config(&pt, &names).expect("cfg");
        assert_eq!(cfg.row_fields, vec!["Category"]);
        assert_eq!(cfg.col_fields, vec!["Color"]);
        assert_eq!(cfg.value_fields[0].0, "Sales");
    }

    #[test]
    fn structure_matches_wps_sample_shape() {
        let sp = sample();
        let pt = pivot_table_xml(&sp).unwrap();
        assert!(pt.contains(r#"<location ref="A3:E8" firstHeaderRow="1" firstDataRow="2" firstDataCol="1"/>"#));
        assert!(pt.contains(r#"<pivotField axis="axisRow" compact="0" showAll="0"><items count="4"><item x="0"/><item x="1"/><item x="2"/><item t="default"/></items></pivotField>"#));
        assert!(pt.contains(r#"<pivotField dataField="1" compact="0" showAll="0"/>"#));
        assert!(pt.contains(r#"<rowFields count="1"><field x="1"/></rowFields>"#));
        assert!(pt.contains(r#"<colFields count="1"><field x="2"/></colFields>"#));
        assert!(pt.contains(r#"<dataField name="Sales的Sum" fld="3" baseField="0" baseItem="0"/>"#));
        let cache = cache_definition_xml(&sp).unwrap();
        assert!(cache.contains(r#"saveData="0" refreshOnLoad="1""#));
        assert!(!cache.contains("r:id=\"rId"));
    }

    #[test]
    fn two_values_append_minus_two_and_level() {
        let mut sp = sample();
        sp.fields.push(PivotFieldSpec { name: "Qty".into(), items: vec![], numeric: true });
        sp.data_fields.push(PivotDataSpec { name: "Qty的Avg".into(), fld: 4, agg: "mean".into() });
        let pt = pivot_table_xml(&sp).unwrap();
        assert!(pt.contains(r#"<colFields count="2"><field x="2"/><field x="-2"/></colFields>"#));
        assert!(pt.contains(r#"firstDataRow="3""#));
        assert!(pt.contains(r#"subtotal="average""#));
    }

    #[test]
    fn invalid_spec_degrades_to_none() {
        let mut sp = sample();
        sp.data_fields.clear();
        assert!(pivot_table_xml(&sp).is_none() && cache_definition_xml(&sp).is_none());
        let mut sp = sample();
        sp.row_fields = vec![9];
        assert!(pivot_table_xml(&sp).is_none());
        let mut sp = sample();
        sp.row_fields = vec![3]; // 数值字段不能做轴字段
        assert!(pivot_table_xml(&sp).is_none());
    }

    #[test]
    fn fragments_and_content_types() {
        let ct = "<Types><Default Extension=\"xml\"/></Types>".to_string();
        let out = append_pivot_content_types(ct.clone(), 2);
        assert!(out.ends_with("</Types>"));
        assert_eq!(out.matches("pivotTable+xml").count(), 2);
        assert_eq!(out.matches("pivotCacheDefinition+xml").count(), 2);
        assert_eq!(append_pivot_content_types(ct.clone(), 0), ct);
        assert_eq!(workbook_pivot_caches_xml(&[]), "");
        assert_eq!(
            workbook_pivot_caches_xml(&[(1, "rId5".into())]),
            r#"<pivotCaches><pivotCache cacheId="1" r:id="rId5"/></pivotCaches>"#
        );
        assert!(pivot_table_rels_xml(2).contains("../pivotCache/pivotCacheDefinition2.xml"));
        assert!(sheet_pivot_rel("rIdPv1", 1).contains("../pivotTables/pivotTable1.xml"));
        assert!(workbook_pivot_cache_rel("rId5", 1).contains("pivotCache/pivotCacheDefinition1.xml"));
        assert_eq!(esc("a&b<\"c\">"), "a&amp;b&lt;&quot;c&quot;&gt;");
    }
}

#[cfg(test)]
mod build_spec_tests {
    use super::*;
    use roe_model::calc::{PivotFieldDef, PivotMeta};

    fn mk() -> Vec<Sheet> {
        let mut src = Sheet::default();
        src.name = "src".into();
        let rows: [(&str, &str, f64); 3] = [("东区", "食品", 100.0), ("南区", "电子", 200.0), ("东区", "电子", 50.0)];
        for (c, h) in ["Region", "Category", "Sales"].iter().enumerate() {
            src.cells.set(0, c as u32, CellValue::Text(h.to_string()));
        }
        for (i, (a, b, n)) in rows.iter().enumerate() {
            let r = i as u32 + 1;
            src.cells.set(r, 0, CellValue::Text(a.to_string()));
            src.cells.set(r, 1, CellValue::Text(b.to_string()));
            src.cells.set(r, 2, CellValue::Number(*n));
        }
        let mut pv = Sheet::default();
        pv.name = "pv".into();
        pv.pivot = Some(PivotMeta {
            source_sheet: "src".into(),
            agg: "sum".into(),
            row_field_names: vec!["Category".into()],
            value_fields: vec![PivotFieldDef {
                name: "Sales的Sum".into(),
                selected_values: vec![],
                agg: Some("sum".into()),
                source_field: Some("Sales".into()),
            }],
            cells_rect: Some((2, 0, 5, 2)),
            row_grand_totals: true,
            col_grand_totals: false,
            ..Default::default()
        });
        vec![pv, src]
    }

    #[test]
    fn build_ok_and_feeds_writers() {
        let sheets = mk();
        let sp = build_spec(&sheets, 0, 1).expect("spec");
        assert_eq!(sp.source_ref, "A1:C4");
        assert_eq!(sp.location_ref, "A3:C6");
        assert_eq!(sp.name, "PivotTable1");
        assert_eq!(sp.style_name, "PivotStyleLight16");
        assert_eq!(sp.cache_id, 1);
        assert_eq!(sp.row_fields, vec![1]);
        assert_eq!(sp.fields[1].items, vec!["食品", "电子"]);
        assert!(sp.fields[2].numeric && !sp.fields[0].numeric);
        assert_eq!(sp.data_fields[0].fld, 2);
        // 交叉映射:meta.col_grand_totals=false -> xlsx rowGrandTotals=0
        assert!(!sp.row_grand_totals && sp.col_grand_totals);
        assert!(pivot_table_xml(&sp).is_some() && cache_definition_xml(&sp).is_some());
        assert!(build_spec(&sheets, 1, 1).is_none(), "源 sheet 本身无 pivot");
    }

    #[test]
    fn degrade_cases_return_none() {
        let mut s = mk();
        s[0].pivot.as_mut().unwrap().source_sheet = "nope".into();
        assert!(build_spec(&s, 0, 1).is_none());
        let mut s = mk();
        s[0].pivot.as_mut().unwrap().cells_rect = None;
        assert!(build_spec(&s, 0, 1).is_none());
        let mut s = mk();
        s[0].pivot.as_mut().unwrap().row_field_names = vec!["Sales".into()];
        assert!(build_spec(&s, 0, 1).is_none(), "数值列不能做轴字段");
        let mut s = mk();
        s[0].pivot.as_mut().unwrap().row_field_names = vec!["Nope".into()];
        assert!(build_spec(&s, 0, 1).is_none());
        let mut s = mk();
        s[0].pivot.as_mut().unwrap().value_fields[0].source_field = None;
        assert!(build_spec(&s, 0, 1).is_none());
        let mut s = mk();
        s[1].cells.set(0, 2, CellValue::Text("Region".into()));
        assert!(build_spec(&s, 0, 1).is_none(), "表头重名");
        let mut s = mk();
        s[0].pivot.as_mut().unwrap().value_fields[0].source_field = Some("Region".into());
        assert!(build_spec(&s, 0, 1).is_none(), "sum 的源列非数值");
    }

    #[test]
    fn explicit_ref_name_style_and_name_collision() {
        let mut s = mk();
        {
            let m = s[0].pivot.as_mut().unwrap();
            m.source_ref = Some("$A$1:$C$4".into());
            m.name = Some("数据透视表1".into());
            m.style_name = Some("PivotStyleLight20".into());
            m.value_fields[0].name = "Sales".into(); // 与源字段同名
        }
        let sp = build_spec(&s, 0, 2).unwrap();
        assert_eq!(sp.source_ref, "A1:C4");
        assert_eq!(sp.name, "数据透视表1");
        assert_eq!(sp.style_name, "PivotStyleLight20");
        assert_eq!(sp.cache_id, 2);
        assert_eq!(sp.data_fields[0].name, "Sales ");
    }

    #[test]
    fn range_helpers() {
        assert_eq!(col_letters(0), "A");
        assert_eq!(col_letters(25), "Z");
        assert_eq!(col_letters(26), "AA");
        assert_eq!(parse_range("A1:H201"), Some((0, 0, 200, 7)));
        assert_eq!(parse_range("$B2:$AA3"), Some((1, 1, 2, 26)));
        assert_eq!(parse_range("A1"), None);
        assert_eq!(parse_range("C3:A1"), None);
        assert_eq!(parse_range("A0:B2"), None);
    }
}

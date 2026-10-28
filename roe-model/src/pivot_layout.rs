//! 透视表布局:把 PivotMeta(树 + 数据矩阵)展开成绝对坐标的单元格列表。
//! 规则与 roe-core/src/tile.rs 的透视渲染段逐段对应;本模块只产出数据,不绘制。
//! 用途:物化进 sheet.cells(公式栏/get_cell/AI/保存),以及后续替换渲染特判。

use crate::calc::{CellValue, PivotHeaderNode, PivotLayoutMode, PivotMeta};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PivotCellKind {
    FilterName,
    FilterValue,
    CornerLabel,
    ColHeader,
    RowHeader,
    /// 小计节点的表头文字("X Total")
    Subtotal,
    Data,
    TotalLabel,
    /// 总计数值(横向/纵向/右下角)
    Total,
}

#[derive(Debug, Clone)]
pub struct PivotCell {
    pub row: u32,
    pub col: u32,
    pub kind: PivotCellKind,
    pub value: CellValue,
}

#[derive(Debug, Clone, Default)]
pub struct PivotLayout {
    /// 按 (row, col) 升序
    pub cells: Vec<PivotCell>,
    /// 包围盒 (r1, c1, r2, c2),含端点;无任何格时为 None
    pub rect: Option<(u32, u32, u32, u32)>,
}

type Grid = BTreeMap<(u32, u32), (PivotCellKind, CellValue)>;

fn put(g: &mut Grid, r: u32, c: u32, kind: PivotCellKind, text: &str) {
    g.insert((r, c), (kind, CellValue::Text(text.to_string())));
}

fn put_num(g: &mut Grid, r: u32, c: u32, kind: PivotCellKind, v: f64) {
    g.insert((r, c), (kind, CellValue::Number(round_sig15(v))));
}

fn walk_col(
    node: &PivotHeaderNode,
    depth: u32,
    offset: u32,
    path: &[String],
    meta: &PivotMeta,
    g: &mut Grid,
    leaves: &mut BTreeMap<u32, Vec<String>>,
) {
    if node.children.is_empty() {
        leaves.insert(offset, path.to_vec());
    }
    let r = meta.base_row + meta.filter_fields.len() as u32 + 1 + depth;
    let c = meta.base_col + meta.data_col_start + offset;
    if !node.value.is_empty() {
        let kind = if node.is_subtotal { PivotCellKind::Subtotal } else { PivotCellKind::ColHeader };
        put(g, r, c, kind, &node.value);
    }
    let mut cursor = offset;
    for child in &node.children {
        let seg = if child.is_subtotal { String::new() } else { child.value.clone() };
        let mut p = path.to_vec();
        p.push(seg);
        // 小计节点借用父节点 depth(与渲染一致)
        let d = if child.is_subtotal { depth } else { depth + 1 };
        walk_col(child, d, cursor, &p, meta, g, leaves);
        cursor += child.cached_span;
    }
}

fn walk_row(
    node: &PivotHeaderNode,
    depth: u32,
    offset: u32,
    path: &[String],
    meta: &PivotMeta,
    is_outline: bool,
    g: &mut Grid,
    leaves: &mut BTreeMap<u32, Vec<String>>,
) {
    // Outline:每个节点自占一行,全部记录查值路径;Tabular:父子共享起始行,只记叶子
    if is_outline || node.children.is_empty() {
        leaves.insert(offset, path.to_vec());
    }
    let r = meta.base_row + meta.data_row_start + offset;
    let c = meta.base_col + depth;
    let text: &str = if node.value.is_empty() && !node.is_subtotal { "(空白)" } else { &node.value };
    if !text.is_empty() {
        let kind = if node.is_subtotal { PivotCellKind::Subtotal } else { PivotCellKind::RowHeader };
        put(g, r, c, kind, text);
    }
    let own_rows = if is_outline { 1 } else { node.cached_span };
    let mut cursor = if is_outline { offset + own_rows } else { offset };
    for child in &node.children {
        let span = if is_outline { child.outline_span } else { child.cached_span };
        let seg = if child.is_subtotal { String::new() } else { child.value.clone() };
        let mut p = path.to_vec();
        p.push(seg);
        let d = if child.is_subtotal { depth } else { depth + 1 };
        walk_row(child, d, cursor, &p, meta, is_outline, g, leaves);
        cursor += span;
    }
}

/// 展开透视表为单元格列表(绝对坐标)。内部会 clone 并重算 span,不修改入参。
pub fn pivot_layout(meta_in: &PivotMeta) -> PivotLayout {
    let mut m = meta_in.clone();
    m.compute_spans();
    let meta = &m;

    let mut g: Grid = Grid::new();
    let filter_len = meta.filter_fields.len() as u32;
    let n_values = meta.value_fields.len().max(1) as u32;
    let is_outline = meta.layout_mode == PivotLayoutMode::Outline;
    let has_row = !meta.row_field_names.is_empty();
    let has_col = !meta.col_field_names.is_empty();

    // 筛选区:字段名在 (fi, base_col),值标签在 (fi, base_col+1)
    for (fi, f) in meta.filter_fields.iter().enumerate() {
        let r = fi as u32;
        put(&mut g, r, meta.base_col, PivotCellKind::FilterName, &f.name);
        let label = if f.selected_values.is_empty() {
            "(全部)".to_string()
        } else if f.selected_values.len() == 1 {
            f.selected_values[0].clone()
        } else {
            "(多项)".to_string()
        };
        put(&mut g, r, meta.base_col + 1, PivotCellKind::FilterValue, &label);
    }

    // 列表头树
    let mut col_leaves: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut cursor = 0u32;
    for child in &meta.col_tree.children {
        let seg = if child.is_subtotal { String::new() } else { child.value.clone() };
        walk_col(child, 0, cursor, &[seg], meta, &mut g, &mut col_leaves);
        cursor += child.cached_span;
    }

    // 行表头树
    let mut row_leaves: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut cursor = 0u32;
    for child in &meta.row_tree.children {
        let span = if is_outline { child.outline_span } else { child.cached_span };
        let seg = if child.is_subtotal { String::new() } else { child.value.clone() };
        walk_row(child, 0, cursor, &[seg], meta, is_outline, &mut g, &mut row_leaves);
        cursor += span;
    }

    let empty: Vec<String> = Vec::new();
    let col_iter: Vec<(u32, &Vec<String>)> = if col_leaves.is_empty() {
        vec![(0u32, &empty)]
    } else {
        col_leaves.iter().map(|(k, v)| (*k, v)).collect()
    };
    let row_iter: Vec<(u32, &Vec<String>)> = if row_leaves.is_empty() {
        vec![(0u32, &empty)]
    } else {
        row_leaves.iter().map(|(k, v)| (*k, v)).collect()
    };

    // 数据区
    for &(col_offset, col_path) in &col_iter {
        for &(row_offset, row_path) in &row_iter {
            for vidx in 0..meta.value_fields.len().max(1) {
                if let Some(val) = meta.data.get(row_path, col_path, vidx) {
                    let (r, c) = if meta.data_on_rows {
                        (
                            meta.base_row + meta.data_row_start + row_offset + vidx as u32,
                            meta.base_col + meta.data_col_start + col_offset,
                        )
                    } else {
                        (
                            meta.base_row + meta.data_row_start + row_offset,
                            meta.base_col + meta.data_col_start + col_offset + vidx as u32,
                        )
                    };
                    put_num(&mut g, r, c, PivotCellKind::Data, val);
                }
            }
        }
    }

    // 总计:位置由叶子偏移现算(与渲染一致)
    let grand_total_row = meta.base_row + meta.data_row_start + row_leaves.keys().max().copied().unwrap_or(0) + 1;
    let grand_total_col = meta.base_col + meta.data_col_start + col_leaves.keys().max().copied().unwrap_or(0) + n_values;
    let total_label = "总计";

    // 横向总计(最右列):有列字段且 col_grand_totals
    if has_col && meta.col_grand_totals {
        let label_row = meta.base_row + filter_len + 1;
        for vidx in 0..n_values {
            let label = if n_values > 1 {
                let name = meta.value_fields.get(vidx as usize).map(|v| v.name.as_str()).unwrap_or("");
                format!("{}-{}", total_label, name)
            } else {
                total_label.to_string()
            };
            put(&mut g, label_row, grand_total_col + vidx, PivotCellKind::TotalLabel, &label);
        }
        for &(row_offset, row_path) in &row_iter {
            for vidx in 0..n_values {
                if let Some(val) = meta.data.get(row_path, &empty, vidx as usize) {
                    put_num(
                        &mut g,
                        meta.base_row + meta.data_row_start + row_offset,
                        grand_total_col + vidx,
                        PivotCellKind::Total,
                        val,
                    );
                }
            }
        }
    }

    // 竖向总计(最下行):有行字段且 row_grand_totals
    if has_row && meta.row_grand_totals {
        put(&mut g, grand_total_row, meta.base_col, PivotCellKind::TotalLabel, total_label);
        for &(col_offset, col_path) in &col_iter {
            for vidx in 0..n_values {
                if let Some(val) = meta.data.get(&empty, col_path, vidx as usize) {
                    put_num(
                        &mut g,
                        grand_total_row,
                        meta.base_col + meta.data_col_start + col_offset + vidx,
                        PivotCellKind::Total,
                        val,
                    );
                }
            }
        }
    }

    // 右下角总计
    if has_row && has_col && meta.row_grand_totals && meta.col_grand_totals {
        for vidx in 0..n_values {
            if let Some(val) = meta.data.get(&empty, &empty, vidx as usize) {
                put_num(&mut g, grand_total_row, grand_total_col + vidx, PivotCellKind::Total, val);
            }
        }
    }

    // 角落标签 a:行字段名(紧邻数据区上方一行,从 base_col 起横排)
    if meta.data_row_start > 0 {
        let label_row = meta.base_row + meta.data_row_start - 1;
        for (i, name) in meta.row_field_names.iter().enumerate() {
            let col = meta.base_col + i as u32;
            if col >= meta.base_col + meta.data_col_start {
                break;
            }
            put(&mut g, label_row, col, PivotCellKind::CornerLabel, name);
        }
    }
    // 角落标签 b:列字段名 + "值"
    if meta.data_col_start > 0 {
        let label_row = meta.base_row + filter_len;
        for (i, name) in meta.col_field_names.iter().enumerate() {
            put(&mut g, label_row, meta.base_col + meta.data_col_start + i as u32, PivotCellKind::CornerLabel, name);
        }
        if meta.value_fields.len() > 1 && !meta.data_on_rows && has_col {
            let col = meta.base_col + meta.data_col_start + meta.col_field_names.len() as u32;
            put(&mut g, label_row, col, PivotCellKind::CornerLabel, "值");
        }
    }
    // 角落标签 c:dataOnRows
    if meta.data_on_rows {
        put(&mut g, meta.base_row + filter_len + 1, meta.base_col, PivotCellKind::CornerLabel, "值");
        for (vidx, vf) in meta.value_fields.iter().enumerate() {
            put(&mut g, meta.base_row + meta.data_row_start + vidx as u32, meta.base_col, PivotCellKind::CornerLabel, &vf.name);
        }
    }
    // 角落标签 d:单值字段名
    if has_row && has_col && meta.value_fields.len() == 1 {
        put(&mut g, meta.base_row + filter_len, meta.base_col, PivotCellKind::CornerLabel, &meta.value_fields[0].name);
    }
    if !has_row && has_col && meta.value_fields.len() == 1 {
        put(&mut g, meta.base_row + meta.data_row_start, meta.base_col, PivotCellKind::CornerLabel, &meta.value_fields[0].name);
    }
    // 角落标签 e:纯行字段,值字段名沿数据列循环
    if has_row && !has_col && !meta.value_fields.is_empty() && !meta.data_on_rows {
        let label_row = meta.base_row + meta.data_row_start - 1;
        let n_cols: u32 = meta.col_tree.children.iter().map(|c| c.cached_span).sum::<u32>().max(meta.value_fields.len() as u32);
        let nv = meta.value_fields.len() as u32;
        for i in 0..n_cols {
            let name = &meta.value_fields[(i % nv) as usize].name;
            put(&mut g, label_row, meta.base_col + meta.data_col_start + i, PivotCellKind::CornerLabel, name);
        }
    }
    // 角落标签 f:多值字段名沿数据列循环
    if meta.value_fields.len() > 1 && !meta.data_on_rows {
        let label_row = meta.base_row + meta.data_row_start - 1;
        let n_cols: u32 = meta.col_tree.children.iter().map(|c| c.cached_span).sum();
        let nv = meta.value_fields.len() as u32;
        for i in 0..n_cols {
            let name = &meta.value_fields[(i % nv) as usize].name;
            put(&mut g, label_row, meta.base_col + meta.data_col_start + i, PivotCellKind::CornerLabel, name);
        }
    }

    // 汇总
    let mut rect: Option<(u32, u32, u32, u32)> = None;
    let mut cells = Vec::with_capacity(g.len());
    for ((r, c), (kind, value)) in g {
        rect = Some(match rect {
            None => (r, c, r, c),
            Some((r1, c1, r2, c2)) => (r1.min(r), c1.min(c), r2.max(r), c2.max(c)),
        });
        cells.push(PivotCell { row: r, col: c, kind, value });
    }
    PivotLayout { cells, rect }
}

/// 把透视表物化进 sheet.cells:先清旧 cells_rect 区域,再写入布局结果,更新 cells_rect。
/// sheet.pivot 为 None 时无操作。返回写入的单元格数。
pub fn materialize_pivot(sheet: &mut crate::calc::Sheet) -> usize {
    let Some(mut meta) = sheet.pivot.take() else { return 0 };
    if let Some((r1, c1, r2, c2)) = meta.cells_rect {
        for r in r1..=r2 {
            for c in c1..=c2 {
                sheet.cells.set(r, c, CellValue::Empty);
            }
        }
    }
    let layout = pivot_layout(&meta);
    let n = layout.cells.len();
    for cell in layout.cells {
        sheet.cells.set(cell.row, cell.col, cell.value);
    }
    meta.cells_rect = layout.rect;
    sheet.pivot = Some(meta);
    n
}

/// 规整到 15 位有效数字,去掉 f64 累加噪声(如 0.1+0.2 -> 0.3)。
/// 非有限值原样返回。只用于物化写单元格,不改 PivotMeta.data。
fn round_sig15(v: f64) -> f64 {
    if !v.is_finite() {
        return v;
    }
    format!("{:.14e}", v).parse::<f64>().unwrap_or(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calc::PivotFieldDef;

    #[test]
    fn test_round_sig15() {
        assert_eq!(round_sig15(0.1 + 0.2), 0.3);
        assert_eq!(round_sig15(398979.23000000004), 398979.23);
        assert_eq!(round_sig15(103656.56999999999), 103656.57);
        assert_eq!(round_sig15(-398979.23000000004), -398979.23);
        assert_eq!(round_sig15(0.1234567), 0.1234567);
        assert_eq!(round_sig15(0.0), 0.0);
        assert!(round_sig15(f64::NAN).is_nan());
    }

    fn text_at(l: &PivotLayout, r: u32, c: u32) -> Option<String> {
        l.cells.iter().find(|x| x.row == r && x.col == c).map(|x| x.value.to_display_string())
    }
    fn num_at(l: &PivotLayout, r: u32, c: u32) -> Option<f64> {
        l.cells.iter().find(|x| x.row == r && x.col == c).and_then(|x| match &x.value {
            CellValue::Number(n) => Some(*n),
            _ => None,
        })
    }

    /// 2 个行字段(Region/Product) x 无列字段 x 1 个值字段,含最外层小计。
    /// base_row=2,data_row_start=1,data_col_start=2(与 v2 创建一致)。
    fn sample_meta() -> PivotMeta {
        let leaf = |v: &str| PivotHeaderNode::new(v);
        let sub = |v: &str| PivotHeaderNode { value: v.into(), is_subtotal: true, ..Default::default() };
        let mut east = PivotHeaderNode::new("East");
        east.children = vec![leaf("Apple"), leaf("Banana"), sub("East Total")];
        let mut west = PivotHeaderNode::new("West");
        west.children = vec![leaf("Apple"), leaf("Banana"), sub("West Total")];
        let root = PivotHeaderNode { children: vec![east, west], ..Default::default() };
        let mut meta = PivotMeta {
            row_field_names: vec!["Region".into(), "Product".into()],
            value_fields: vec![PivotFieldDef::new("SalesSum")],
            row_tree: root,
            base_row: 2,
            base_col: 0,
            data_row_start: 1,
            data_col_start: 2,
            row_grand_totals: true,
            col_grand_totals: true,
            ..Default::default()
        };
        let s = |x: &str| x.to_string();
        meta.data.insert(vec![s("East"), s("Apple")], vec![], 0, 100.0);
        meta.data.insert(vec![s("East"), s("Banana")], vec![], 0, 200.0);
        meta.data.insert(vec![s("East"), s("")], vec![], 0, 300.0);
        meta.data.insert(vec![s("West"), s("Apple")], vec![], 0, 150.0);
        meta.data.insert(vec![s("West"), s("Banana")], vec![], 0, 50.0);
        meta.data.insert(vec![s("West"), s("")], vec![], 0, 200.0);
        meta.data.insert(vec![], vec![], 0, 500.0);
        meta
    }

    #[test]
    fn layout_outline_two_row_fields_with_subtotal() {
        let l = pivot_layout(&sample_meta());
        // 角落标签
        assert_eq!(text_at(&l, 2, 0).as_deref(), Some("Region"));
        assert_eq!(text_at(&l, 2, 1).as_deref(), Some("Product"));
        assert_eq!(text_at(&l, 2, 2).as_deref(), Some("SalesSum"));
        // 行表头(Outline:East 3, Apple 4, Banana 5, East Total 6, West 7 ...)
        assert_eq!(text_at(&l, 3, 0).as_deref(), Some("East"));
        assert_eq!(text_at(&l, 4, 1).as_deref(), Some("Apple"));
        assert_eq!(text_at(&l, 5, 1).as_deref(), Some("Banana"));
        assert_eq!(text_at(&l, 6, 0).as_deref(), Some("East Total"));
        assert_eq!(text_at(&l, 7, 0).as_deref(), Some("West"));
        assert_eq!(text_at(&l, 10, 0).as_deref(), Some("West Total"));
        // 数据区(第 2 列);父节点行 East/West 无值
        assert_eq!(num_at(&l, 3, 2), None);
        assert_eq!(num_at(&l, 4, 2), Some(100.0));
        assert_eq!(num_at(&l, 5, 2), Some(200.0));
        assert_eq!(num_at(&l, 6, 2), Some(300.0));
        assert_eq!(num_at(&l, 8, 2), Some(150.0));
        assert_eq!(num_at(&l, 9, 2), Some(50.0));
        assert_eq!(num_at(&l, 10, 2), Some(200.0));
        // 竖向总计
        assert_eq!(text_at(&l, 11, 0).as_deref(), Some("总计"));
        assert_eq!(num_at(&l, 11, 2), Some(500.0));
        // 无列字段:没有横向总计与右下角
        assert_eq!(l.rect, Some((2, 0, 11, 2)));
    }

    #[test]
    fn layout_row_grand_total_switch_off() {
        let mut meta = sample_meta();
        meta.row_grand_totals = false;
        let l = pivot_layout(&meta);
        assert_eq!(text_at(&l, 11, 0), None);
        assert_eq!(num_at(&l, 11, 2), None);
    }

    #[test]
    fn layout_does_not_mutate_input() {
        let meta = sample_meta();
        let _ = pivot_layout(&meta);
        assert_eq!(meta.row_tree.children[0].cached_span, 0);
    }

    #[test]
    fn materialize_writes_cells_readable_by_get() {
        let mut sheet = crate::calc::Sheet::default();
        sheet.pivot = Some(sample_meta());
        let n = materialize_pivot(&mut sheet);
        assert!(n > 0);
        assert_eq!(sheet.cells.get(3, 0).to_display_string(), "East");
        assert!(matches!(sheet.cells.get(4, 2), CellValue::Number(v) if v == 100.0));
        assert_eq!(sheet.cells.get(11, 0).to_display_string(), "总计");
        assert_eq!(sheet.pivot.as_ref().unwrap().cells_rect, Some((2, 0, 11, 2)));
    }

    #[test]
    fn materialize_twice_clears_stale_cells() {
        let mut sheet = crate::calc::Sheet::default();
        sheet.pivot = Some(sample_meta());
        materialize_pivot(&mut sheet);
        // 关闭竖向总计后重新物化,第 11 行应被清空
        sheet.pivot.as_mut().unwrap().row_grand_totals = false;
        materialize_pivot(&mut sheet);
        assert!(matches!(sheet.cells.get(11, 0), CellValue::Empty));
        assert!(matches!(sheet.cells.get(11, 2), CellValue::Empty));
        assert_eq!(sheet.cells.get(3, 0).to_display_string(), "East");
        assert_eq!(sheet.pivot.as_ref().unwrap().cells_rect, Some((2, 0, 10, 2)));
    }
}

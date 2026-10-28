use roe_model::calc::{Sheet, Workbook};
use roe_script::calc_api::{register, with_workbook};
use std::collections::HashMap;

fn with_pivot() -> Workbook {
    let mut wb = Workbook {
        sheets: vec![Sheet { name: "Data".into(), ..Default::default() }],
        named_ranges: HashMap::new(),
        named_formulas: HashMap::new(),
        styles: roe_model::style::StyleRegistry::default(),
        formula_dialect_source: Default::default(),
    };
    let rows = [
        ["Region", "Product", "Sales"],
        ["East", "Apple", "100"],
        ["East", "Banana", "50"],
        ["West", "Apple", "70"],
        ["West", "Banana", "30"],
    ];
    let mut e = rhai::Engine::new();
    register(&mut e);
    let mut script = String::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            script.push_str(&format!("set_cell(0, {r}, {c}, \"{v}\");\n"));
        }
    }
    with_workbook(&mut wb, || { e.eval::<()>(&script).unwrap(); });
    let cfg: roe_polars_bridge::PivotConfig = serde_json::from_str(
        r#"{"rows":["Region"],"cols":["Product"],"values":["Sales"],"agg":"sum","filters":{}}"#,
    ).unwrap();
    let idx = roe_polars_bridge::pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PT".into()).unwrap();
    assert_eq!(idx, 1);
    wb
}

fn engine() -> rhai::Engine {
    let mut e = rhai::Engine::new();
    register(&mut e);
    e
}

/// 返回 (脚本结果, 脏单元格数)
fn run(wb: &mut Workbook, e: &rhai::Engine, script: &str) -> (String, usize) {
    let mut out = String::new();
    let dirty = with_workbook(wb, || { out = e.eval::<String>(script).unwrap(); });
    (out, dirty.len())
}

#[test]
fn set_config_swaps_layout_and_marks_dirty() {
    let mut wb = with_pivot();
    let e = engine();
    let (r, dirty) = run(&mut wb, &e,
        r#"pivot_set_config(1, `{"rows":["Product"],"cols":["Region"],"values":["Sales"],"filters":[]}`)"#);
    assert_eq!(r, "ok");
    assert!(dirty > 0, "成功必须标脏,否则不入撤销栈");
    let m = wb.sheets[1].pivot.as_ref().unwrap();
    assert_eq!(m.row_field_names, vec!["Product".to_string()]);
    assert_eq!(m.col_field_names, vec!["Region".to_string()]);
}

#[test]
fn set_config_rejects_bad_input_and_keeps_pivot() {
    let mut wb = with_pivot();
    let e = engine();
    for script in [
        r#"pivot_set_config(1, `{"rows":["Nope"],"cols":[],"values":["Sales"],"filters":[]}`)"#,
        r#"pivot_set_config(1, `{"rows":["Region"],"cols":["Region"],"values":["Sales"]}`)"#,
        r#"pivot_set_config(1, `{"value":["Sales"]}`)"#,
        r#"pivot_set_config(1, `{"rows":"Region"}`)"#,
        r#"pivot_set_config(1, `not json`)"#,
        r#"pivot_set_config(0, `{"rows":["Region"]}`)"#,
        r#"pivot_set_config(99, `{}`)"#,
        r#"pivot_set_config(-1, `{}`)"#,
    ] {
        let (r, dirty) = run(&mut wb, &e, script);
        assert!(r.starts_with("err:"), "{script} => {r}");
        assert_eq!(dirty, 0, "失败不应标脏: {script}");
    }
    let m = wb.sheets[1].pivot.as_ref().unwrap();
    assert_eq!(m.row_field_names, vec!["Region".to_string()]);
    assert_eq!(m.col_field_names, vec!["Product".to_string()]);
}

#[test]
fn set_filter_then_clear() {
    let mut wb = with_pivot();
    let e = engine();
    let (r, dirty) = run(&mut wb, &e, r#"pivot_set_filter(1, "Region", `["East"]`)"#);
    assert_eq!(r, "ok");
    assert!(dirty > 0);
    let m = wb.sheets[1].pivot.as_ref().unwrap();
    assert_eq!(m.field_filters.get("Region"), Some(&vec!["East".to_string()]));
    assert_eq!(m.row_tree.children.len(), 1);
    let (r, _) = run(&mut wb, &e, r#"pivot_set_filter(1, "Region", "[]")"#);
    assert_eq!(r, "ok");
    let m = wb.sheets[1].pivot.as_ref().unwrap();
    assert!(m.field_filters.is_empty());
    assert_eq!(m.row_tree.children.len(), 2);
}

#[test]
fn set_filter_rejects_bad_input() {
    let mut wb = with_pivot();
    let e = engine();
    for script in [
        r#"pivot_set_filter(1, "Sales", `["1"]`)"#,
        r#"pivot_set_filter(1, "Nope", `["x"]`)"#,
        r#"pivot_set_filter(1, "Region", `East`)"#,
        r#"pivot_set_filter(0, "Region", "[]")"#,
    ] {
        let (r, dirty) = run(&mut wb, &e, script);
        assert!(r.starts_with("err:"), "{script} => {r}");
        assert_eq!(dirty, 0, "{script}");
    }
    assert!(wb.sheets[1].pivot.as_ref().unwrap().field_filters.is_empty());
}

#[test]
fn set_sort_desc_then_asc_and_rejects_page_field() {
    let mut wb = with_pivot();
    let e = engine();
    let (r, dirty) = run(&mut wb, &e, r#"pivot_set_sort(1, "Product", false)"#);
    assert_eq!(r, "ok");
    assert!(dirty > 0);
    let m = wb.sheets[1].pivot.as_ref().unwrap();
    let cols: Vec<String> = m.col_tree.children.iter().map(|c| c.value.clone()).collect();
    assert_eq!(cols, vec!["Banana".to_string(), "Apple".to_string()]);
    let (r, _) = run(&mut wb, &e, r#"pivot_set_sort(1, "Product", true)"#);
    assert_eq!(r, "ok");
    assert!(wb.sheets[1].pivot.as_ref().unwrap().sort.is_empty());

    run(&mut wb, &e, r#"pivot_set_config(1, `{"rows":["Product"],"values":["Sales"],"filters":["Region"]}`)"#);
    let (r, dirty) = run(&mut wb, &e, r#"pivot_set_sort(1, "Region", false)"#);
    assert!(r.starts_with("err:"), "{r}");
    assert_eq!(dirty, 0);
}

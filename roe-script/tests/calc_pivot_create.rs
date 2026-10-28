use roe_model::calc::{Sheet, Workbook};
use roe_script::calc_api::{register, with_workbook};
use std::collections::HashMap;

fn data_wb() -> Workbook {
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
    wb
}

fn run(wb: &mut Workbook, script: &str) -> (String, usize, bool) {
    let mut e = rhai::Engine::new();
    register(&mut e);
    let mut out = String::new();
    let dirty = with_workbook(wb, || { out = e.eval::<String>(script).unwrap(); });
    (out, dirty.len(), roe_script::calc_api::take_struct_edit())
}

const OK_CFG: &str = r#"`{"rows":["Region"],"cols":["Product"],"values":["Sales"],"agg":"sum","filters":{}}`"#;

#[test]
fn create_ok_adds_sheet_and_marks_edits() {
    let mut wb = data_wb();
    let (r, dirty, structural) = run(&mut wb, &format!(r#"pivot_create(0, {OK_CFG}, "PT")"#));
    assert_eq!(r, "ok:1");
    assert!(dirty > 0);
    assert!(structural);
    assert_eq!(wb.sheets.len(), 2);
    assert_eq!(wb.sheets[1].name, "PT");
    assert!(wb.sheets[1].pivot.is_some());
}

#[test]
fn create_page_field_with_empty_filter_does_not_restrict_rows() {
    let mut wb = data_wb();
    let cfg = r#"`{"rows":["Product"],"cols":[],"values":["Sales"],"agg":"mean","filters":{"Region":[]}}`"#;
    let (r, _, _) = run(&mut wb, &format!(r#"pivot_create(0, {cfg}, "PT2")"#));
    assert_eq!(r, "ok:1");
    let m = wb.sheets[1].pivot.as_ref().unwrap();
    assert_eq!(m.filter_fields.len(), 1);
    assert_eq!(m.row_tree.children.len(), 2);
}

#[test]
fn create_rejects_bad_input_and_leaves_workbook_unchanged() {
    let mut wb = data_wb();
    let long = "x".repeat(32);
    let scripts = vec![
        format!(r#"pivot_create(0, {OK_CFG}, "Data")"#),
        format!(r#"pivot_create(0, {OK_CFG}, "data")"#),
        format!(r#"pivot_create(0, {OK_CFG}, "")"#),
        format!(r#"pivot_create(0, {OK_CFG}, "a/b")"#),
        format!(r#"pivot_create(0, {OK_CFG}, "{long}")"#),
        format!(r#"pivot_create(5, {OK_CFG}, "PT")"#),
        format!(r#"pivot_create(-1, {OK_CFG}, "PT")"#),
        r#"pivot_create(0, `{"rows":["Nope"],"cols":[],"values":["Sales"],"agg":"sum","filters":{}}`, "PT")"#.to_string(),
        r#"pivot_create(0, `{"rows":["Region"],"cols":["Region"],"values":["Sales"],"agg":"sum","filters":{}}`, "PT")"#.to_string(),
        r#"pivot_create(0, `{"rows":["Region"],"cols":[],"values":["Sales"],"agg":"median","filters":{}}`, "PT")"#.to_string(),
        r#"pivot_create(0, `{"rows":["Region"],"cols":[],"values":[],"agg":"sum","filters":{}}`, "PT")"#.to_string(),
        r#"pivot_create(0, `not json`, "PT")"#.to_string(),
    ];
    for sc in scripts {
        let (r, dirty, structural) = run(&mut wb, &sc);
        assert!(r.starts_with("err:"), "{sc} => {r}");
        assert_eq!(dirty, 0, "{sc}");
        assert!(!structural, "{sc}");
        assert_eq!(wb.sheets.len(), 1, "{sc}");
    }
}

#[test]
fn create_rejects_pivot_sheet_as_source() {
    let mut wb = data_wb();
    let (r, _, _) = run(&mut wb, &format!(r#"pivot_create(0, {OK_CFG}, "PT")"#));
    assert_eq!(r, "ok:1");
    let (r, _, _) = run(&mut wb, &format!(r#"pivot_create(1, {OK_CFG}, "PT3")"#));
    assert!(r.starts_with("err:"), "{r}");
    assert_eq!(wb.sheets.len(), 2);
}

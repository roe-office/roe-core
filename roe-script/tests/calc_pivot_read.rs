use roe_model::calc::{Sheet, Workbook};
use roe_script::calc_api::{register, register_readonly, with_workbook};
use std::collections::HashMap;

fn data_workbook() -> Workbook {
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
    let e = {
        let mut e = rhai::Engine::new();
        register(&mut e);
        e
    };
    let mut script = String::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            script.push_str(&format!("set_cell(0, {r}, {c}, \"{v}\");\n"));
        }
    }
    with_workbook(&mut wb, || {
        e.eval::<()>(&script).unwrap();
    });
    wb
}

fn with_pivot() -> Workbook {
    let mut wb = data_workbook();
    let cfg: roe_polars_bridge::PivotConfig = serde_json::from_str(
        r#"{"rows":["Region"],"cols":["Product"],"values":["Sales"],"agg":"sum","filters":{}}"#,
    )
    .unwrap();
    let idx = roe_polars_bridge::pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PT".into()).unwrap();
    assert_eq!(idx, 1);
    wb
}

fn run(wb: &mut Workbook, e: &rhai::Engine, script: &str) -> String {
    let mut out = String::new();
    with_workbook(wb, || {
        out = e.eval::<String>(script).unwrap();
    });
    out
}

#[test]
fn get_pivot_config_returns_layout() {
    let mut wb = with_pivot();
    let mut e = rhai::Engine::new();
    register(&mut e);
    let s = run(&mut wb, &e, "get_pivot_config(1)");
    let v: serde_json::Value = serde_json::from_str(&s).unwrap_or_else(|_| panic!("不是 JSON: {s}"));
    assert_eq!(v["rows"], serde_json::json!(["Region"]));
    assert_eq!(v["cols"], serde_json::json!(["Product"]));
    assert_eq!(v["values"], serde_json::json!(["Sales"]));
    assert_eq!(v["filters"], serde_json::json!([]));
    assert_eq!(v["source_idx"], serde_json::json!(0));
    assert_eq!(v["fields"], serde_json::json!(["Region", "Product", "Sales"]));
}

#[test]
fn get_pivot_config_errors_are_err_prefixed() {
    let mut wb = with_pivot();
    let mut e = rhai::Engine::new();
    register(&mut e);
    for script in ["get_pivot_config(0)", "get_pivot_config(99)", "get_pivot_config(-1)"] {
        let s = run(&mut wb, &e, script);
        assert!(s.starts_with("err:"), "{script} => {s}");
    }
}

#[test]
fn get_pivot_config_available_in_readonly_env() {
    let mut wb = with_pivot();
    let mut e = rhai::Engine::new();
    register_readonly(&mut e);
    let s = run(&mut wb, &e, "get_pivot_config(1)");
    assert!(s.starts_with('{'), "{s}");
}

use roe_model::object_ids::ensure_object_ids;
use roe_parser::ooxml::xlsx::XlsxParser;
use roe_script::calc_api::{register, with_workbook};

fn load(name: &str) -> roe_model::calc::Workbook {
    let path = format!("{}/../test_samples/xlsx/{}", env!("CARGO_MANIFEST_DIR"), name);
    let mut wb = XlsxParser::parse(&std::fs::read(path).unwrap()).unwrap();
    for sh in wb.sheets.iter_mut() { ensure_object_ids(sh); }
    wb
}

fn run_str(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine, script: &str) -> Result<String, String> {
    let mut out: Result<String, String> = Err(String::new());
    with_workbook(wb, || { out = e.eval::<String>(script).map_err(|x| x.to_string()); });
    out
}

fn run_unit(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine, script: &str) -> Result<(), String> {
    let mut out: Result<(), String> = Err(String::new());
    with_workbook(wb, || { out = e.eval::<()>(script).map_err(|x| x.to_string()); });
    out
}

fn get(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine, id: u64) -> serde_json::Value {
    let s = run_str(wb, e, &format!("calc_get_object(0, {id})")).unwrap();
    serde_json::from_str(&s).unwrap()
}

fn first_id(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine) -> u64 {
    let s = run_str(wb, e, "calc_list_objects(0)").unwrap();
    serde_json::from_str::<serde_json::Value>(&s).unwrap().as_array().unwrap()[0]["id"].as_u64().unwrap()
}

#[test]
fn move_to_cell_keeps_size() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let id = first_id(&mut wb, &e);
    let before = get(&mut wb, &e, id);
    run_unit(&mut wb, &e, &format!("calc_move_object_to_cell(0, {id}, 2, 3)")).unwrap();
    let after = get(&mut wb, &e, id);
    let cfg = roe_core::tile::CalcRenderConfig::default();
    let (ex, ey) = roe_core::tile::cell_to_pt(&wb.sheets[0], 2, 3, &cfg);
    assert!((after["x_pt"].as_f64().unwrap() - ex as f64).abs() < 0.02, "{after}");
    assert!((after["y_pt"].as_f64().unwrap() - ey as f64).abs() < 0.02, "{after}");
    assert_eq!(after["width_pt"], before["width_pt"]);
    assert_eq!(after["height_pt"], before["height_pt"]);
}

#[test]
fn resize_to_range_covers_cells() {
    let mut wb = load("sc_data_xlsx__groupShape.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let id = first_id(&mut wb, &e);
    run_unit(&mut wb, &e, &format!("calc_resize_object_to_range(0, {id}, 2, 1, 5, 3)")).unwrap();
    let after = get(&mut wb, &e, id);
    let cfg = roe_core::tile::CalcRenderConfig::default();
    let (x1, y1) = roe_core::tile::cell_to_pt(&wb.sheets[0], 2, 1, &cfg);
    let (x2, y2) = roe_core::tile::cell_to_pt(&wb.sheets[0], 6, 4, &cfg);
    assert!((after["x_pt"].as_f64().unwrap() - x1 as f64).abs() < 0.02, "{after}");
    assert!((after["y_pt"].as_f64().unwrap() - y1 as f64).abs() < 0.02, "{after}");
    assert!((after["width_pt"].as_f64().unwrap() - (x2 - x1) as f64).abs() < 0.02, "{after}");
    assert!((after["height_pt"].as_f64().unwrap() - (y2 - y1) as f64).abs() < 0.02, "{after}");
}

#[test]
fn errors_are_clear_and_do_not_modify() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let id = first_id(&mut wb, &e);
    let before = get(&mut wb, &e, id);
    let err = run_unit(&mut wb, &e, "calc_move_object_to_cell(0, 9999, 1, 1)").unwrap_err();
    assert!(err.contains("不存在") && err.contains("calc_list_objects"), "{err}");
    let err = run_unit(&mut wb, &e, &format!("calc_move_object_to_cell(0, {id}, -1, 1)")).unwrap_err();
    assert!(err.contains("超出范围"), "{err}");
    let err = run_unit(&mut wb, &e, &format!("calc_resize_object_to_range(0, {id}, 5, 1, 2, 3)")).unwrap_err();
    assert!(err.contains("区域不合法"), "{err}");
    let err = run_unit(&mut wb, &e, "calc_resize_object_to_range(99, 1, 0, 0, 1, 1)").unwrap_err();
    assert!(err.contains("越界"), "{err}");
    assert_eq!(get(&mut wb, &e, id), before);
}

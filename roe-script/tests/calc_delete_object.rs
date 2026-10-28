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

fn run_bool(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine, script: &str) -> Result<bool, String> {
    let mut out: Result<bool, String> = Err(String::new());
    with_workbook(wb, || { out = e.eval::<bool>(script).map_err(|x| x.to_string()); });
    out
}

fn ids(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine) -> Vec<u64> {
    let list = run_str(wb, e, "calc_list_objects(0)").unwrap();
    let v: serde_json::Value = serde_json::from_str(&list).unwrap();
    v.as_array().unwrap().iter().map(|o| o["id"].as_u64().unwrap()).collect()
}

#[test]
fn delete_image_then_again() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let before = ids(&mut wb, &e);
    assert_eq!(before.len(), 1);
    let id = before[0];
    assert!(run_bool(&mut wb, &e, &format!("calc_delete_object(0, {id})")).unwrap());
    assert!(ids(&mut wb, &e).is_empty());
    assert!(!run_bool(&mut wb, &e, &format!("calc_delete_object(0, {id})")).unwrap());
    let err = run_str(&mut wb, &e, &format!("calc_get_object(0, {id})")).unwrap_err();
    assert!(err.contains("不存在"), "{err}");
}

#[test]
fn delete_one_shape_keeps_others_and_errors() {
    let mut wb = load("sc_data_xlsx__groupShape.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let before = ids(&mut wb, &e);
    assert_eq!(before.len(), 3);
    assert!(run_bool(&mut wb, &e, &format!("calc_delete_object(0, {})", before[1])).unwrap());
    let after = ids(&mut wb, &e);
    assert_eq!(after, vec![before[0], before[2]]);
    assert!(!run_bool(&mut wb, &e, "calc_delete_object(0, 0)").unwrap());
    assert!(!run_bool(&mut wb, &e, "calc_delete_object(0, 9999)").unwrap());
    let err = run_bool(&mut wb, &e, "calc_delete_object(99, 1)").unwrap_err();
    assert!(err.contains("越界"), "{err}");
}

use roe_model::object_ids::ensure_object_ids;
use roe_parser::ooxml::xlsx::XlsxParser;
use roe_script::calc_api::{register, with_workbook};

fn load(name: &str) -> roe_model::calc::Workbook {
    let path = format!("{}/../test_samples/xlsx/{}", env!("CARGO_MANIFEST_DIR"), name);
    let mut wb = XlsxParser::parse(&std::fs::read(path).unwrap()).unwrap();
    for sh in wb.sheets.iter_mut() { ensure_object_ids(sh); }
    wb
}

#[test]
fn list_image_sample() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let mut out = String::new();
    with_workbook(&mut wb, || { out = e.eval::<String>("calc_list_objects(0)").unwrap(); });
    eprintln!("image_hyperlink => {out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["kind"], "image");
    assert!(arr[0]["id"].as_u64().unwrap() > 0);
}

#[test]
fn list_shapes_and_get_object() {
    let mut wb = load("sc_data_xlsx__groupShape.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let mut out = String::new();
    with_workbook(&mut wb, || { out = e.eval::<String>("calc_list_objects(0)").unwrap(); });
    eprintln!("groupShape => {out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    let ids: Vec<u64> = arr.iter().map(|o| o["id"].as_u64().unwrap()).collect();
    assert!(ids.iter().all(|i| *i > 0));
    assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), 3, "id 应互异");
    let mut one = String::new();
    let script = format!("calc_get_object(0, {})", ids[0]);
    with_workbook(&mut wb, || { one = e.eval::<String>(&script).unwrap(); });
    assert!(one.contains("\"kind\":\"shape\""), "{one}");
    let mut err = String::new();
    with_workbook(&mut wb, || { err = e.eval::<String>("calc_get_object(0, 9999)").unwrap_err().to_string(); });
    assert!(err.contains("不存在") && err.contains("calc_list_objects"), "{err}");
}

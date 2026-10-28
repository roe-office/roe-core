use roe_model::object_ids::ensure_object_ids;
use roe_parser::ooxml::xlsx::XlsxParser;
use roe_script::calc_api::{register, with_workbook};

const CHART_EXAMPLE: &str = r##"{"kind":"Bar","title":"销量","x_labels":["A","B"],"series":[{"name":"s1","data":[1,2]}]}"##;

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

fn run_i64(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine, script: &str) -> Result<i64, String> {
    let mut out: Result<i64, String> = Err(String::new());
    with_workbook(wb, || { out = e.eval::<i64>(script).map_err(|x| x.to_string()); });
    out
}

#[test]
fn chart_id_does_not_collide_with_existing_image() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let script = format!("insert_chart(0, 5, 5, `{}`)", CHART_EXAMPLE);
    let id1 = run_i64(&mut wb, &e, &script).unwrap();
    let id2 = run_i64(&mut wb, &e, &script).unwrap();
    let list = run_str(&mut wb, &e, "calc_list_objects(0)").unwrap();
    let v: serde_json::Value = serde_json::from_str(&list).unwrap();
    let ids: Vec<u64> = v.as_array().unwrap().iter().map(|o| o["id"].as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 3, "{list}");
    let set: std::collections::HashSet<u64> = ids.iter().cloned().collect();
    assert_eq!(set.len(), 3, "id 撞号: {ids:?}");
    assert!(id1 > 1 && id2 > id1, "{id1} {id2}");
}

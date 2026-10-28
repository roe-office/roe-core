use roe_model::object_ids::ensure_object_ids;
use roe_parser::ooxml::xlsx::XlsxParser;
use roe_script::calc_api::{register, with_workbook};

fn load(name: &str) -> roe_model::calc::Workbook {
    let path = format!("{}/../test_samples/xlsx/{}", env!("CARGO_MANIFEST_DIR"), name);
    let mut wb = XlsxParser::parse(&std::fs::read(path).unwrap()).unwrap();
    for sh in wb.sheets.iter_mut() { ensure_object_ids(sh); }
    wb
}

fn run(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine, script: &str) -> Result<String, String> {
    let mut out: Result<String, String> = Err(String::new());
    with_workbook(wb, || {
        out = e.eval::<String>(script).map_err(|x| x.to_string());
    });
    out
}

fn first_id(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine) -> u64 {
    let list = run(wb, e, "calc_list_objects(0)").unwrap();
    let v: serde_json::Value = serde_json::from_str(&list).unwrap();
    v.as_array().unwrap()[0]["id"].as_u64().unwrap()
}

#[test]
fn set_rect_on_image() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let id = first_id(&mut wb, &e);
    let s = format!("calc_set_object_rect(0, {id}, 48.0, 30.0, 300.0, 200.0); calc_get_object(0, {id})");
    let out = run(&mut wb, &e, &s).unwrap();
    eprintln!("image after => {out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["kind"], "image");
    assert_eq!(v["x_pt"].as_f64().unwrap(), 48.0);
    assert_eq!(v["y_pt"].as_f64().unwrap(), 30.0);
    assert_eq!(v["width_pt"].as_f64().unwrap(), 300.0);
    assert_eq!(v["height_pt"].as_f64().unwrap(), 200.0);
}

#[test]
fn set_rect_on_shape_keeps_rotation() {
    let mut wb = load("sc_data_xlsx__groupShape.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let id = first_id(&mut wb, &e);
    let before: serde_json::Value =
        serde_json::from_str(&run(&mut wb, &e, &format!("calc_get_object(0, {id})")).unwrap()).unwrap();
    let s = format!("calc_set_object_rect(0, {id}, 10.0, 20.0, 80.0, 60.0); calc_get_object(0, {id})");
    let after: serde_json::Value = serde_json::from_str(&run(&mut wb, &e, &s).unwrap()).unwrap();
    assert_eq!(after["kind"], "shape");
    assert_eq!(after["width_pt"].as_f64().unwrap(), 80.0);
    assert_eq!(after["height_pt"].as_f64().unwrap(), 60.0);
    assert_eq!(after["rot"], before["rot"]);
}

#[test]
fn errors_are_clear_and_do_not_modify() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let id = first_id(&mut wb, &e);
    let before = run(&mut wb, &e, &format!("calc_get_object(0, {id})")).unwrap();

    let err = run(&mut wb, &e, "calc_set_object_rect(0, 9999, 1.0, 1.0, 10.0, 10.0); \"\"").unwrap_err();
    assert!(err.contains("不存在") && err.contains("calc_list_objects"), "{err}");

    let err = run(&mut wb, &e, &format!("calc_set_object_rect(0, {id}, -1.0, 1.0, 10.0, 10.0); \"\"")).unwrap_err();
    assert!(err.contains("不能为负"), "{err}");

    let err = run(&mut wb, &e, &format!("calc_set_object_rect(0, {id}, 1.0, 1.0, 0.0, 10.0); \"\"")).unwrap_err();
    assert!(err.contains("必须大于 0"), "{err}");

    let err = run(&mut wb, &e, "calc_set_object_rect(99, 1, 1.0, 1.0, 10.0, 10.0); \"\"").unwrap_err();
    assert!(err.contains("越界"), "{err}");

    let err = run(&mut wb, &e, "calc_set_object_rect(0, 0, 1.0, 1.0, 10.0, 10.0); \"\"").unwrap_err();
    assert!(err.contains("正整数"), "{err}");

    let after = run(&mut wb, &e, &format!("calc_get_object(0, {id})")).unwrap();
    assert_eq!(before, after, "报错后对象不应变化");
}

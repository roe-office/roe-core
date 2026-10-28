use base64::Engine as _;
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

fn run_i64(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine, script: &str) -> Result<i64, String> {
    let mut out: Result<i64, String> = Err(String::new());
    with_workbook(wb, || { out = e.eval::<i64>(script).map_err(|x| x.to_string()); });
    out
}

fn run_unit(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine, script: &str) -> Result<(), String> {
    let mut out: Result<(), String> = Err(String::new());
    with_workbook(wb, || { out = e.eval::<()>(script).map_err(|x| x.to_string()); });
    out
}

fn list(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine) -> Vec<serde_json::Value> {
    let s = run_str(wb, e, "calc_list_objects(0)").unwrap();
    serde_json::from_str::<serde_json::Value>(&s).unwrap().as_array().unwrap().clone()
}

#[test]
fn insert_shape_textbox_and_set_text() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let a = run_i64(&mut wb, &e, "calc_insert_shape(0, \"ellipse\", 10.0, 20.0, 120.0, 80.0, \"#ff0000\")").unwrap();
    let b = run_i64(&mut wb, &e, "calc_insert_textbox(0, 48.0, 30.0, 200.0, 60.0, \"你好\")").unwrap();
    assert!(a > 1 && b > a, "{a} {b}");
    run_unit(&mut wb, &e, &format!("calc_set_object_text(0, {a}, \"第一行\\n第二行\")")).unwrap();
    let one = run_str(&mut wb, &e, &format!("calc_get_object(0, {a})")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&one).unwrap();
    assert_eq!(v["kind"], "shape");
    assert_eq!(v["sub"], "ellipse");
    assert_eq!(v["text"], "第一行\n第二行");
    assert_eq!(list(&mut wb, &e).len(), 3);
}

#[test]
fn errors_are_clear_and_do_not_modify() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let img_id = list(&mut wb, &e)[0]["id"].as_u64().unwrap();
    let err = run_i64(&mut wb, &e, "calc_insert_shape(0, \"notashape\", 0.0, 0.0, 10.0, 10.0, \"\")").unwrap_err();
    assert!(err.contains("未知图形预设名"), "{err}");
    let err = run_i64(&mut wb, &e, "calc_insert_shape(99, \"rect\", 0.0, 0.0, 10.0, 10.0, \"\")").unwrap_err();
    assert!(err.contains("越界"), "{err}");
    let err = run_i64(&mut wb, &e, "calc_insert_textbox(0, 0.0, 0.0, 0.0, 10.0, \"x\")").unwrap_err();
    assert!(err.contains("必须大于 0"), "{err}");
    let err = run_unit(&mut wb, &e, &format!("calc_set_object_text(0, {img_id}, \"x\")")).unwrap_err();
    assert!(err.contains("没有文字"), "{err}");
    let err = run_unit(&mut wb, &e, "calc_set_object_text(0, 9999, \"x\")").unwrap_err();
    assert!(err.contains("不存在"), "{err}");
    assert_eq!(list(&mut wb, &e).len(), 1);
}

#[test]
fn xlsx_roundtrip_keeps_new_shapes_and_text() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let a = run_i64(&mut wb, &e, "calc_insert_shape(0, \"heart\", 10.0, 20.0, 120.0, 80.0, \"#ff0000\")").unwrap();
    run_unit(&mut wb, &e, &format!("calc_set_object_text(0, {a}, \"第一行\\n第二行\")")).unwrap();
    run_i64(&mut wb, &e, "calc_insert_textbox(0, 48.0, 30.0, 200.0, 60.0, \"你好\")").unwrap();

    let b64 = run_str(&mut wb, &e, "save_xlsx()").unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).unwrap();
    let mut wb2 = XlsxParser::parse(&bytes).unwrap();
    for sh in wb2.sheets.iter_mut() { ensure_object_ids(sh); }
    let objs = list(&mut wb2, &e);
    eprintln!("reloaded => {objs:?}");
    assert_eq!(objs.len(), 3, "图片 + 两个图形都应在");
    assert!(objs.iter().any(|o| o["kind"] == "image"));
    assert!(objs.iter().any(|o| o["sub"] == "heart" && o["text"] == "第一行\n第二行"));
    let tb = objs.iter().find(|o| o["text"] == "你好").expect("文本框文字丢失");
    assert_eq!(tb["sub"], "rect");
    assert!((tb["x_pt"].as_f64().unwrap() - 48.0).abs() < 0.5);
    assert!((tb["y_pt"].as_f64().unwrap() - 30.0).abs() < 0.5);
}

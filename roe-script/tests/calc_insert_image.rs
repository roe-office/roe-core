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

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);
    v
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

fn count(wb: &mut roe_model::calc::Workbook, e: &rhai::Engine) -> usize {
    let list = run_str(wb, e, "calc_list_objects(0)").unwrap();
    serde_json::from_str::<serde_json::Value>(&list).unwrap().as_array().unwrap().len()
}

#[test]
fn insert_base64_default_size_and_read_back() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    assert_eq!(count(&mut wb, &e), 1);
    let b = base64::engine::general_purpose::STANDARD.encode(png(400, 200));
    let script = format!("calc_insert_image(0, \"base64:{b}\", 10.0, 20.0, 0.0, 0.0)");
    let id = run_i64(&mut wb, &e, &script).unwrap();
    assert!(id > 1, "新 id 不得与已有对象重复: {id}");
    assert_eq!(count(&mut wb, &e), 2);
    let one = run_str(&mut wb, &e, &format!("calc_get_object(0, {id})")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&one).unwrap();
    assert_eq!(v["kind"], "image");
    assert_eq!(v["mime"], "image/png");
    assert_eq!(v["width_pt"].as_f64().unwrap(), 200.0);
    assert_eq!(v["height_pt"].as_f64().unwrap(), 100.0);
    assert_eq!(v["x_pt"].as_f64().unwrap(), 10.0);
}

#[test]
fn insert_from_path() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let p = std::env::temp_dir().join("roe_calc_insert_image_e2e.png");
    std::fs::write(&p, png(100, 300)).unwrap();
    let script = format!("calc_insert_image(0, \"{}\", 0.0, 0.0, 50.0, 0.0)", p.to_str().unwrap());
    let id = run_i64(&mut wb, &e, &script).unwrap();
    let _ = std::fs::remove_file(&p);
    let one = run_str(&mut wb, &e, &format!("calc_get_object(0, {id})")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&one).unwrap();
    assert_eq!(v["width_pt"].as_f64().unwrap(), 50.0);
    assert_eq!(v["height_pt"].as_f64().unwrap(), 150.0);
}

#[test]
fn insert_errors_do_not_modify() {
    let mut wb = load("sc_data_xlsx__image_hyperlink.xlsx");
    let mut e = rhai::Engine::new();
    register(&mut e);
    let bad = base64::engine::general_purpose::STANDARD.encode(b"hello world, not an image at all");
    let err = run_i64(&mut wb, &e, &format!("calc_insert_image(0, \"base64:{bad}\", 0.0, 0.0, 0.0, 0.0)")).unwrap_err();
    assert!(err.contains("PNG/JPEG"), "{err}");
    let ok = base64::engine::general_purpose::STANDARD.encode(png(10, 10));
    let err = run_i64(&mut wb, &e, &format!("calc_insert_image(99, \"base64:{ok}\", 0.0, 0.0, 0.0, 0.0)")).unwrap_err();
    assert!(err.contains("越界"), "{err}");
    let err = run_i64(&mut wb, &e, &format!("calc_insert_image(0, \"base64:{ok}\", -1.0, 0.0, 0.0, 0.0)")).unwrap_err();
    assert!(err.contains("不能为负"), "{err}");
    assert_eq!(count(&mut wb, &e), 1);
}

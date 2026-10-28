//! Writer meta 防漂移测试:WRITER_API_META 必须与 writer_api::register 实际注册名单一致。
use crate::calc_api_meta::CALC_API_META;
use crate::calc_df_meta::DF_API_META;
use crate::writer_api_meta::WRITER_API_META;
use rhai::Engine;
use std::collections::BTreeSet;

fn registered(e: &Engine) -> BTreeSet<String> {
    e.gen_fn_signatures(false)
        .into_iter()
        .map(|s| s.split('(').next().unwrap_or("").trim().to_string())
        .collect()
}

fn writer_names() -> BTreeSet<String> {
    WRITER_API_META.iter().map(|m| m.0.to_string()).collect()
}

#[test]
fn writer_meta_matches_engine() {
    let mut e = Engine::new();
    crate::writer_api::register(&mut e);
    let (reg, meta) = (registered(&e), writer_names());
    assert_eq!(reg, meta, "仅注册有: {:?}; 仅清单有: {:?}",
        reg.difference(&meta).collect::<Vec<_>>(), meta.difference(&reg).collect::<Vec<_>>());
}

#[test]
fn writer_meta_names_unique() {
    assert_eq!(WRITER_API_META.len(), writer_names().len(), "WRITER_API_META 有重名");
}

/// Writer 与 Calc/DF/Impress 共用一个 Engine(ScriptEngine::new),同名会互相覆盖。
#[test]
fn writer_meta_no_collision_with_other_apis() {
    let mut imp = Engine::new();
    crate::impress_api::register(&mut imp);
    let mut others: BTreeSet<String> = CALC_API_META.iter().map(|m| m.0.to_string()).collect();
    others.extend(DF_API_META.iter().map(|m| m.0.to_string()));
    others.extend(registered(&imp));
    let w = writer_names();
    let clash: Vec<_> = w.intersection(&others).collect();
    assert!(clash.is_empty(), "Writer 与其他接口重名: {:?}", clash);
}

const READ_NAMES: [&str; 22] = [
    "get_writer_formula",
    "paragraph_count", "get_paragraph", "get_paragraph_style", "list_paragraph_styles",
    "get_char_style", "list_char_styles", "get_char_format", "get_char_format_range",
    "get_paragraph_format", "get_frame_wrap", "section_count", "get_section", "save_docx",
    "inspect_document", "get_table_cell", "find_text",
    "get_page_setup", "get_header_footer", "get_comment_threads", "get_object_rotation", "get_writer_chart_json",
];

#[test]
fn writer_readonly_exact_names() {
    let mut e = Engine::new();
    crate::writer_api::register_readonly(&mut e);
    let mut want: BTreeSet<String> = READ_NAMES.iter().map(|s| s.to_string()).collect();
    let reg = registered(&e);
    // register_readonly 另注册一个只读版 list_functions(不在 WRITER_API_META 里)
    want.insert("list_functions".to_string());
    assert_eq!(reg, want, "仅注册有: {:?}; 仅白名单有: {:?}",
        reg.difference(&want).collect::<Vec<_>>(), want.difference(&reg).collect::<Vec<_>>());
    let reads: BTreeSet<String> = READ_NAMES.iter().map(|s| s.to_string()).collect();
    assert!(reads.is_subset(&writer_names()), "白名单含不在 meta 里的函数");
}

#[test]
fn writer_readonly_engine_rejects_writes_and_allows_reads() {
    let eng = crate::engine::ScriptEngine::new_readonly_writer();
    let mut doc = roe_model::writer::WriterDocument::default();
    let err = eng.run_writer("set_paragraph(0, \"x\")", &mut doc).unwrap_err();
    assert!(err.contains("Function not found"), "写函数应未注册: {err}");
    let err = eng.run_writer("append_paragraph(\"x\")", &mut doc).unwrap_err();
    assert!(err.contains("Function not found"), "写函数应未注册: {err}");
    let n = eng.run_writer("paragraph_count()", &mut doc).unwrap();
    assert_eq!(n.as_i64(), Some(0), "读函数应可用: {n}");
}

#[test]
fn writer_readonly_names_match_const() {
    let want: BTreeSet<String> = crate::writer_api::READ_FN_NAMES.iter().map(|s| s.to_string()).collect();
    let mut e = Engine::new();
    crate::writer_api::register_read_fns_for_test(&mut e);
    assert_eq!(registered(&e), want, "READ_FN_NAMES 与 register_read_fns 不一致");
}

#[test]
fn writer_readonly_list_functions() {
    let eng = crate::engine::ScriptEngine::new_readonly_writer();
    let mut doc = roe_model::writer::WriterDocument::default();
    let all = eng.run_writer("list_functions(\"\")", &mut doc).unwrap();
    let all = all.as_str().unwrap().to_string();
    assert!(all.contains("get_paragraph") && all.contains("save_docx"), "应列出只读函数: {all}");
    let tagged = |name: &str, tag: &str| all.contains(&format!("{name}{tag}")) || all.contains(&format!("{name}*{tag}"));
    assert!(tagged("get_paragraph", "[r]"), "get_paragraph 应标 [r]: {all}");
    assert!(tagged("paragraph_count", "[r]"), "paragraph_count 应标 [r]: {all}");
    assert!(tagged("set_paragraph", "[w]"), "set_paragraph 应标 [w]: {all}");
    assert!(tagged("append_paragraph", "[w]"), "append_paragraph 应标 [w]: {all}");
    assert!(!tagged("set_paragraph", "[r]"), "set_paragraph 不应标 [r]: {all}");
    assert!(!all.contains("set_cell"), "不应列出 Calc 函数: {all}");
}

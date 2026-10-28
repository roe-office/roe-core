//! Impress meta 防漂移测试:IMPRESS_API_META 必须与 impress_api::register 实际注册名单一致。
use crate::calc_api_meta::CALC_API_META;
use crate::calc_df_meta::DF_API_META;
use crate::impress_api_meta::IMPRESS_API_META;
use crate::writer_api_meta::WRITER_API_META;
use rhai::Engine;
use std::collections::BTreeSet;

fn registered(e: &Engine) -> BTreeSet<String> {
    e.gen_fn_signatures(false)
        .into_iter()
        .map(|s| s.split('(').next().unwrap_or("").trim().to_string())
        .collect()
}

fn impress_names() -> BTreeSet<String> {
    IMPRESS_API_META.iter().map(|m| m.0.to_string()).collect()
}

#[test]
fn impress_meta_matches_engine() {
    let mut e = Engine::new();
    crate::impress_api::register(&mut e);
    let (reg, meta) = (registered(&e), impress_names());
    assert_eq!(reg, meta, "仅注册有: {:?}; 仅清单有: {:?}",
        reg.difference(&meta).collect::<Vec<_>>(), meta.difference(&reg).collect::<Vec<_>>());
}

#[test]
fn impress_meta_names_unique() {
    assert_eq!(IMPRESS_API_META.len(), impress_names().len(), "IMPRESS_API_META 有重名");
}

/// Impress 与 Calc/DF/Writer 共用一个 Engine(ScriptEngine::new),同名会互相覆盖。
#[test]
fn impress_meta_no_collision_with_other_apis() {
    let mut others: BTreeSet<String> = CALC_API_META.iter().map(|m| m.0.to_string()).collect();
    others.extend(DF_API_META.iter().map(|m| m.0.to_string()));
    others.extend(WRITER_API_META.iter().map(|m| m.0.to_string()));
    let i = impress_names();
    let clash: Vec<_> = i.intersection(&others).collect();
    assert!(clash.is_empty(), "Impress 与其他接口重名: {:?}", clash);
}

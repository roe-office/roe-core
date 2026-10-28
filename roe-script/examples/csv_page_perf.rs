//! 单进程单操作对比: MODE=preview|page0|pagemid  CSV=big1m.csv
use roe_model::calc::{Sheet, Workbook};
use roe_script::calc_df_api::{set_csv_cache_dir, DatasetStore};
use roe_script::engine::ScriptEngine;
use std::time::Instant;

const DATA: &str = "/home/yong/roe_perf_data";

fn status_kb(key: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(rest) = l.strip_prefix(key) {
            return rest.trim_start_matches(':').trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
        }
    }
    0
}

fn main() {
    set_csv_cache_dir(Some(format!("{DATA}/cache").into()));
    let eng = ScriptEngine::new();
    let mut wb = Workbook {
        sheets: vec![Sheet { name: "Sheet1".into(), ..Default::default() }],
        named_ranges: Default::default(),
        named_formulas: Default::default(),
        styles: roe_model::style::StyleRegistry::default(),
        formula_dialect_source: Default::default(),
    };
    let mut ds = DatasetStore::new();
    let name = std::env::var("CSV").unwrap_or_else(|_| "big1m.csv".into());
    let mode = std::env::var("MODE").unwrap_or_else(|_| "preview".into());
    let load = format!(r#"df_load_csv_cached("{DATA}/{name}", "", true)"#);
    let op = match mode.as_str() {
        "preview" => "df_preview(5)",
        "page0" => "df_page(0, 5)",
        "pagemid" => "df_page(500000, 5)",
        _ => panic!("MODE 无效"),
    };
    // 预热:建缓存(若缺),不计入对比
    let script = format!("let a = {load}; a");
    eng.run_calc_ds(&script, &mut wb, &mut ds).expect("warm");

    // 每个脚本结束 WORKING_DF 会清,所以 load+op 必须同一脚本
    let full = format!("let a = {load}; let b = {op}; b");
    // 自检: clear_refs 后 VmHWM 是否回落到 RSS 附近
    let before = status_kb("VmHWM");
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let after = status_kb("VmHWM");
    println!("[自检] VmHWM {} -> {} MB, VmRSS {} MB", before / 1024, after / 1024, status_kb("VmRSS") / 1024);
    let rss0 = status_kb("VmRSS");
    let t = Instant::now();
    let r = eng.run_calc_ds(&full, &mut wb, &mut ds);
    let ms = t.elapsed().as_millis();
    let hwm = status_kb("VmHWM");
    let rss1 = status_kb("VmRSS");
    match r {
        Ok((_, v)) => {
            let s = v.to_string();
            println!("[{mode}] {ms} ms | 峰值 {} MB | RSS {} -> {} MB", hwm / 1024, rss0 / 1024, rss1 / 1024);
            println!("[{mode}] 返回: {}", s.chars().take(2000).collect::<String>());
        }
        Err(e) => println!("[{mode}] 脚本错误: {e}"),
    }
}

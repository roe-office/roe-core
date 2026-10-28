//! df_load_csv_cached 端到端验证(v2:每个场景在同一脚本内完成)。
//! 用法: CSV=big1m.csv cargo run -p roe-script --example csv_cache_perf
use roe_model::calc::{Sheet, Workbook};
use roe_script::calc_df_api::{set_csv_cache_dir, DatasetStore};
use roe_script::engine::ScriptEngine;
use std::path::PathBuf;
use std::time::Instant;

const DATA: &str = "/home/yong/roe_perf_data";

fn empty_wb() -> Workbook {
    Workbook {
        sheets: vec![Sheet { name: "Sheet1".into(), ..Default::default() }],
        named_ranges: Default::default(),
        named_formulas: Default::default(),
        styles: roe_model::style::StyleRegistry::default(),
        formula_dialect_source: Default::default(),
    }
}

fn status_kb(key: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(rest) = l.strip_prefix(key) {
            return rest.trim_start_matches(':').trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
        }
    }
    0
}

fn reset_peak() {
    if let Err(e) = std::fs::write("/proc/self/clear_refs", "5") {
        println!("  [警告] 重置 VmHWM 失败: {e}");
    }
}

fn cut(s: &str) -> String {
    s.chars().take(500).collect()
}

fn step(eng: &ScriptEngine, wb: &mut Workbook, ds: &mut DatasetStore, label: &str, script: &str) {
    reset_peak();
    let rss0 = status_kb("VmRSS");
    let t = Instant::now();
    let r = eng.run_calc_ds(script, wb, ds);
    let ms = t.elapsed().as_millis();
    let hwm = status_kb("VmHWM");
    let rss1 = status_kb("VmRSS");
    match r {
        Ok((_, v)) => println!(
            "[{label}] {ms} ms | 峰值 {} MB | RSS {} -> {} MB | 返回: {}",
            hwm / 1024, rss0 / 1024, rss1 / 1024, cut(&v.to_string())
        ),
        Err(e) => println!("[{label}] {ms} ms | 脚本错误: {}", cut(&e)),
    }
}

fn list_cache(dir: &PathBuf) {
    let (mut n, mut total) = (0, 0u64);
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            n += 1;
            total += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    println!("  缓存目录: {} 个文件, 共 {} MB", n, total / 1024 / 1024);
}

fn main() {
    let cache = PathBuf::from(format!("{DATA}/cache"));
    let _ = std::fs::remove_dir_all(&cache);
    set_csv_cache_dir(Some(cache.clone()));

    let eng = ScriptEngine::new();
    let mut wb = empty_wb();
    let mut ds = DatasetStore::new();
    let name = std::env::var("CSV").unwrap_or_else(|_| "big.csv".into());
    let big = format!("{DATA}/{name}");

    let s_prev = format!(r#"let a = df_load_csv_cached("{big}", "", true); let b = df_preview(5); a + " | " + b"#);
    let s_sql = format!(r#"let a = df_load_csv_cached("{big}", "", true); let b = df_sql("t", "select count(*) as n, sum(price) as s from t"); a + " | " + b"#);
    let s_save = format!(r#"let a = df_load_csv_cached("{big}", "", true); let b = df_save_as("t"); a + " | " + b"#);
    let s_gbk = format!(r#"let a = df_load_csv_cached("{DATA}/gbk_small.csv", "", true); let b = df_preview(5); a + " | " + b"#);

    println!("=== 轮1: 冷导入 + preview5 (单脚本) ===");
    step(&eng, &mut wb, &mut ds, "load+preview", &s_prev);
    list_cache(&cache);

    println!("=== 轮2: 命中 + preview5 ===");
    step(&eng, &mut wb, &mut ds, "load+preview", &s_prev);

    println!("=== 轮3: 命中 + sql全表聚合 ===");
    step(&eng, &mut wb, &mut ds, "load+sql", &s_sql);

    println!("=== 轮4: touch 后失效重建 + preview5 ===");
    let f = std::fs::OpenOptions::new().write(true).open(&big).expect("open csv");
    f.set_modified(std::time::SystemTime::now()).expect("set_modified");
    drop(f);
    step(&eng, &mut wb, &mut ds, "load+preview", &s_prev);
    list_cache(&cache);

    println!("=== 轮5: 跨脚本 (df_save_as 物化 -> 另一脚本 df_use) ===");
    step(&eng, &mut wb, &mut ds, "load+save_as", &s_save);
    step(&eng, &mut wb, &mut ds, "list_datasets", "df_list_datasets()");
    step(&eng, &mut wb, &mut ds, "use+preview", r#"let a = df_use("t"); let b = df_preview(5); a + " | " + b"#);

    println!("=== 轮6: GBK 内容检查 ===");
    step(&eng, &mut wb, &mut ds, "gbk load+preview", &s_gbk);
    println!("=== 完成 ===");
}

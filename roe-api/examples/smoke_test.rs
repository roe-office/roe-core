//! 冒烟测试：遍历 curated 目录下所有精选样本文件，尝试用对应的 open_*_bytes 解析，
//! 捕获 panic，只判断"能否成功打开/不崩溃"，不检查渲染正确性。
//! 用法: cargo run -p roe-api --example smoke_test -- <curated目录路径> <结果输出csv路径>

use std::fs;
use std::panic;
use std::path::Path;
use std::time::Instant;

fn ext_lower(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn try_open(ext: &str, data: &[u8]) -> Result<String, String> {
    match ext {
        "docx" | "docm" | "dotx" | "dotm" => {
            roe_parser::RoeDocument::open_docx_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "odt" | "ott" | "sxw" | "odm" => {
            roe_parser::RoeDocument::open_odf_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "fodt" => {
            roe_parser::RoeDocument::open_fodt_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "xlsx" | "xlsm" | "xlsb" => {
            roe_parser::RoeDocument::open_xlsx_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "ods" => {
            roe_parser::RoeDocument::open_odf_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "fods" => {
            roe_parser::RoeDocument::open_fods_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "pptx" | "pptm" | "ppsx" | "potx" => {
            roe_parser::RoeDocument::open_pptx_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "odp" => {
            roe_parser::RoeDocument::open_odf_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "fodp" => {
            roe_parser::RoeDocument::open_fodp_bytes(data)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        "csv" => {
            roe_parser::RoeDocument::open_csv_bytes(data, None)
                .map(|_| "ok".to_string())
                .map_err(|e| format!("parse_err: {e}"))
        }
        other => Err(format!("unknown_ext: {other}")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let curated_dir = args.get(1).cloned().unwrap_or_else(|| {
        format!("{}/Projects/roe/test_samples/curated", std::env::var("HOME").unwrap())
    });
    let out_csv = args.get(2).cloned().unwrap_or_else(|| "/tmp/smoke_test_result.csv".to_string());

    // 屏蔽 panic 时打印的默认堆栈信息到 stderr（我们自己会记录），减少刷屏
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let mut rows: Vec<(String, String, String, String, u128)> = Vec::new();
    // (format_folder, filename, status, detail, elapsed_ms)

    let format_folders = ["docx", "odt", "xlsx", "ods", "pptx", "odp", "csv"];

    let mut total = 0usize;
    let mut ok_count = 0usize;
    let mut err_count = 0usize;
    let mut panic_count = 0usize;

    for folder in format_folders {
        let dir_path = Path::new(&curated_dir).join(folder);
        if !dir_path.is_dir() {
            continue;
        }
        let mut entries: Vec<_> = fs::read_dir(&dir_path)
            .unwrap_or_else(|_| panic!("cannot read dir {:?}", dir_path))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        entries.sort();

        for path in entries {
            total += 1;
            let ext = ext_lower(&path);
            let fname = path.file_name().unwrap().to_string_lossy().to_string();

            let data = match fs::read(&path) {
                Ok(d) => d,
                Err(e) => {
                    rows.push((folder.to_string(), fname, "io_err".to_string(), e.to_string(), 0));
                    err_count += 1;
                    continue;
                }
            };

            let start = Instant::now();
            let ext_clone = ext.clone();
            let data_clone = data.clone();
            let result = panic::catch_unwind(move || try_open(&ext_clone, &data_clone));
            let elapsed = start.elapsed().as_millis();

            match result {
                Ok(Ok(_)) => {
                    rows.push((folder.to_string(), fname, "ok".to_string(), String::new(), elapsed));
                    ok_count += 1;
                }
                Ok(Err(detail)) => {
                    rows.push((folder.to_string(), fname, "parse_error".to_string(), detail, elapsed));
                    err_count += 1;
                }
                Err(panic_payload) => {
                    let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic payload".to_string()
                    };
                    rows.push((folder.to_string(), fname, "PANIC".to_string(), msg, elapsed));
                    panic_count += 1;
                }
            }

            if total % 50 == 0 {
                eprintln!("progress: {total} files processed...");
            }
        }
    }

    panic::set_hook(default_hook);

    // 写 CSV
    let mut csv_content = String::from("format,filename,status,detail,elapsed_ms\n");
    for (fmt, fname, status, detail, ms) in &rows {
        let detail_escaped = detail.replace('"', "'").replace('\n', " ").replace('\r', "");
        csv_content.push_str(&format!(
            "\"{fmt}\",\"{fname}\",\"{status}\",\"{detail_escaped}\",{ms}\n"
        ));
    }
    fs::write(&out_csv, csv_content).expect("write csv");

    println!("\n=== 冒烟测试完成 ===");
    println!("总文件数: {total}");
    println!("成功(ok): {ok_count}");
    println!("解析错误(parse_error/io_err): {err_count}");
    println!("PANIC(崩溃): {panic_count}");
    println!("详细结果: {out_csv}");
}

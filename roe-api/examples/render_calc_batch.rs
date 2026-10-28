use std::collections::HashSet;
use std::path::Path;
use roe_api::test_support::open_calc_for_test as open_calc;
use roe_core::tile_geometry::{col_x_px, row_y_px};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input_dir = args.get(1).expect("usage: render_calc_batch <input_dir> <output_dir> <progress_file> [count]");
    let output_dir = args.get(2).expect("usage: render_calc_batch <input_dir> <output_dir> <progress_file> [count]");
    let progress_file = args.get(3).expect("usage: render_calc_batch <input_dir> <output_dir> <progress_file> [count]");
    let count: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10);

    std::fs::create_dir_all(output_dir).expect("create output dir");

    let done: HashSet<String> = std::fs::read_to_string(progress_file)
        .unwrap_or_default()
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut entries: Vec<String> = std::fs::read_dir(input_dir)
        .expect("read input dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();
    entries.sort();

    let remaining: Vec<&String> = entries
        .iter()
        .filter(|p| {
            let stem = Path::new(p).file_stem().unwrap().to_string_lossy().to_string();
            !done.contains(&stem)
        })
        .collect();

    println!("total: {}, already done: {}, remaining: {}", entries.len(), done.len(), remaining.len());

    let batch: Vec<&&String> = remaining.iter().take(count).collect();

    let mut newly_done = Vec::new();

    for path in batch {
        let filename = Path::new(*path).file_stem().unwrap().to_string_lossy().to_string();
        println!("=== {} ===", filename);
        // v121: 每个文件单独一个子文件夹，方便人工核对时源文件和渲染图放在一起，
        // 不用再按文件名去大目录里搜。
        let file_folder = format!("{}/{}", output_dir, filename);
        std::fs::create_dir_all(&file_folder).expect("create per-file output folder");
        // 把源文件本身也拷贝进这个子文件夹
        let src_path = Path::new(*path);
        if let Some(ext) = src_path.extension() {
            let dest = format!("{}/{}.{}", file_folder, filename, ext.to_string_lossy());
            if let Err(e) = std::fs::copy(src_path, &dest) {
                println!("  WARN: failed to copy source file into folder: {}", e);
            }
        }
        let wb = match open_calc(path) {
            Ok(wb) => wb,
            Err(e) => {
                println!("  SKIP (parse/unsupported): {}", e);
                newly_done.push(filename);
                continue;
            }
        };
        if wb.sheets.is_empty() {
            println!("  SKIP: no sheets");
            newly_done.push(filename);
            continue;
        }
        println!("  sheet count: {}", wb.sheets.len());
        let max_cols = 20u32;
        let max_rows = 50u32;
        for i in 0..wb.sheets.len() {
            let sheet_name = wb.sheets[i].name.clone();
            let mut renderer = roe_core::tile::CalcTileRenderer::new(&wb);
            renderer.sheet_idx = i;
            // v122: 迁移至 render_viewport_into（roe-ext_progress_v114第二步），
            // 用 scroll=0/zoom=100/header=0 模拟"无滚动无缩放无标题栏的整表渲染"，
            // 与原 render_sheet_preview 语义等价。vw/vh 换算逻辑复用
            // verify_viewport_migration.rs 中已验证过、对所有测试样本
            // size_match=true 的实现。
            let sheet = &wb.sheets[i];
            let cfg = &renderer.config;
            let vw = col_x_px(sheet, max_cols, cfg) as u32 + 1;
            let vh = row_y_px(sheet, max_rows, cfg) as u32 + 1;
            let mut buf = vec![0u8; (vw as usize) * (vh as usize) * 4];
            let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, 0.0, 0.0, &mut buf);
            match ok {
                true => {
                    let pixmap = tiny_skia::Pixmap::from_vec(
                        buf,
                        tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
                    ).unwrap();
                    let png = pixmap.encode_png().unwrap();
                    // ROOT-018修复：ods外部引用型sheet名（如含file:///...路径和转义引号的
                    // 交叉工作簿引用名）可能长达数百字符，直接拼进文件名会超出文件系统255字节
                    // 上限导致std::fs::write panic（InvalidFilename, code 36）。
                    // 超长/含特殊字符时退化为短哈希，保留原名可读性但保证文件系统安全。
                    let mut safe_name = sheet_name
                        .replace('/', "_")
                        .replace('\\', "_")
                        .replace("&apos;", "")
                        .replace(':', "_")
                        .replace('#', "_");
                    const MAX_SAFE_NAME_LEN: usize = 60;
                    if safe_name.len() > MAX_SAFE_NAME_LEN || safe_name.is_empty() {
                        use std::collections::hash_map::DefaultHasher;
                        use std::hash::{Hash, Hasher};
                        let mut hasher = DefaultHasher::new();
                        sheet_name.hash(&mut hasher);
                        let short_hash = hasher.finish();
                        let truncated: String = safe_name.chars().take(30).collect();
                        safe_name = format!("{}_h{:x}", truncated, short_hash);
                    }
                    let out_path = format!("{}/{}__sheet{}_{}.png", file_folder, filename, i, safe_name);
                    std::fs::write(&out_path, &png).unwrap();
                    println!("  sheet {} ({}) -> {} ({}x{})", i, sheet_name, out_path, vw, vh);
                }
                false => {
                    println!("  sheet {} ({}): SKIP render_viewport_into returned false", i, sheet_name);
                }
            }
        }
        newly_done.push(filename);
    }

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(progress_file)
        .expect("open progress file");
    use std::io::Write;
    for name in &newly_done {
        writeln!(f, "{}", name).unwrap();
    }
    println!("marked {} files as done in {}", newly_done.len(), progress_file);
}

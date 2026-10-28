use std::collections::HashSet;
use std::path::Path;
use roe_writer::{layout::ParagraphLayoutEngine, tile::WriterTileRenderer};
use roe_writer::FontRegistry;
use roe_parser::RoeDocument;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    const USAGE: &str = "usage: render_docx_batch <input_dir> <progress_file> [output_dir] [count]";
    let input_dir = args.get(1).expect(USAGE);
    let progress_file = args.get(2).expect(USAGE);
    // output_dir 不传时默认落在系统临时目录下的固定子目录，关机自动清空，
    // 不需要每次手动记得传 /tmp/xxx（避免下次session交接时忘记这个约定）。
    let default_output_dir = std::env::temp_dir().join("roe_docx_render").to_string_lossy().to_string();
    let output_dir: &str = args.get(3).map(|s| s.as_str()).unwrap_or(&default_output_dir);
    let count: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10);
    println!("output_dir: {}", output_dir);

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
        .filter(|p| p.to_lowercase().ends_with(".docx"))
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

        let file_folder = format!("{}/{}", output_dir, filename);
        std::fs::create_dir_all(&file_folder).expect("create per-file output folder");
        let src_path = Path::new(*path);
        if let Some(ext) = src_path.extension() {
            let dest = format!("{}/{}.{}", file_folder, filename, ext.to_string_lossy());
            if let Err(e) = std::fs::copy(src_path, &dest) {
                println!("  WARN: failed to copy source file into folder: {}", e);
            }
        }

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                println!("  SKIP (read failed): {:?}", e);
                newly_done.push(filename);
                continue;
            }
        };

        let doc = match RoeDocument::open_docx_bytes(&data) {
            Ok(d) => d,
            Err(e) => {
                println!("  SKIP (parse failed): {:?}", e);
                newly_done.push(filename);
                continue;
            }
        };

        let wdoc = match doc {
            RoeDocument::Writer(d) => d,
            _ => {
                println!("  SKIP: not a Writer document");
                newly_done.push(filename);
                continue;
            }
        };

        println!("  body elements: {}", wdoc.body.len());

        let registry = FontRegistry::new_with_system_fonts();
        let mut engine = ParagraphLayoutEngine::new(registry);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.layout(&wdoc)));
        let result = match result {
            Ok(r) => r,
            Err(_) => {
                println!("  SKIP: layout panicked");
                newly_done.push(filename);
                continue;
            }
        };
        let arena = &result.arena;
        println!("  pages: {}", arena.page_order.len());

        // [FIX 2026-09-14] 此前这里的注释"主路径用默认空FontContext即可"是
        // 错误判断——空FontContext不含KaTeX字体，导致本脚本批量渲染的所有
        // 带公式用例，字体全部静默fallback到系统字体，历史上用本脚本做的
        // D/E类用例核实，如果某个用例含公式，核实结论可能不可靠，需要
        // 重新核实一次（不影响不含公式的用例）。
        // 现在统一走FontRegistry::build_font_context_with_katex()，与
        // render_one.rs保持一致。
        let mut renderer = WriterTileRenderer::new_with_fonts(
            roe_writer::FontRegistry::build_font_context_with_katex(), (),
        );
        for (i, &pid) in arena.page_order.iter().enumerate().take(10) {
            let page = &arena.pages[pid];
            let scale = 2.0f32; // 临时验证值，与render_one.rs对齐；正式默认值待讨论（v25遗留问题1）
            let pw = (page.width * scale) as u32;
            let ph = (page.height * scale) as u32;
            if pw == 0 || ph == 0 {
                println!("  page {}: SKIP invalid size ({}x{})", i, pw, ph);
                continue;
            }
            let vp_y = arena.page_global_y(pid);
            let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
            match renderer.render_viewport(arena, &wdoc, 0.0, vp_y, page.width, page.height, pw, ph, &mut buf) {
                Ok(_) => {
                    let pixmap = match tiny_skia::Pixmap::from_vec(
                        buf, tiny_skia::IntSize::from_wh(pw, ph).unwrap()
                    ) {
                        Some(p) => p,
                        None => {
                            println!("  page {}: SKIP pixmap construction failed", i);
                            continue;
                        }
                    };
                    let png = pixmap.encode_png().unwrap();
                    let out_path = format!("{}/{}__page{}.png", file_folder, filename, i);
                    std::fs::write(&out_path, &png).unwrap();
                    println!("  page {} -> {} ({}x{})", i, out_path, pw, ph);
                }
                Err(e) => {
                    println!("  page {}: SKIP render failed: {:?}", i, e);
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

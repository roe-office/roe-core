use std::collections::HashSet;
use std::path::Path;
use roe_impress::slide_renderer::SlideRenderer;
use roe_parser::RoeDocument;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input_dir = args.get(1).expect("usage: render_pptx_batch <input_dir> <output_dir> <progress_file> [count]");
    let output_dir = args.get(2).expect("usage: render_pptx_batch <input_dir> <output_dir> <progress_file> [count]");
    let progress_file = args.get(3).expect("usage: render_pptx_batch <input_dir> <output_dir> <progress_file> [count]");
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

        // 仿照calc侧v121：每个文件单独一个子文件夹，源文件和渲染图放一起，
        // 人工核对时不用去大目录里找源文件。
        let file_folder = format!("{}/{}", output_dir, filename);
        std::fs::create_dir_all(&file_folder).expect("create per-file output folder");
        let src_path = Path::new(*path);
        if let Some(ext) = src_path.extension() {
            let dest = format!("{}/{}.{}", file_folder, filename, ext.to_string_lossy());
            if let Err(e) = std::fs::copy(src_path, &dest) {
                println!("  WARN: failed to copy source file into folder: {}", e);
            }
        }

        let doc = match RoeDocument::open_pptx_file(path) {
            Ok(d) => d,
            Err(e) => {
                println!("  SKIP (parse failed): {:?}", e);
                newly_done.push(filename);
                continue;
            }
        };

        let pres = match doc {
            RoeDocument::Impress(p) => p,
            _ => {
                println!("  SKIP: not an Impress document");
                newly_done.push(filename);
                continue;
            }
        };

        if pres.slides.is_empty() {
            println!("  SKIP: no slides");
            newly_done.push(filename);
            continue;
        }

        let (sw, sh) = pres.slide_size;
        println!("  slide_size: {:.1}x{:.1} pt, {} slides", sw, sh, pres.slides.len());

        let scale = 2.0f32;
        let pw = (sw * scale) as u32;
        let ph = (sh * scale) as u32;

        if pw == 0 || ph == 0 {
            println!("  SKIP: invalid slide size ({}x{})", pw, ph);
            newly_done.push(filename);
            continue;
        }

        let mut renderer = SlideRenderer::new(sw, sh);

        for (i, slide) in pres.slides.iter().enumerate() {
            let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
            match renderer.render_slide(
                slide, &pres.styles, None, scale, &mut buf,
                &pres.theme_colors, Some(&pres.master_slides)
            ) {
                Ok((_actual_w, _actual_h)) => {
                    let pixmap = match tiny_skia::Pixmap::from_vec(
                        buf, tiny_skia::IntSize::from_wh(pw, ph).unwrap()
                    ) {
                        Some(p) => p,
                        None => {
                            println!("  slide {}: SKIP pixmap construction failed", i);
                            continue;
                        }
                    };
                    let png = pixmap.encode_png().unwrap();
                    let out_path = format!("{}/{}__slide{}.png", file_folder, filename, i);
                    std::fs::write(&out_path, &png).unwrap();
                    println!("  slide {} -> {} ({}x{})", i, out_path, pw, ph);
                }
                Err(e) => {
                    println!("  slide {}: SKIP render_slide failed: {:?}", i, e);
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

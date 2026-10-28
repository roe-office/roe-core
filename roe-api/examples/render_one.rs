use std::path::Path;
use roe_writer::layout::ParagraphLayoutEngine;
use roe_writer::tile::WriterTileRenderer;
use roe_writer::FontRegistry;
use roe_writer::Margins;
use roe_parser::RoeDocument;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    const USAGE: &str = "usage: render_one <input.docx> [output_dir]";
    let input_path = args.get(1).expect(USAGE);
    // output_dir 不传时默认落在系统临时目录下的固定子目录，关机自动清空。
    let default_output_dir = std::env::temp_dir().join("roe_docx_render_one").to_string_lossy().to_string();
    let output_dir: &str = args.get(2).map(|s| s.as_str()).unwrap_or(&default_output_dir);
    println!("output_dir: {}", output_dir);

    // 目录创建提前到拷贝源文件之前：拷贝依赖目标目录已存在，
    // 之前顺序反了导致拷贝静默失败（目录还没建，copy的目标路径不存在）。
    std::fs::create_dir_all(output_dir).expect("create output dir");

    let filename = Path::new(input_path).file_stem().unwrap().to_string_lossy().to_string();
    println!("=== {} ===", filename);

    let src_path = Path::new(input_path);
    if let Some(ext) = src_path.extension() {
        let dest = format!("{}/{}.{}", output_dir, filename, ext.to_string_lossy());
        if let Err(e) = std::fs::copy(src_path, &dest) {
            println!("  WARN: failed to copy source file into output dir: {}", e);
        }
    }

    let data = std::fs::read(input_path).expect("read file");
    let doc = RoeDocument::open_docx_bytes(&data).expect("parse docx");
    let wdoc = match doc {
        RoeDocument::Writer(d) => d,
        _ => panic!("not a Writer document"),
    };

    println!("  body elements: {}", wdoc.body.len());
    println!("  title_page: {}", wdoc.title_page);
    println!("  even_and_odd_headers: {}", wdoc.even_and_odd_headers);
    println!("  header.default paragraphs: {}", wdoc.header.default.as_ref().map(|v| v.len()).unwrap_or(0));
    println!("  footer.default paragraphs: {}", wdoc.footer.default.as_ref().map(|v| v.len()).unwrap_or(0));

    // [DEBUG v79] 验证任务B parser层第一步：sections 是否真的每节独立、
    // 不再是"全部指向同一份"。见 ROE_DOCX_Progress_v79.md 3.3节。
    println!("  sections.len(): {}", wdoc.sections.len());
    for (idx, sec) in wdoc.sections.iter().enumerate() {
        let hf_text = |hf: &Option<roe_model::writer::HeaderFooterSet>| -> String {
            hf.as_ref()
                .and_then(|h| h.default.as_ref())
                .map(|paras| {
                    paras.iter()
                        .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                        .collect::<Vec<_>>()
                        .join(" / ")
                })
                .unwrap_or_else(|| "<none>".to_string())
        };
        println!(
            "    [section {}] name={:?} title_page={:?} header_default={:?} footer_default={:?}",
            idx,
            sec.name,
            sec.title_page,
            hf_text(&sec.header),
            hf_text(&sec.footer),
        );
    }

    let mut registry = FontRegistry::new_with_system_fonts();
    // [FIX v41] render_one 之前给 WriterTileRenderer 传的是全新空
    // FontContext（parley::FontContext::new()），没有加载 KaTeX 字体，
    // 导致公式渲染时 Parley 找不到 "Main-Regular" 等 KaTeX 字体族，
    // fallback 到系统字体（甚至 NotoColorEmoji），数字等字符轮廓缺失。
    // v40 已把 formula 渲染合并进正文用的同一个 font_context/layout_context，
    // 这里必须把 registry 里已加载 KaTeX 字体的 font_context 转移给
    // renderer，而不是重新 new 一个空的。engine 排版测量阶段不需要
    // font_context（那是渲染阶段用的资源），take 出来不影响 layout()
    // 的调用顺序（engine.layout 在下面已经跑完）。
    let font_context = std::mem::replace(&mut registry.font_context, parley::FontContext::new());
    let mut engine = ParagraphLayoutEngine::new(registry);
    // [FIX v77] 读取 wdoc 实际 sectPr 解析出的 page_size/margin_*，
    // 不再使用 ParagraphLayoutEngine::new() 里写死的 A4 默认值。
    engine.page_width  = wdoc.page_size.0;
    engine.page_height = wdoc.page_size.1;
    engine.margins = Margins {
        top:    wdoc.margin_top,
        bottom: wdoc.margin_bottom,
        left:   wdoc.margin_left,
        right:  wdoc.margin_right,
    };
    // 文档）。曾尝试在这里动态传值，但landscape+窄边距文档传真实尺寸后
    // 画布变矮，内容被迫多分1页，与"表格混排绕排"验证目标产生纠缠、
    // 干扰判断。临时回退到默认A4，先专注验证浮动表格宽度/方向/文字环绕
    // 本身是否正确，页面尺寸传值是独立于本专项的下一步，记入交接文档
    // 待办，不在这次改动。
    let result = engine.layout(&wdoc);
    let arena = &result.arena;
    println!("  pages: {}", arena.page_order.len());

    let mut renderer = WriterTileRenderer::new_with_fonts(font_context, ());
    for (i, &pid) in arena.page_order.iter().enumerate().take(10) {
        let page = &arena.pages[pid];
        let scale = 2.0f32;
        let pw = (page.width * scale) as u32;
        let ph = (page.height * scale) as u32;
        if pw == 0 || ph == 0 { println!("  page {}: SKIP invalid size", i); continue; }
        let vp_y = arena.page_global_y(pid);
        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        match renderer.render_viewport(arena, &wdoc, 0.0, vp_y, page.width, page.height, pw, ph, &mut buf) {
            Ok(_) => {
                let pixmap = tiny_skia::Pixmap::from_vec(buf, tiny_skia::IntSize::from_wh(pw, ph).unwrap()).unwrap();
                let png = pixmap.encode_png().unwrap();
                // 防御性补一次目录创建：多页渲染场景下观察到写入中途
                // 目录可能已不存在（例如系统tmp清理机制介入），create_dir_all
                // 是幂等操作，目录已存在时不会报错。
                std::fs::create_dir_all(output_dir).expect("ensure output dir exists before write");
                let out_path = format!("{}/{}__page{}.png", output_dir, filename, i);
                std::fs::write(&out_path, &png).unwrap();
                println!("  page {} -> {}", i, out_path);
            }
            Err(e) => println!("  page {}: SKIP render failed: {:?}", i, e),
        }
    }
}

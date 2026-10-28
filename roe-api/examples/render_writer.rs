use roe_writer::{layout::{arena_to_layout_pages, ParagraphLayoutEngine}, tile::WriterTileRenderer};
use roe_writer::FontRegistry;

fn main() {
    let path = std::env::args().nth(1).unwrap_or("/home/yong/test_image.fodt".to_string());
    let data = std::fs::read(&path).expect("read file");
    let doc = match roe_parser::RoeDocument::open_fodt_bytes(&data).expect("parse") {
        roe_parser::RoeDocument::Writer(d) => d,
        _ => panic!("not a writer doc"),
    };
    println!("body elements: {}", doc.body.len());
    let registry = FontRegistry::new_with_system_fonts();
    let mut engine = ParagraphLayoutEngine::new(registry);
    let result = engine.layout(&doc);
    let pages = arena_to_layout_pages(&result.arena, &doc);
    println!("pages: {}", pages.len());
    let mut renderer = WriterTileRenderer::new();
    for (i, page) in pages.iter().enumerate().take(3) {
        println!("page {}: {} paragraphs, {} images, {} float_frames", i, page.paragraphs.len(), page.images.len(), page.float_frames.len());
        let scale = 1.0f32;
        let pw = (page.width * scale) as u32;
        let ph = (page.height * scale) as u32;
        let mut buf = vec![0u8; (pw * ph * 4) as usize];
        renderer.render_page_preview(page, scale, &mut buf).expect("render");
        let pixmap = tiny_skia::Pixmap::from_vec(
            buf,
            tiny_skia::IntSize::from_wh(pw, ph).unwrap(),
        ).unwrap();
        let png = pixmap.encode_png().unwrap();
        let out = format!("/tmp/writer_page{}.png", i);
        std::fs::write(&out, &png).unwrap();
        println!("  → {} ({} bytes)", out, png.len());
    }
}

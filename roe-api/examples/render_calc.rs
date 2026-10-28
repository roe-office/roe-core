use roe_core::tile_geometry::{col_x_px, row_y_px};

fn main() {
    let path = std::env::args().nth(1).unwrap_or("/home/yong/test_image.fods".to_string());
    let data = std::fs::read(&path).expect("read file");
    let wb = match roe_parser::RoeDocument::open_fods_bytes(&data).expect("parse") {
        roe_parser::RoeDocument::Calc(wb) => wb,
        _ => panic!("not a calc doc"),
    };
    println!("sheets: {}", wb.sheets.len());
    for (i, sheet) in wb.sheets.iter().enumerate() {
        println!("sheet {}: {} floating_images", i, sheet.floating_images.len());
    }
    let sheet = &wb.sheets[0];
    let mut renderer = roe_core::tile::CalcTileRenderer::new(&wb);
    let max_cols = 20u32;
    let max_rows = 50u32;
    let cfg = &renderer.config;
    let vw = col_x_px(sheet, max_cols, cfg) as u32 + 1;
    let vh = row_y_px(sheet, max_rows, cfg) as u32 + 1;
    let mut buf = vec![0u8; (vw as usize) * (vh as usize) * 4];
    let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, 0.0, 0.0, &mut buf);
    if ok {
        let pixmap = tiny_skia::Pixmap::from_vec(
            buf,
            tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        let png = pixmap.encode_png().unwrap();
        std::fs::write("/tmp/calc_preview.png", &png).unwrap();
        println!("→ /tmp/calc_preview.png ({}x{}, {} bytes)", vw, vh, png.len());
    }
    // v115死代码清理：zoom=200 HiDPI tile 测试（render_tile_into/TileId）
    // 已随瓦片滚动路径整体退役一并删除，前台已完全切换到
    // render_viewport_into/roe_render_sheet_viewport。
}

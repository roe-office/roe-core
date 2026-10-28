use std::path::Path;
use roe_api::test_support::open_calc_for_test as open_calc;
use roe_core::tile_geometry::{col_x_px, row_y_px};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let output_dir = args.get(2).map(|s| s.as_str()).unwrap_or("/tmp");
    let wb = open_calc(path).expect("open failed");
    let filename = Path::new(path).file_stem().unwrap().to_string_lossy().to_string();
    println!("sheet count: {}", wb.sheets.len());
    let max_cols = 20u32;
    let max_rows = 50u32;
    for i in 0..wb.sheets.len() {
        let sheet_name = wb.sheets[i].name.clone();
        // ROOT-078验证：显式注入CachedEvaluator，走evaluator分支
        let evaluator = Box::new(roe_calc::CachedEvaluator::new(&wb));
        let mut renderer = roe_core::tile::CalcTileRenderer::new(&wb)
            .with_formula_evaluator(evaluator);
        renderer.sheet_idx = i;
        let sheet = &wb.sheets[i];
        let cfg = &renderer.config;
        let vw = col_x_px(sheet, max_cols, cfg) as u32 + 1;
        let vh = row_y_px(sheet, max_rows, cfg) as u32 + 1;
        let mut buf = vec![0u8; (vw as usize) * (vh as usize) * 4];
        let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, 0.0, 0.0, &mut buf);
        if ok {
            let pixmap = tiny_skia::Pixmap::from_vec(buf, tiny_skia::IntSize::from_wh(vw, vh).unwrap()).unwrap();
            let png = pixmap.encode_png().unwrap();
            let safe_name = sheet_name.replace('/', "_");
            let out_path = format!("{}/{}__sheet{}_{}.png", output_dir, filename, i, safe_name);
            std::fs::write(&out_path, &png).unwrap();
            println!("sheet {} ({}) -> {} ({}x{})", i, sheet_name, out_path, vw, vh);
        } else {
            println!("sheet {} ({}): render_viewport_into returned false", i, sheet_name);
        }
    }
}

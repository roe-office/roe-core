//! ROOT-082 optimalRowHeight 端到端渲染验证：走真实的
//! roe_open_document_from_bytes + roe_render_sheet_viewport C-ABI
//! 入口（两阶段Layout+Paint），而不是直接调
//! CalcTileRenderer::render_viewport_into（那样会跳过新写的
//! layout_measure_viewport，看到的还是未测量的估算值效果）。
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let output_dir = args.get(2).map(|s| s.as_str()).unwrap_or("/tmp");
    let filename = Path::new(path).file_stem().unwrap().to_string_lossy().to_string();

    let data = std::fs::read(path).expect("read failed");
    let h = roe_api::roe_open_document_from_bytes(data.as_ptr(), data.len(), 5 /* xlsx */);
    if h == 0 {
        eprintln!("open failed");
        std::process::exit(1);
    }

    for sheet_idx in 0..2u32 {
        let vw = 1200u32;
        let vh = 900u32;
        let mut buf = vec![0u8; (vw as usize) * (vh as usize) * 4];
        let code = roe_api::roe_render_sheet_viewport(
            h, sheet_idx,
            0.0, 0.0,
            vw, vh,
            100,
            50.0, 20.0,
            buf.as_mut_ptr(), buf.len(),
        );
        if code == 0 {
            let pixmap = tiny_skia::Pixmap::from_vec(buf, tiny_skia::IntSize::from_wh(vw, vh).unwrap()).unwrap();
            let png = pixmap.encode_png().unwrap();
            let out_path = format!("{}/{}__sheet{}_viewport.png", output_dir, filename, sheet_idx);
            std::fs::write(&out_path, &png).unwrap();
            println!("sheet {} -> {}", sheet_idx, out_path);
        } else {
            println!("sheet {}: render failed, code={}", sheet_idx, code);
        }
    }
}

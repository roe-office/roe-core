use roe_api::test_support::open_calc_for_test as open_calc;
use roe_core::tile_geometry::CalcRenderConfig;
use roe_core::row_height_measure::{layout_measure_viewport, measure_row_height_wrap, row_has_wrap_text};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let mut wb = open_calc(path).expect("open failed");
    let sheet = &mut wb.sheets[0]; // Sheet1
    let cfg = CalcRenderConfig::default();

    println!("col_widths: {:?}", sheet.col_widths);
    println!("row 0 has_wrap_text: {}", row_has_wrap_text(sheet, 0));
    println!("row 3 has_wrap_text: {}", row_has_wrap_text(sheet, 3));

    // 直接单独调用wrap路径看结果（不经过贪吃蛇分流）
    let mut fc = parley::FontContext::new();
    let mut lc = parley::LayoutContext::new();
    let h0 = measure_row_height_wrap(sheet, 0, &cfg, &mut fc, &mut lc);
    let h3 = measure_row_height_wrap(sheet, 3, &cfg, &mut fc, &mut lc);
    println!("measure_row_height_wrap(row0) = {:?}", h0);
    println!("measure_row_height_wrap(row3) = {:?}", h3);

    println!("--- 测量前 row_heights ---");
    for (i, rh) in sheet.row_heights.iter().enumerate() {
        println!("  row {}: {:?}", i, rh);
    }

    // 走真实的layout_measure_viewport（模拟渲染时的调用，视口给够大）
    layout_measure_viewport(sheet, &cfg, 0.0, 900.0);

    println!("--- layout_measure_viewport后 row_heights ---");
    for (i, rh) in sheet.row_heights.iter().enumerate() {
        println!("  row {}: {:?} (current={:.2}pt)", i, rh, rh.current());
    }
}

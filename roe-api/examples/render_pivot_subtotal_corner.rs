// v152：用真实 xlsx（pivot_2row2col.xlsx 的 pivot_test 明细 sheet）验证
// 行列小计交汇点。字段约定（用户明确）：
//   Region/Quarter -> 筛选字段（本次不使用，留空）
//   Category/SubCategory -> 行字段（2层，最外层 Category 开小计）
//   Color/Size -> 列字段（2层，最外层 Color 开小计）
//   Sales -> 值字段
// 验证标准（用户手工在 OnlyOffice 里核对过的关系）：
//   行小计（某 Category 下所有 SubCategory 求和）与列小计（某 Color 下所有 Size 求和）
//   在交汇格处必须相等：横着加=竖着加。
use roe_api::test_support::open_calc_for_test as open_calc;
use roe_core::tile::{CalcTileRenderer, CalcRenderConfig};
use roe_core::tile_geometry::{col_x_px, row_y_px};
use roe_polars_bridge::{PivotConfig, pivot_sheet_into_workbook_v2};

fn main() {
    let path = "/home/yong/pivot_tests/pivot_2row2col.xlsx";
    let mut wb = open_calc(path).expect("open failed");
    println!("sheet count: {}", wb.sheets.len());
    for (i, s) in wb.sheets.iter().enumerate() {
        println!("  sheet[{}] = {}", i, s.name);
    }

    let source_idx = wb.sheets.iter().position(|s| s.name == "pivot_test")
        .expect("找不到名为 pivot_test 的明细 sheet");
    println!("明细 sheet 索引: {}", source_idx);

    let cfg = PivotConfig {
        rows: vec!["Category".into(), "SubCategory".into()],
        cols: vec!["Color".into(), "Size".into()],
        values: vec!["Sales".into()],
        agg: "sum".into(),
        filters: Default::default(),
    };

    let pivot_idx = pivot_sheet_into_workbook_v2(&mut wb, source_idx, &cfg, "PivotCornerTest".into())
        .expect("pivot failed");
    println!("透视 sheet idx={}", pivot_idx);

    // 打印行/列树结构，方便肉眼核对小计节点是否插入正确
    let meta = wb.sheets[pivot_idx].pivot.as_ref().expect("pivot meta");
    println!("\n--- row_tree ---");
    for outer in &meta.row_tree.children {
        println!("  {} (subtotal={}, children={})", outer.value, outer.is_subtotal, outer.children.len());
        for inner in &outer.children {
            println!("    {} (subtotal={})", inner.value, inner.is_subtotal);
        }
    }
    println!("\n--- col_tree ---");
    for outer in &meta.col_tree.children {
        println!("  {} (subtotal={}, children={})", outer.value, outer.is_subtotal, outer.children.len());
        for inner in &outer.children {
            println!("    {} (subtotal={})", inner.value, inner.is_subtotal);
        }
    }

    // 核心验证：找到 Category 小计 x Color 小计 的交汇点，跟横向/纵向求和比对
    println!("\n--- 交汇点验证（横竖相加应相等） ---");
    for cat_outer in &meta.row_tree.children {
        let cat = &cat_outer.value; // 注意：小计节点 value 是 "{Category} Total"，这里先只处理明细节点求交汇点所在的真实Category值
        if cat_outer.is_subtotal { continue; }
        for color_outer in &meta.col_tree.children {
            if color_outer.is_subtotal { continue; }
            let color = &color_outer.value;

            // 横向：该Category行小计，在该Color的所有Size列上求和
            let row_subtotal_path = vec![cat.clone(), String::new()];
            let mut horizontal_sum = 0.0;
            for size_inner in &color_outer.children {
                let col_path = vec![color.clone(), size_inner.value.clone()];
                if let Some(v) = meta.data.get(&row_subtotal_path, &col_path, 0) {
                    horizontal_sum += v;
                }
            }

            // 纵向：该Color列小计，在该Category的所有SubCategory行上求和
            let col_subtotal_path = vec![color.clone(), String::new()];
            let mut vertical_sum = 0.0;
            for sub_inner in &cat_outer.children {
                let row_path = vec![cat.clone(), sub_inner.value.clone()];
                if let Some(v) = meta.data.get(&row_path, &col_subtotal_path, 0) {
                    vertical_sum += v;
                }
            }

            // 交汇点本身（row小计 x col小计）
            let corner = meta.data.get(&row_subtotal_path, &col_subtotal_path, 0);

            println!(
                "{} x {}: 横向求和={:.2}, 纵向求和={:.2}, 交汇点直接查值={:?}, 一致={}",
                cat, color, horizontal_sum, vertical_sum, corner,
                (horizontal_sum - vertical_sum).abs() < 0.01
                    && corner.map(|c| (c - horizontal_sum).abs() < 0.01).unwrap_or(false)
            );
        }
    }

    // 渲染出图，肉眼核对完整表格
    let mut renderer = CalcTileRenderer::new(&wb);
    renderer.sheet_idx = pivot_idx;
    renderer.config = CalcRenderConfig {
        default_col_width: 110.0,
        default_row_height: 28.0,
        ..CalcRenderConfig::default()
    };
    // v152：3层Color(红/绿/蓝) x 2层Size(大/小) + 每组小计列 + 总计列，
    // 共 3*(2+1) + 1 = 10 列数据区，加上左侧2列行头 = 12 列，留余量给 16。
    // 行：3个Category x (2明细+1小计) + 总计行 = 10 行，加上表头4行，留余量给 16。
    let max_cols = 16u32;
    let max_rows = 20u32;
    let sheet = &wb.sheets[pivot_idx];
    let cfg = &renderer.config;
    let vw = col_x_px(sheet, max_cols, cfg) as u32 + 1;
    let vh = row_y_px(sheet, max_rows, cfg) as u32 + 1;
    let mut buf = vec![0u8; (vw as usize) * (vh as usize) * 4];
    let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, 0.0, 0.0, &mut buf);
    assert!(ok, "render_viewport_into failed");
    let pixmap = tiny_skia::Pixmap::from_vec(buf, tiny_skia::IntSize::from_wh(vw, vh).unwrap()).unwrap();
    pixmap.save_png("/tmp/roe_pivot_subtotal_corner.png").unwrap();
    println!("\n→ /tmp/roe_pivot_subtotal_corner.png ({}x{})", vw, vh);
}

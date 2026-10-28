// 验证：直接渲染 pivot_2row2col.xlsx 打开后的"工作表1"（sheet[0]，A路径xlsx静态快照），
// 不做任何v2重算，看小计是否被渲染出来。用于排查"是A路径解析器bug"还是
// "前台/脚本渲染方法本身有差异"。
use roe_core::tile::CalcTileRenderer;

fn main() {

    let path = "/home/yong/pivot_tests/pivot_2row2col.xlsx";
    let bytes = std::fs::read(path).expect("read");
    let doc = roe_parser::RoeDocument::open_xlsx_bytes(&bytes).expect("parse");
    let wb = match doc { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
    println!("sheets: {:?}", wb.sheets.iter().map(|s| &s.name).collect::<Vec<_>>());

    let sheet0 = &wb.sheets[0];
    println!("sheet[0]={} pivot.is_some()={}", sheet0.name, sheet0.pivot.is_some());
    if let Some(p) = &sheet0.pivot {
        println!("row_tree.children.len()={}", p.row_tree.children.len());
        for outer in &p.row_tree.children {
            println!("  {} (subtotal={}, children={})", outer.value, outer.is_subtotal, outer.children.len());
            for inner in &outer.children {
                println!("    {} (subtotal={})", inner.value, inner.is_subtotal);
            }
        }
    }

    // v156：验证B路径接线后，pivot区域的静态单元格快照是否已被清空
    // （v155诊断的根因修复：Excel/WPS保存透视表时会把展开状态的完整
    // 视觉内容写入<sheetData>普通单元格，与PivotMeta动态渲染同屏叠加
    // 会造成文字重叠等视觉异常）。
    if let Some(p) = &sheet0.pivot {
        let (base_row, base_col, last_row, last_col) =
            (p.base_row, p.base_col, p.last_row, p.last_col);
        println!(
            "pivot region base_row={} base_col={} last_row={} last_col={}",
            base_row, base_col, last_row, last_col
        );
        let mut non_empty_count = 0u32;
        let mut first_few: Vec<(u32, u32, String)> = Vec::new();
        for r in base_row..=last_row {
            for c in base_col..=last_col {
                let v = sheet0.cells.get(r, c);
                if !matches!(v, roe_model::calc::CellValue::Empty) {
                    non_empty_count += 1;
                    if first_few.len() < 10 {
                        first_few.push((r, c, format!("{:?}", v)));
                    }
                }
            }
        }
        println!("non_empty_cells_in_pivot_region={}", non_empty_count);
        for (r, c, v) in &first_few {
            println!("  non-empty at ({}, {}): {}", r, c, v);
        }
        if non_empty_count == 0 {
            println!("[CHECK-PASS] pivot region cells fully cleared");
        } else {
            println!("[CHECK-FAIL] pivot region still has {} non-empty cells", non_empty_count);
        }
    }

    let mut renderer = CalcTileRenderer::new(&wb);
    renderer.sheet_idx = 0;
    let vw = 1200u32;
    let vh = 800u32;
    let mut buf = vec![0u8; (vw * vh * 4) as usize];
    let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, 80.0, 20.0, &mut buf);
    assert!(ok, "render failed");
    let pixmap = tiny_skia::Pixmap::from_vec(buf, tiny_skia::IntSize::from_wh(vw, vh).unwrap()).unwrap();
    pixmap.save_png("/tmp/probe_sheet0.png").unwrap();
    println!("saved /tmp/probe_sheet0.png");
}

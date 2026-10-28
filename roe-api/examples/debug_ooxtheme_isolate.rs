// v94诊断专用工具：逐个隔离渲染 sd_data_pptx__ooxtheme.pptx 的
// slideLayout1.xml 里那个 <p:grpSp>（装饰形状分组）内的每一个子形状，
// 每个形状单独输出一张PNG，方便肉眼对照OnlyOffice母版拆解截图，
// 定位具体是哪个形状（geometry/颜色/位置）渲染错了。
//
// 用法: cargo run -p roe-api --example debug_ooxtheme_isolate -- <pptx路径> <输出目录>
use roe_impress::slide_renderer::SlideRenderer;
use roe_model::impress::{Shape, ShapeContent, Slide};
use roe_parser::RoeDocument;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pptx_path = args.get(1).expect("usage: debug_ooxtheme_isolate <pptx> <output_dir>");
    let output_dir = args.get(2).expect("usage: debug_ooxtheme_isolate <pptx> <output_dir>");

    std::fs::create_dir_all(output_dir).expect("create output dir");

    let doc = RoeDocument::open_pptx_file(pptx_path).expect("parse failed");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not a pptx"),
    };

    let (sw, sh) = pres.slide_size;
    let scale = 2.0f32;
    let pw = (sw * scale) as u32;
    let ph = (sh * scale) as u32;

    let slide = &pres.slides[0];

    // 找到 layout_shapes 里那个 Group（本样本已知只有1个非占位符
    // 顶层形状，且它就是Group；写成通用查找，不假设固定index）。
    let group_shape = slide.layout_shapes.iter()
        .find(|s| matches!(&s.content, ShapeContent::Group { .. }))
        .expect("no Group shape found in layout_shapes");

    let (children, ch_off, ch_ext) = match &group_shape.content {
        ShapeContent::Group { children, ch_off, ch_ext } => (children, *ch_off, *ch_ext),
        _ => unreachable!(),
    };

    println!("Group id={} position={:?} ch_off={:?} ch_ext={:?}, {} children",
        group_shape.id, group_shape.position, ch_off, ch_ext, children.len());

    // Group自身在父坐标系(slide)里的位置/尺寸，用于算 child_scale
    // （与slide_renderer.rs render_shape里Group分支的算法保持一致）。
    let gr = &group_shape.position;
    let (ch_off_x, ch_off_y) = ch_off.unwrap_or((0.0, 0.0));
    let (ch_ext_w, ch_ext_h) = ch_ext.unwrap_or((gr.w(), gr.h()));
    let scale_x = if ch_ext_w.abs() > f32::EPSILON { gr.w() / ch_ext_w } else { 1.0 };
    let scale_y = if ch_ext_h.abs() > f32::EPSILON { gr.h() / ch_ext_h } else { 1.0 };

    let mut renderer = SlideRenderer::new(sw, sh);

    for (idx, child) in children.iter().enumerate() {
        // 手动复刻 render_shape 顶部的 child_scale 映射，把子形状的
        // 局部坐标转换成"独立渲染时"应该使用的最终父坐标系坐标
        // （因为这里我们不通过Group包装渲染，而是把子形状提升为
        // 顶层独立shape，所以要提前把变换烘焙进它的position里）。
        let raw_x = child.position.x();
        let raw_y = child.position.y();
        let raw_w = child.position.w();
        let raw_h = child.position.h();
        let mapped_x = (raw_x - ch_off_x) * scale_x + gr.x();
        let mapped_y = (raw_y - ch_off_y) * scale_y + gr.y();
        let mapped_w = raw_w * scale_x;
        let mapped_h = raw_h * scale_y;

        let mut isolated = child.clone();
        isolated.position = roe_model::common::Transform::from_rect(mapped_x, mapped_y, mapped_w, mapped_h);
        // 保留原始rotation/flip（from_rect不带这些，手动拷回）
        isolated.position.rotation = child.position.rotation;
        isolated.position.flip_h = child.position.flip_h;
        isolated.position.flip_v = child.position.flip_v;

        println!("  child #{} id={} name_geom={:?} mapped_pos=({:.1},{:.1},{:.1},{:.1}) fill={:?} stroke={:?}",
            idx, child.id, isolated.geometry, mapped_x, mapped_y, mapped_w, mapped_h, isolated.style.fill, isolated.style.stroke);

        let isolated_slide = Slide { layout_idx: None,
            shapes: vec![isolated],
            master_idx: None,  // 不画母版/layout背景，只看这一个形状
            animation_tree: Default::default(),
            transition: None,
            notes: None,
            background: None,
            layout_shapes: Vec::new(),
        };

        let mut buf = vec![0u8; (pw as usize) * (ph as usize) * 4];
        match renderer.render_slide(
            &isolated_slide, &pres.styles, None, scale, &mut buf,
            &pres.theme_colors, Some(&pres.master_slides)
        ) {
            Ok(_) => {
                let pixmap = tiny_skia::Pixmap::from_vec(
                    buf, tiny_skia::IntSize::from_wh(pw, ph).unwrap()
                ).unwrap();
                let png = pixmap.encode_png().unwrap();
                let out_path = format!("{}/child_{:02}_id{}.png", output_dir, idx, child.id);
                std::fs::write(&out_path, &png).unwrap();
                println!("    -> {}", out_path);
            }
            Err(e) => println!("    RENDER FAILED: {:?}", e),
        }
    }
}

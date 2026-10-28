// 一次性实验探针：验证方案B（标准DPI渲染+Pattern等比放大）在小尺寸图表场景下
// 的清晰度是否可接受。用真实样本 chart-theme-override.pptx 里 chart shape 的
// 实际 pt 尺寸（360x216，从 slide1.xml 的 <a:ext cx="4572000" cy="2743200"/>
// 换算而来，EMU/12700=pt），而非之前误用的整页尺寸(960x540)。
// 不修改任何生产代码，字体加载逻辑从 slide_renderer.rs::svg_to_png_bytes 复制而来。
use charts_rs::{BarChart, Series};

fn svg_to_png_bytes(svg_str: &str, width: u32, height: u32) -> Option<Vec<u8>> {
    let mut opt = resvg::usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    opt.fontdb_mut().set_serif_family("Noto Sans CJK SC");
    let tree = resvg::usvg::Tree::from_str(svg_str, &opt).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    pixmap.encode_png().ok()
}

fn render_bar_chart(width: u32, height: u32, out_path: &str) {
    let series = vec![
        Series::new("".to_string(), vec![1.0, 2.0, 3.0]),
    ];
    let mut c = BarChart::new(series, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    c.width = width as f32;
    c.height = height as f32;
    c.legend_show = Some(false);
    c.grid_stroke_width = 1.0;
    let svg = c.svg().expect("svg gen failed");
    let png = svg_to_png_bytes(&svg, width, height).expect("render failed");
    std::fs::write(out_path, &png).unwrap();
    println!("{} -> {} ({}x{})", out_path, out_path, width, height);
}

fn main() {
    // 真实 chart shape 尺寸：360x216 pt
    let w_pt = 360.0_f32;
    let h_pt = 216.0_f32;
    let scale = 2.0_f32;

    // 组1：当前默认行为，直接按 scale=2.0 渲染 720x432
    render_bar_chart((w_pt * scale) as u32, (h_pt * scale) as u32, "/tmp/roe_small_scale2_direct.png");

    // 组2：方案B，标准DPI渲染 360x216，之后用外部工具放大2倍到720x432对比
    render_bar_chart(w_pt as u32, h_pt as u32, "/tmp/roe_small_scale1_standard.png");
}

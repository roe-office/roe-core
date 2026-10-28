// PROOT-033 探针：绕开roe的pptx解析和渲染管线，直接用真实数据
// (x_labels a/b/c, series 1.0/2.0/3.0) 单独测试 charts-rs 的 BarChart，
// 排查是roe这边的调用/传参问题，还是charts-rs库本身对这组输入的bug。
use charts_rs::{BarChart, Series};

fn main() {
    let series = vec![Series::new("".to_string(), vec![1.0, 2.0, 3.0])];
    let x_labels = vec!["a".to_string(), "b".to_string(), "c".to_string()];

    let mut c = BarChart::new(series, x_labels);
    c.title_text = "图表标题".to_string();
    c.width = 600.0;
    c.height = 540.0;
    c.legend_show = Some(false);
    c.legend_category = charts_rs::LegendCategory::RoundRect;
    c.grid_stroke_width = 1.0;

    match c.svg() {
        Ok(svg) => {
            std::fs::write("/tmp/roe_chart_svg_debug.svg", &svg).unwrap();
            println!("SVG written, {} bytes", svg.len());
            println!("--- first 500 chars ---");
            println!("{}", &svg.chars().take(500).collect::<String>());
        }
        Err(e) => {
            println!("svg() FAILED: {:?}", e);
        }
    }
}

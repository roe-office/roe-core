use roe_parser::odf::FodpParser;

fn main() {
    let xml = std::fs::read_to_string("/tmp/test_chart.fodp").expect("读取 FODP 失败");
    let pres = FodpParser::parse(&xml).expect("解析失败");
    println!("幻灯片数: {}", pres.slides.len());
    for (i, slide) in pres.slides.iter().enumerate() {
        println!("Slide {}: {} 个 shape", i, slide.shapes.len());
        for (j, shape) in slide.shapes.iter().enumerate() {
            println!("  Shape {}: {:?}", j, shape.content);
        }
    }
}

use roe_parser::ooxml::pptx::PptxParser;

fn main() {
    let data = std::fs::read("/tmp/test_chart.pptx").expect("读取 pptx 失败");
    let pres = PptxParser::parse(&data).expect("解析失败");
    println!("幻灯片数: {}", pres.slides.len());
    for (i, slide) in pres.slides.iter().enumerate() {
        println!("Slide {}: {} 个 shape", i, slide.shapes.len());
        for (j, shape) in slide.shapes.iter().enumerate() {
            println!("  Shape {}: {:?}", j, shape.content);
        }
    }
}

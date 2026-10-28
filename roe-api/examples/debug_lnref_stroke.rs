// PROOT-新bug 探针：确认 lnRef schemeClr fallback 是否真的解析出主题色，
// 还是恒为黑色。样本 fdo47434.pptx 的 cxnSp 全部走
// <p:style><a:lnRef idx="1"><a:schemeClr val="accent1"/></a:lnRef>，
// 理论上应该 fallback 出 accent1 主题色（非黑）。
use roe_parser::RoeDocument;

fn main() {
    let path = "/home/yong/Projects/roe/test_samples/curated/pptx/small__oox_unit_data__chart-theme-override.pptx";
    let doc = RoeDocument::open_pptx_file(path).expect("parse pptx");
    let pres = match doc {
        RoeDocument::Impress(p) => p,
        _ => panic!("not an Impress document"),
    };

    println!("theme_colors accent1 = {:?}", pres.theme_colors.get("accent1"));

    for (i, slide) in pres.slides.iter().enumerate() {
        println!("=== slide {} ===", i);
        for s in &slide.shapes {
            println!("  shape id={}", s.id);
            println!("    fill:   {:?}", s.style.fill);
            println!("    stroke: {:?}", s.style.stroke);
            println!("    content: {:?}", s.content);
        }
    }
}

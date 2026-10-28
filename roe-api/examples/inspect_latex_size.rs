fn main() {
    let font_size_pt: f64 = 10.5;
    let cases: Vec<(&str, &str)> = vec![
        ("纯文字abcd（基准）", "abcd"),
        ("只有overbrace不带上标", r"\overbrace{abcd}"),
        ("只有普通上标不带overbrace", "{x}^{4}"),
        ("组合（真实场景，应精确=16.237703）", r"{\overbrace{abcd}}^{4}"),
        ("只有underbrace不带下标", r"\underbrace{xyz}"),
        ("只有普通下标不带underbrace", "{x}_{3}"),
        ("组合（真实场景，应精确depth=9.36747）", r"{\underbrace{xyz}}_{3}"),
    ];
    for (label, latex) in cases {
        match roe_formula::latex_layout_size(latex, font_size_pt) {
            Ok((w, h, d)) => println!("{:<40} latex={:<32?} width_pt={:.6} height_pt={:.6} depth_pt={:.6}", label, latex, w, h, d),
            Err(e) => println!("{:<40} latex={:<32?} ERROR: {:?}", label, latex, e),
        }
    }
}

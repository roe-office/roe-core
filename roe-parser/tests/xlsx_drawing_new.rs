use base64::Engine as _;
use roe_model::calc::{FloatingImage, FloatingShape};
use roe_model::common::{Color, FillStyle, ShapeGeometry};
use roe_model::writer::{Paragraph, Run};
use roe_parser::ooxml::xlsx::XlsxParser;
use roe_parser::ooxml::xlsx_writer::XlsxWriter;

fn near(a: f32, b: f32) -> bool { (a - b).abs() < 0.05 }

#[test]
fn new_image_and_shape_roundtrip() {
    let path = format!("{}/../test_samples/xlsx/sc_data_xlsx__image_hyperlink.xlsx", env!("CARGO_MANIFEST_DIR"));
    let mut wb = XlsxParser::parse(&std::fs::read(path).unwrap()).unwrap();
    let s = &mut wb.sheets[0];
    s.floating_images.clear();
    s.floating_shapes.clear();
    s.floating_charts.clear();
    s.floating_stat_charts.clear();

    let mut png = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&[0u8; 32]);
    s.floating_images.push(FloatingImage {
        id: 0,
        data_base64: base64::engine::general_purpose::STANDARD.encode(&png),
        mime: "image/png".to_string(),
        width_pt: 120.0, height_pt: 80.0, x_pt: 30.0, y_pt: 40.0,
        rot: 0.0, hidden: false,
    });
    s.floating_shapes.push(FloatingShape { stroke: None,
        id: 0,
        x_pt: 200.0, y_pt: 60.0, width_pt: 90.0, height_pt: 50.0,
        rot: 0.0, flip_v: false,
        geometry: ShapeGeometry::Ellipse,
        fill: Some(FillStyle::Solid(Color { r: 255, g: 0, b: 16, a: 255 })),
        text_content: vec![Paragraph {
            runs: vec![Run { text: "A&B".to_string(), font_size: Some(12.0), bold: Some(true), ..Default::default() }],
            ..Default::default()
        }],
    });

    let bytes = XlsxWriter::write(&wb).expect("写出失败");

    // 包内必须真有媒体文件
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
    assert!(zip.by_name("xl/media/image1.png").is_ok(), "缺 xl/media/image1.png");

    let back = XlsxParser::parse(&bytes).expect("回读失败");
    let b = &back.sheets[0];
    assert_eq!(b.floating_images.len(), 1, "图片数量");
    assert_eq!(b.floating_shapes.len(), 1, "图形数量");

    let im = &b.floating_images[0];
    assert_eq!(im.mime, "image/png");
    assert!(near(im.x_pt, 30.0) && near(im.y_pt, 40.0), "图片位置 {:?}", (im.x_pt, im.y_pt));
    assert!(near(im.width_pt, 120.0) && near(im.height_pt, 80.0), "图片尺寸");

    let sh = &b.floating_shapes[0];
    assert!(near(sh.x_pt, 200.0) && near(sh.y_pt, 60.0), "图形位置");
    assert!(near(sh.width_pt, 90.0) && near(sh.height_pt, 50.0), "图形尺寸");
    match &sh.geometry {
        ShapeGeometry::PresetPath(d) => assert_eq!(d.preset_name, "ellipse"),
        other => panic!("几何不是预设椭圆: {:?}", other),
    }
    match &sh.fill {
        Some(FillStyle::Solid(c)) => assert_eq!((c.r, c.g, c.b), (255, 0, 16)),
        other => panic!("填充不对: {:?}", other),
    }
    let text: String = sh.text_content.iter().flat_map(|p| p.runs.iter()).map(|r| r.text.clone()).collect();
    assert_eq!(text, "A&B");
}

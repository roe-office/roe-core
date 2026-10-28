use roe_model::common::{
    Color, FillStyle, ShapeGeometry, ShapeStyle, StrokeStyle, StyleRef,
};
use roe_model::writer::{BodyElement, Paragraph, Run, Shape, WriterDocument};
use roe_parser::ooxml::docx_writer::DocxWriter;
use std::fs;

fn main() {
    // 场景一：inline 矩形 + 纯色填充（红色）
    let shape_inline_rect = Shape {
        geometry: ShapeGeometry::Rect,
        style: ShapeStyle {
            fill: Some(FillStyle::Solid(Color { r: 0xFF, g: 0x00, b: 0x00, a: 255 })),
            stroke: None,
            shadow: None,
            head_arrow: None,
            tail_arrow: None,
        },
        text_content: vec![],
        width_pt: 100.0,
        height_pt: 50.0,
        x_pt: 0.0,
        y_pt: 0.0,
        anchor_type_h: None,
        anchor_type_v: None,
        align_h: None,
        align_v: None,
        wrap_type: None,
        dist_t_pt: 0.0,
        dist_b_pt: 0.0,
        dist_l_pt: 0.0,
        dist_r_pt: 0.0,
        flip_v: None,
    };

    // 场景二：anchor 椭圆 + 蓝色描边 + square 环绕，位置偏移 (15pt, 25pt)
    let shape_anchor_ellipse = Shape {
        geometry: ShapeGeometry::Ellipse,
        style: ShapeStyle {
            fill: None,
            stroke: Some(StrokeStyle {
                color: Color { r: 0x00, g: 0x00, b: 0xFF, a: 255 },
                width: 2.0,
            }),
            shadow: None,
            head_arrow: None,
            tail_arrow: None,
        },
        text_content: vec![],
        width_pt: 80.0,
        height_pt: 80.0,
        x_pt: 15.0,
        y_pt: 25.0,
        anchor_type_h: Some("column".to_string()),
        anchor_type_v: Some("paragraph".to_string()),
        align_h: None,
        align_v: None,
        wrap_type: Some("square".to_string()),
        dist_t_pt: 0.0,
        dist_b_pt: 0.0,
        dist_l_pt: 0.0,
        dist_r_pt: 0.0,
        flip_v: None,
    };

    // 场景三：inline 矩形 + 文本框文字 "Hello Shape"
    let para = Paragraph {
        style: StyleRef("Normal".to_string()),
        runs: vec![Run {
            text: "Hello Shape".to_string(),
            ..Default::default()
        }],
        list_info: None,
        ..Default::default()
    };
    let shape_with_text = Shape {
        geometry: ShapeGeometry::Rect,
        style: ShapeStyle {
            fill: None,
            stroke: None,
            shadow: None,
            head_arrow: None,
            tail_arrow: None,
        },
        text_content: vec![para],
        width_pt: 120.0,
        height_pt: 60.0,
        x_pt: 0.0,
        y_pt: 0.0,
        anchor_type_h: None,
        anchor_type_v: None,
        align_h: None,
        align_v: None,
        wrap_type: None,
        dist_t_pt: 0.0,
        dist_b_pt: 0.0,
        dist_l_pt: 0.0,
        dist_r_pt: 0.0,
        flip_v: None,
    };

    fn label(text: &str) -> BodyElement {
        BodyElement::Paragraph(Paragraph {
            style: StyleRef("Normal".to_string()),
            runs: vec![Run {
                text: text.to_string(),
                ..Default::default()
            }],
            list_info: None,
            ..Default::default()
        })
    }

    let doc = WriterDocument {
        body: vec![
            label("场景一：inline 矩形 + 红色纯色填充（应显示为一个红色矩形，紧跟在这行文字后面）"),
            BodyElement::Shape(shape_inline_rect),
            label("场景二：anchor 椭圆 + 蓝色描边，环绕方式 square，偏移(15pt, 25pt)（应显示为一个蓝色描边的椭圆，环绕在文字旁边）"),
            BodyElement::Shape(shape_anchor_ellipse),
            label("这里是一段用来测试环绕效果的占位文字，用来观察anchor形状是否正确地与文字环绕排列，如果形状浮动在文字上方或位置明显不对，说明环绕/定位有问题。这段文字要足够长，才能看出环绕效果，所以我在这里多写几句占位内容凑够长度。"),
            label("场景三：inline 矩形（无填充无描边） + 文本框内文字 \"Hello Shape\"（应显示为一个矩形框，内部可见文字 Hello Shape）"),
            BodyElement::Shape(shape_with_text),
            label("以上三个场景到此结束。"),
        ],
        ..Default::default()
    };

    let bytes = DocxWriter::write(&doc).expect("write failed");
    fs::write("/tmp/shape_test.docx", &bytes).expect("failed to write file");
    println!("已生成 /tmp/shape_test.docx，共 {} 字节", bytes.len());
}

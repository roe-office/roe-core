//! v320d1: built-in default master and layouts (16:9, 960x540pt).
use roe_model::common::{ShapeGeometry, ShapeStyle, Transform};
use roe_model::impress::{
    MasterSlide, PlaceholderHintStyle, PlaceholderInfo, Shape, ShapeContent, SlideLayout,
    SlideLayoutKind, TextAnchor, TextBodyProps,
};
use roe_model::style::Alignment;

const W: f32 = 960.0;

fn hint_for(ty: &str) -> PlaceholderHintStyle {
    let (size, anchor) = match ty {
        "ctrTitle" => (60.0, TextAnchor::Bottom),
        "title" => (60.0, TextAnchor::Middle),
        "subTitle" => (24.0, TextAnchor::Top),
        _ => (24.0, TextAnchor::Top),
    };
    PlaceholderHintStyle {
        font_size: Some(size),
        algn: Some(Alignment::Center),
        anchor: Some(anchor),
        ..Default::default()
    }
}

fn ph(id: u32, ty: &str, idx: u32, x: f32, y: f32, w: f32, h: f32) -> Shape {
    Shape {
        id,
        geometry: ShapeGeometry::Rect,
        position: Transform::from_rect(x, y, w, h),
        style: ShapeStyle::default(),
        content: ShapeContent::Text(vec![roe_model::writer::Paragraph::default()]),
        text_body: TextBodyProps { anchor: hint_for(ty).anchor.unwrap_or(TextAnchor::Top), ..TextBodyProps::default() },
        placeholder: Some(PlaceholderInfo {
            ph_type: Some(ty.to_string()),
            index: Some(idx),
            hint_style: Some(hint_for(ty)),
        }),
        st_cxn: None,
        end_cxn: None,
    }
}

fn layout(name: &str, kind: SlideLayoutKind, shapes: Vec<Shape>) -> SlideLayout {
    SlideLayout {
        name: name.to_string(),
        kind,
        shapes,
        background: None,
        show_master_shapes: true,
        color_map: None,
        raw_ext: None,
    }
}

pub fn default_master() -> MasterSlide {
    let title = |id| ph(id, "title", 0, 66.0, 29.0, W - 132.0, 105.0);
    let body = |id| ph(id, "body", 1, 66.0, 144.0, W - 132.0, 342.0);
    let layouts = vec![
        layout("Title Slide", SlideLayoutKind::Title, vec![
            ph(2, "ctrTitle", 0, 120.0, 168.0, W - 240.0, 117.0),
            ph(3, "subTitle", 1, 120.0, 297.0, W - 240.0, 135.0),
        ]),
        layout("Title and Content", SlideLayoutKind::TitleAndContent, vec![title(2), body(3)]),
        layout("Section Header", SlideLayoutKind::SectionHeader, vec![
            ph(2, "title", 0, 66.0, 200.0, W - 132.0, 100.0),
            ph(3, "body", 1, 66.0, 310.0, W - 132.0, 80.0),
        ]),
        layout("Two Content", SlideLayoutKind::TwoContent, vec![
            title(2),
            ph(3, "body", 1, 66.0, 144.0, 396.0, 342.0),
            ph(4, "body", 2, 498.0, 144.0, 396.0, 342.0),
        ]),
        layout("Title Only", SlideLayoutKind::TitleOnly, vec![title(2)]),
        layout("Blank", SlideLayoutKind::Blank, vec![]),
    ];
    MasterSlide {
        theme_xml: None,
        tx_styles_xml: None,
        layouts,
        color_map: None,
        raw_ext: None,
        name: "Office".to_string(),
        shapes: vec![title(2), body(3)],
        background: None,
    }
}

/// Empty placeholder shapes for a new slide based on a layout.
pub fn slide_shapes_from_layout(layout: &SlideLayout) -> Vec<Shape> {
    layout.shapes.iter().filter(|s| s.placeholder.is_some()).cloned().collect()
}

//! 浮动对象清单:把图表/统计图/图片/图形统一成一份只读列表,
//! 供 C 导出 roe_calc_list_objects 与 Rhai calc_list_objects 共用。

use serde::Serialize;

use crate::calc::Sheet;
use crate::common::{FillStyle, ShapeGeometry};

#[derive(Debug, Clone, Serialize)]
pub struct ObjStroke {
    pub color: String,
    pub width: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectInfo {
    pub id: u64,
    /// chart | stat_chart | image | shape
    pub kind: &'static str,
    /// 细分类型:图表种类 / 图形预设名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    pub x_pt: f64,
    pub y_pt: f64,
    pub width_pt: f64,
    pub height_pt: f64,
    pub rot: f64,
    pub hidden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// 图形填充(仅纯色,#rrggbb)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// 图形边框
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke: Option<ObjStroke>,
}

fn r2(v: f32) -> f64 { ((v as f64) * 100.0).round() / 100.0 }

fn geometry_name(g: &ShapeGeometry) -> String {
    match g {
        ShapeGeometry::Rect => "rect".into(),
        ShapeGeometry::RoundRect { .. } => "roundRect".into(),
        ShapeGeometry::Ellipse => "ellipse".into(),
        ShapeGeometry::Triangle => "triangle".into(),
        ShapeGeometry::Line => "line".into(),
        ShapeGeometry::Custom(s) => s.clone(),
        ShapeGeometry::PresetPath(d) => {
            if d.preset_name.is_empty() { "custom".into() } else { d.preset_name.clone() }
        }
    }
}

/// 按叠放顺序(底到顶)列出:图表 -> 统计图 -> 图片 -> 图形。
pub fn list_objects(sheet: &Sheet) -> Vec<ObjectInfo> {
    let mut out = Vec::new();
    for c in &sheet.floating_charts {
        out.push(ObjectInfo {
            id: c.id, kind: "chart", sub: Some(format!("{:?}", c.kind)),
            x_pt: r2(c.x_pt), y_pt: r2(c.y_pt), width_pt: r2(c.width_pt), height_pt: r2(c.height_pt),
            rot: 0.0, hidden: false, title: c.title.clone(), text: None, mime: None, fill: None, stroke: None,
        });
    }
    for c in &sheet.floating_stat_charts {
        out.push(ObjectInfo {
            id: c.id, kind: "stat_chart", sub: Some(format!("{:?}", c.model.kind)),
            x_pt: r2(c.x_pt), y_pt: r2(c.y_pt), width_pt: r2(c.width_pt), height_pt: r2(c.height_pt),
            rot: 0.0, hidden: false, title: c.model.title.clone(), text: None, mime: None, fill: None, stroke: None,
        });
    }
    for i in &sheet.floating_images {
        out.push(ObjectInfo {
            id: i.id, kind: "image", sub: None,
            x_pt: r2(i.x_pt), y_pt: r2(i.y_pt), width_pt: r2(i.width_pt), height_pt: r2(i.height_pt),
            rot: r2(i.rot), hidden: i.hidden, title: None, text: None, mime: Some(i.mime.clone()), fill: None, stroke: None,
        });
    }
    for s in &sheet.floating_shapes {
        let shape_fill: Option<String> = match &s.fill {
            Some(FillStyle::Solid(c)) => Some(format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)),
            _ => None,
        };
        let shape_stroke: Option<ObjStroke> = s.stroke.as_ref().map(|st| ObjStroke {
            color: format!("#{:02x}{:02x}{:02x}", st.color.r, st.color.g, st.color.b),
            width: ((st.width as f64) * 100.0).round() / 100.0,
        });
        let text: String = s.text_content.iter()
            .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect::<Vec<_>>().join("\n");
        out.push(ObjectInfo {
            id: s.id, kind: "shape", sub: Some(geometry_name(&s.geometry)),
            x_pt: r2(s.x_pt), y_pt: r2(s.y_pt), width_pt: r2(s.width_pt), height_pt: r2(s.height_pt),
            rot: r2(s.rot), hidden: false, title: None,
            text: if text.is_empty() { None } else { Some(text) }, mime: None,
            fill: shape_fill, stroke: shape_stroke,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calc::{FloatingImage, FloatingShape};
    use crate::object_ids::ensure_object_ids;
    use crate::writer::{Paragraph, Run};

    #[test]
    fn lists_images_and_shapes_with_ids() {
        let mut sh = Sheet::default();
        sh.floating_images.push(FloatingImage {
            id: 0, data_base64: String::new(), mime: "image/png".into(),
            width_pt: 100.0, height_pt: 50.0, x_pt: 10.0, y_pt: 20.0, rot: 0.0, hidden: false,
        });
        sh.floating_shapes.push(FloatingShape { stroke: Some(crate::common::StrokeStyle { color: crate::common::Color { r: 0, g: 0, b: 255, a: 255 }, width: 2.0 }),
            id: 0, x_pt: 1.0, y_pt: 2.0, width_pt: 30.0, height_pt: 40.0, rot: 90.0, flip_v: false,
            geometry: ShapeGeometry::Ellipse, fill: Some(crate::common::FillStyle::Solid(crate::common::Color { r: 255, g: 128, b: 0, a: 255 })),
            text_content: vec![Paragraph {
                runs: vec![Run { text: "hi".into(), ..Default::default() }],
                ..Default::default()
            }],
        });
        ensure_object_ids(&mut sh);
        let v = list_objects(&sh);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].id, v[0].kind), (1, "image"));
        assert_eq!(v[0].mime.as_deref(), Some("image/png"));
        assert_eq!((v[1].id, v[1].kind), (2, "shape"));
        assert_eq!(v[1].sub.as_deref(), Some("ellipse"));
        assert_eq!(v[1].text.as_deref(), Some("hi"));
        assert_eq!(v[1].rot, 90.0);
        assert_eq!(v[1].fill.as_deref(), Some("#ff8000"));
        let st = v[1].stroke.as_ref().unwrap();
        assert_eq!(st.color, "#0000ff");
        assert_eq!(st.width, 2.0);
        assert!(v[0].fill.is_none() && v[0].stroke.is_none());
    }

    #[test]
    fn empty_sheet_is_empty() {
        assert!(list_objects(&Sheet::default()).is_empty());
    }
}

//! xlsx 浮动图片 / 图形 / 文本框写出（xdr:pic / xdr:sp）。
//!
//! 与图表一致，统一用 absoluteAnchor（坐标为 pt 换算 EMU）。
//! 图片内部同时写 a:off / a:ext，保证 xlsx.rs 解析端能读回。
//! 本文件不依赖 xlsx_writer.rs，由其在写 drawing 时调用。

use base64::Engine as _;
use roe_model::calc::{FloatingImage, FloatingShape};
use roe_model::common::{FillStyle, ShapeGeometry};
use roe_model::writer::Paragraph;
use std::io::{Seek, Write};

const EMU: f32 = 12700.0;

/// 一个 sheet 的浮动对象写出结果
pub struct SheetObjects {
    /// 追加到 xdr:wsDr 里的锚点 XML
    pub frames: String,
    /// 追加到 drawingN.xml.rels 里的 Relationship（图片）
    pub rels: String,
    /// 需要写进 zip 的媒体文件（路径, 字节）
    pub media: Vec<(String, Vec<u8>)>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn emu(pt: f32) -> i64 {
    (pt * EMU).round() as i64
}

fn rot_attr(rot: f32) -> String {
    if rot.abs() < 0.0001 {
        return String::new();
    }
    let raw = ((rot as f64) * 60000.0).round() as i64;
    let full = 360 * 60000;
    format!(r#" rot="{}""#, raw.rem_euclid(full))
}

fn mime_ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        _ => "png",
    }
}

fn ext_content_type(ext: &str) -> &'static str {
    match ext {
        "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        _ => "image/png",
    }
}

/// 是否存在需要写 drawing 的图片/图形
pub fn has_objects(images: &[FloatingImage], shapes: &[FloatingShape]) -> bool {
    !images.is_empty() || !shapes.is_empty()
}

/// 在 [Content_Types].xml 里补图片扩展名的 Default（已有则不重复）
pub fn append_media_defaults<'a, I>(ct: String, all_images: I) -> String
where
    I: IntoIterator<Item = &'a FloatingImage>,
{
    let mut exts: Vec<&'static str> = Vec::new();
    for img in all_images {
        let e = mime_ext(&img.mime);
        if !exts.contains(&e) {
            exts.push(e);
        }
    }
    let mut ct = ct;
    for e in exts {
        if ct.contains(&format!("Extension=\"{e}\"")) {
            continue;
        }
        let add = format!(
            r#"<Default Extension="{e}" ContentType="{}"/>"#,
            ext_content_type(e)
        );
        ct = ct.replacen("</Types>", &format!("{add}</Types>"), 1);
    }
    ct
}

/// 生成一个 sheet 的图片和图形锚点。
/// first_shape_id：本 sheet drawing 内下一个可用的 cNvPr id（图表之后）。
/// media_no：工作簿内全局媒体序号（调用方跨 sheet 保持，函数内递增）。
pub fn build_objects(
    images: &[FloatingImage],
    shapes: &[FloatingShape],
    first_shape_id: usize,
    media_no: &mut usize,
) -> SheetObjects {
    let mut out = SheetObjects {
        frames: String::new(),
        rels: String::new(),
        media: Vec::new(),
    };
    let mut id = first_shape_id;
    let mut img_k = 0usize;

    for img in images {
        let bytes = match base64::engine::general_purpose::STANDARD.decode(img.data_base64.as_bytes()) {
            Ok(b) if !b.is_empty() => b,
            _ => continue,
        };
        img_k += 1;
        *media_no += 1;
        let ext = mime_ext(&img.mime);
        let rid = format!("rIdI{img_k}");
        out.rels.push_str(&format!(
            r#"<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image{}.{ext}"/>"#,
            *media_no
        ));
        out.media.push((format!("xl/media/image{}.{ext}", *media_no), bytes));
        let (x, y, cx, cy) = (emu(img.x_pt), emu(img.y_pt), emu(img.width_pt), emu(img.height_pt));
        let hidden = if img.hidden { r#" hidden="1""# } else { "" };
        out.frames.push_str(&format!(
            r#"<xdr:absoluteAnchor><xdr:pos x="{x}" y="{y}"/><xdr:ext cx="{cx}" cy="{cy}"/><xdr:pic><xdr:nvPicPr><xdr:cNvPr id="{id}" name="Picture {id}"{hidden}/><xdr:cNvPicPr/></xdr:nvPicPr><xdr:blipFill><a:blip xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill><xdr:spPr><a:xfrm{}><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic><xdr:clientData/></xdr:absoluteAnchor>"#,
            rot_attr(img.rot)
        ));
        id += 1;
    }

    for sh in shapes {
        let (x, y, cx, cy) = (emu(sh.x_pt), emu(sh.y_pt), emu(sh.width_pt), emu(sh.height_pt));
        let mut xfrm_attrs = rot_attr(sh.rot);
        if sh.flip_v {
            xfrm_attrs.push_str(r#" flipV="1""#);
        }
        let (prst, av) = geometry_prst(&sh.geometry);
        let fill = match &sh.fill {
            Some(FillStyle::Solid(c)) => format!(
                r#"<a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill>"#,
                c.r, c.g, c.b
            ),
            _ => "<a:noFill/>".to_string(),
        };
        let tx = text_body(&sh.text_content);
        out.frames.push_str(&format!(
            r#"<xdr:absoluteAnchor><xdr:pos x="{x}" y="{y}"/><xdr:ext cx="{cx}" cy="{cy}"/><xdr:sp macro="" textlink=""><xdr:nvSpPr><xdr:cNvPr id="{id}" name="Shape {id}"/><xdr:cNvSpPr/></xdr:nvSpPr><xdr:spPr><a:xfrm{xfrm_attrs}><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="{}">{av}</a:prstGeom>{fill}</xdr:spPr>{tx}</xdr:sp><xdr:clientData/></xdr:absoluteAnchor>"#,
            esc(&prst)
        ));
        id += 1;
    }
    out
}

/// 几何 → (prst 名, avLst XML)。PresetPath 保留原预设名和调整值。
fn geometry_prst(g: &ShapeGeometry) -> (String, String) {
    match g {
        ShapeGeometry::Rect => ("rect".into(), "<a:avLst/>".into()),
        ShapeGeometry::RoundRect { .. } => ("roundRect".into(), "<a:avLst/>".into()),
        ShapeGeometry::Ellipse => ("ellipse".into(), "<a:avLst/>".into()),
        ShapeGeometry::Triangle => ("triangle".into(), "<a:avLst/>".into()),
        ShapeGeometry::Line => ("line".into(), "<a:avLst/>".into()),
        ShapeGeometry::Custom(s) => (s.clone(), "<a:avLst/>".into()),
        ShapeGeometry::PresetPath(d) => {
            let name = if d.preset_name.is_empty() { "rect".to_string() } else { d.preset_name.clone() };
            if d.adj_overrides.is_empty() {
                (name, "<a:avLst/>".into())
            } else {
                let mut s = String::from("<a:avLst>");
                for (n, v) in &d.adj_overrides {
                    s.push_str(&format!(
                        r#"<a:gd name="{}" fmla="val {}"/>"#,
                        esc(n),
                        v.round() as i64
                    ));
                }
                s.push_str("</a:avLst>");
                (name, s)
            }
        }
    }
}

fn text_body(paras: &[Paragraph]) -> String {
    if paras.is_empty() {
        return String::new();
    }
    let mut s = String::from("<xdr:txBody><a:bodyPr/><a:lstStyle/>");
    for p in paras {
        s.push_str("<a:p>");
        for r in &p.runs {
            if r.text.is_empty() {
                continue;
            }
            let mut attrs = String::from(r#" lang="en-US""#);
            if let Some(sz) = r.font_size {
                attrs.push_str(&format!(r#" sz="{}""#, (sz * 100.0).round() as i64));
            }
            if r.bold == Some(true) {
                attrs.push_str(r#" b="1""#);
            }
            let color = match &r.color {
                Some(c) => format!(
                    r#"<a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill>"#,
                    c.r, c.g, c.b
                ),
                None => String::new(),
            };
            s.push_str(&format!(
                "<a:r><a:rPr{attrs}>{color}</a:rPr><a:t>{}</a:t></a:r>",
                esc(&r.text)
            ));
        }
        s.push_str("</a:p>");
    }
    s.push_str("</xdr:txBody>");
    s
}

/// 把媒体文件写进 zip
pub fn write_media<W: Write + Seek>(
    zw: &mut zip::ZipWriter<W>,
    opts: zip::write::SimpleFileOptions,
    media: &[(String, Vec<u8>)],
) -> Result<(), String> {
    for (path, bytes) in media {
        zw.start_file(path.as_str(), opts).map_err(|e| e.to_string())?;
        zw.write_all(bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::common::Color;
    use roe_model::writer::Run;

    fn img(mime: &str) -> FloatingImage {
        let raw: Vec<u8> = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        FloatingImage {
            id: 0,
            data_base64: base64::engine::general_purpose::STANDARD.encode(&raw),
            mime: mime.to_string(),
            width_pt: 100.0,
            height_pt: 50.0,
            x_pt: 10.0,
            y_pt: 20.0,
            rot: 0.0,
            hidden: false,
        }
    }

    #[test]
    fn image_writes_pic_media_and_rel() {
        let mut n = 0usize;
        let o = build_objects(&[img("image/png")], &[], 2, &mut n);
        assert_eq!(n, 1);
        assert_eq!(o.media.len(), 1);
        assert_eq!(o.media[0].0, "xl/media/image1.png");
        assert!(o.frames.contains("<xdr:pic>"));
        assert!(o.frames.contains(r#"r:embed="rIdI1""#));
        assert!(o.frames.contains(r#"<a:off x="127000" y="254000"/>"#));
        assert!(o.frames.contains(r#"<a:ext cx="1270000" cy="635000"/>"#));
        assert!(o.rels.contains("../media/image1.png"));
    }

    #[test]
    fn media_no_is_global_across_sheets() {
        let mut n = 0usize;
        let _ = build_objects(&[img("image/png")], &[], 2, &mut n);
        let o = build_objects(&[img("image/jpeg")], &[], 2, &mut n);
        assert_eq!(n, 2);
        assert_eq!(o.media[0].0, "xl/media/image2.jpeg");
    }

    #[test]
    fn bad_base64_is_skipped() {
        let mut bad = img("image/png");
        bad.data_base64 = "!!!not base64!!!".to_string();
        let mut n = 0usize;
        let o = build_objects(&[bad], &[], 2, &mut n);
        assert_eq!(n, 0);
        assert!(o.frames.is_empty() && o.media.is_empty());
    }

    #[test]
    fn shape_writes_geometry_fill_and_text() {
        let sh = FloatingShape { stroke: None,
            id: 0,
            x_pt: 1.0,
            y_pt: 2.0,
            width_pt: 30.0,
            height_pt: 40.0,
            rot: 90.0,
            flip_v: true,
            geometry: ShapeGeometry::Ellipse,
            fill: Some(FillStyle::Solid(Color { r: 255, g: 0, b: 16, a: 255 })),
            text_content: vec![Paragraph {
                runs: vec![Run {
                    text: "A&B".to_string(),
                    font_size: Some(12.0),
                    bold: Some(true),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let mut n = 0usize;
        let o = build_objects(&[], &[sh], 5, &mut n);
        assert!(o.frames.contains(r#"cNvPr id="5""#));
        assert!(o.frames.contains(r#"prst="ellipse""#));
        assert!(o.frames.contains(r#"rot="5400000""#));
        assert!(o.frames.contains(r#"flipV="1""#));
        assert!(o.frames.contains(r#"val="FF0010""#));
        assert!(o.frames.contains("A&amp;B"));
        assert!(o.frames.contains(r#"sz="1200""#));
    }

    #[test]
    fn content_types_defaults_added_once() {
        let ct = "<Types></Types>".to_string();
        let imgs = vec![img("image/png"), img("image/png"), img("image/jpeg")];
        let ct = append_media_defaults(ct, imgs.iter());
        assert_eq!(ct.matches(r#"Extension="png""#).count(), 1);
        assert_eq!(ct.matches(r#"Extension="jpeg""#).count(), 1);
    }
}

//! roe_model::impress::Presentation → PPTX (OOXML PresentationML) 字节流
//!
//! 与 pptx.rs parser 对称设计，范围对齐 parser 现状（pptx.rs 文件头注释
//! 自己写明的 MVP 范围：Table/Chart/SmartArt/Animation/Notes/VML 均不支持）：
//!
//! 写出的内容：
//! - Presentation.slide_size（pt，写出时换算回 EMU，×12700）
//! - 每个 Slide 的 shapes：Shape.id / position（Rect，pt→EMU）/ geometry
//!   （ShapeGeometry 几个 preset 变体）/ style.fill（仅 Solid/None，不含
//!   Gradient）/ style.stroke（颜色+宽度）/ content（Text 段落文字 或
//!   Image 内联图片数据）
//!
//! 刻意不写的内容（parser 不读，写了无法 roundtrip 验证）：
//! - Slide.animation_tree / transition / notes：parser 解析后这三项恒为
//!   default()/None/None（pptx.rs 第137-139行），不是从 XML 读回的真实值
//! - master_slides：母版/版式的形状和背景由 write_masters_and_layouts 从内存模型
//!   写回（v320i 往返探针 roundtrip_master_edit 验证）；早期"只写固定最小占位
//!   master"的说法已过时
//! - ShapeStyle.shadow：parser 不读
//! - FillStyle::Gradient：parser 只读 srgbClr（→Solid）和 noFill（→None），
//!   不解析渐变；写出时如果遇到 Gradient 变体，退化为写 noFill（保底，
//!   不报错，但提示性的从根本上说这属于"超出当前 parser 表达能力"的输入）
//! - Paragraph 除 runs[].text 外的字段（alignment等）：parser 不读
//! - Run 除 text 外的字段（bold/italic/font_size等）：parser 不读
//! - stroke 颜色：parser 读 stroke 时颜色硬编码黑色（pptx.rs 第429行
//!   `Color{r:0,g:0,b:0,a:255}`，没有真正解析线条颜色 XML 属性），所以
//!   stroke.color 即使 writer 写出真实颜色，roundtrip 测试也只能断言
//!   读回来的颜色固定是黑色——这是 parser 自身限制，不是 writer 的 bug，
//!   测试里会按这个现状断言，不要误以为是 writer 颜色写错了
//!
//! 单位换算：内存模型一律 pt，OOXML 一律 EMU，统一 ×12700 / ÷12700
//! （与 pptx.rs parser 的换算方向一致，v27/v28 已修复统一）。
//!
//! 生成的最小合法 OPC 包文件清单：
//!   [Content_Types].xml
//!   _rels/.rels
//!   ppt/presentation.xml
//!   ppt/_rels/presentation.xml.rels
//!   ppt/slideMasters/slideMaster1.xml          ← 固定占位，不含真实数据
//!   ppt/slideMasters/_rels/slideMaster1.xml.rels
//!   ppt/slideLayouts/slideLayout1.xml          ← 固定占位，不含真实数据
//!   ppt/slideLayouts/_rels/slideLayout1.xml.rels
//!   ppt/slides/slideN.xml ...
//!   ppt/slides/_rels/slideN.xml.rels ...       ← 仅当该 slide 有图片时含内容
//!   ppt/media/imageN.png/.jpg/.gif/.bin ...    ← 每张内联图片一个文件

use std::io::Write;
use roe_model::common::Transform;

use roe_model::impress::{FillStyle, Presentation, Shape, ShapeContent, ShapeGeometry, Slide};
use roe_model::writer::Paragraph;

use crate::ParseError;

pub struct PptxWriter;

impl PptxWriter {
    /// Presentation 内存模型 → pptx 字节流（zip 包）
    pub fn write(prs: &Presentation) -> Result<Vec<u8>, ParseError> {
        let slide_count = prs.slides.len();
        let masters = fill_masters(prs); // v320c1

        let chart_total: usize = prs.slides.iter().map(|s| s.shapes.iter().filter(|sh| matches!(sh.content, ShapeContent::Chart(_))).count()).sum();
        let stat_total: usize = prs.slides.iter().map(|s| s.shapes.iter().filter(|sh| matches!(sh.content, ShapeContent::StatChart(_))).count()).sum();
        let content_types = w320_content_types(slide_count, &masters).replace("</Types>", &super::chart_writer::content_type_overrides(chart_total));
        let content_types = content_types.replace("</Types>", &super::chartex_writer::content_type_overrides(stat_total));
        let root_rels = write_root_rels();
        let presentation_xml = w320_presentation_xml(prs, &masters);
        let presentation_rels = w320_presentation_rels(slide_count, masters.len());
        // v320c1: master/layout parts are written by write_masters_and_layouts

        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        write_zip_text(&mut zw, opts, "[Content_Types].xml", &content_types)?;
        write_zip_text(&mut zw, opts, "_rels/.rels", &root_rels)?;
        write_zip_text(&mut zw, opts, "ppt/presentation.xml", &presentation_xml)?;
        write_zip_text(&mut zw, opts, "ppt/_rels/presentation.xml.rels", &presentation_rels)?;
        let mut media_counter: u32 = 1;
        write_masters_and_layouts(&mut zw, opts, &masters, &mut media_counter)?;
        write_zip_text(&mut zw, opts, "docProps/core.xml", &write_core_props_xml())?;
        write_zip_text(&mut zw, opts, "docProps/app.xml", &write_app_props_xml())?;
        // v320c1: layout rels written in write_masters_and_layouts

        // 图片去重：同一 Presentation 内不同 Shape 的图片字节各自独立编号，
        // 不做内容去重（parser 解析时也是按 slide 独立收集，不跨 slide 共享）。
        let mut chart_counter: u32 = 0;
        let mut chartex_counter: u32 = 0;

        for (idx, slide) in prs.slides.iter().enumerate() {
            let slide_num = idx + 1;
            let has_chart = slide.shapes.iter().any(|sh| matches!(sh.content, ShapeContent::Chart(_) | ShapeContent::StatChart(_)));
            let stripped;
            let slide_ref: &Slide = if has_chart {
                let mut s2 = slide.clone();
                s2.shapes.retain(|sh| !matches!(sh.content, ShapeContent::Chart(_) | ShapeContent::StatChart(_)));
                stripped = s2;
                &stripped
            } else {
                slide
            };
            let (mut slide_xml, image_rels, images) =
                write_slide_xml(slide_ref, &mut media_counter);
            let mut chart_rel_xml = String::new();
            if has_chart {
                let mut frames = String::new();
                let mut n = 0;
                for sh in &slide.shapes {
                    if let ShapeContent::Chart(cm) = &sh.content {
                        chart_counter += 1;
                        n += 1;
                        let crid = format!("rIdC{n}");
                        frames.push_str(&super::chart_writer::graphic_frame_xml(sh, &crid));
                        chart_rel_xml.push_str(&format!(
                            r#"<Relationship Id="{crid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart{chart_counter}.xml"/>"#
                        ));
                        write_zip_text(
                            &mut zw, opts,
                            &format!("ppt/charts/chart{chart_counter}.xml"),
                            &super::chart_writer::chart_to_xml(cm),
                        )?;
                    } else if let ShapeContent::StatChart(sm) = &sh.content {
                        chartex_counter += 1;
                        n += 1;
                        let xrid = format!("rIdX{n}");
                        frames.push_str(&super::chartex_writer::graphic_frame_xml(sh, &xrid, None));
                        chart_rel_xml.push_str(&format!(
                            r#"<Relationship Id="{xrid}" Type="http://schemas.microsoft.com/office/2014/relationships/chartEx" Target="../charts/chartEx{chartex_counter}.xml"/>"#
                        ));
                        write_zip_text(
                            &mut zw, opts,
                            &format!("ppt/charts/chartEx{chartex_counter}.xml"),
                            &super::chartex_writer::chartex_to_xml(sm),
                        )?;
                    }
                }
                slide_xml = slide_xml.replacen("</p:spTree>", &format!("{frames}</p:spTree>"), 1);
            }

            write_zip_text(
                &mut zw, opts,
                &format!("ppt/slides/slide{slide_num}.xml"),
                &slide_xml,
            )?;

            let layout_rel = format!(r#"<Relationship Id="rIdL1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout{}.xml"/>"#, slide_layout_number(&masters, slide));
            let rels_xml = write_slide_rels(&image_rels).replacen("</Relationships>", &format!("{layout_rel}{chart_rel_xml}</Relationships>"), 1);
            write_zip_text(
                &mut zw, opts,
                &format!("ppt/slides/_rels/slide{slide_num}.xml.rels"),
                &rels_xml,
            )?;

            for (path, data) in images {
                write_zip_bytes(&mut zw, opts, &path, &data)?;
            }
        }

        let cursor = zw
            .finish()
            .map_err(|e| ParseError::Malformed(format!("zip finish failed: {e}")))?;
        Ok(cursor.into_inner())
    }
}

fn write_zip_text<W: Write + std::io::Seek>(
    zw: &mut zip::ZipWriter<W>,
    opts: zip::write::SimpleFileOptions,
    path: &str,
    content: &str,
) -> Result<(), ParseError> {
    zw.start_file(path, opts)
        .map_err(|e| ParseError::Malformed(format!("zip start_file '{path}' failed: {e}")))?;
    zw.write_all(content.as_bytes())
        .map_err(|e| ParseError::Malformed(format!("zip write '{path}' failed: {e}")))?;
    Ok(())
}

fn write_zip_bytes<W: Write + std::io::Seek>(
    zw: &mut zip::ZipWriter<W>,
    opts: zip::write::SimpleFileOptions,
    path: &str,
    data: &[u8],
) -> Result<(), ParseError> {
    zw.start_file(path, opts)
        .map_err(|e| ParseError::Malformed(format!("zip start_file '{path}' failed: {e}")))?;
    zw.write_all(data)
        .map_err(|e| ParseError::Malformed(format!("zip write '{path}' failed: {e}")))?;
    Ok(())
}

// ─── [Content_Types].xml ──────────────────────────────────────────────────────

fn write_content_types(slide_count: usize) -> String {
    let mut overrides = String::new();
    overrides.push_str(
        r#"<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>"#,
    );
    overrides.push_str(
        r#"<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>"#,
    );
    overrides.push_str(
        concat!(
            r#"<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>"#,
            r#"<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>"#,
            r#"<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>"#,
            r#"<Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>"#,
        ),
    );
    for i in 1..=slide_count {
        overrides.push_str(&format!(
            r#"<Override PartName="/ppt/slides/slide{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Default Extension="png" ContentType="image/png"/>
<Default Extension="jpeg" ContentType="image/jpeg"/>
<Default Extension="jpg" ContentType="image/jpeg"/>
<Default Extension="gif" ContentType="image/gif"/>
{overrides}
</Types>"#
    )
}

// ─── _rels/.rels ────────────────────────────────────────────────────────────

fn write_root_rels() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
</Relationships>"#
        .to_string()
}

// ─── ppt/presentation.xml ───────────────────────────────────────────────────

fn write_presentation_xml(prs: &Presentation) -> String {
    write_presentation_xml_base(prs).replace("</p:presentation>", "<p:notesSz cx=\"6858000\" cy=\"9144000\"/>\n</p:presentation>")
}

fn write_presentation_xml_base(prs: &Presentation) -> String {
    let cx = (prs.slide_size.0 * 12700.0).round() as i64;
    let cy = (prs.slide_size.1 * 12700.0).round() as i64;

    let mut sld_id_list = String::new();
    // sldMasterId 固定 rId1（指向唯一的 slideMaster1），slide rId 从 rId2 起
    // （rId 编号规则与 write_presentation_rels 必须一致，两处修改要同步）。
    for (idx, _slide) in prs.slides.iter().enumerate() {
        let rid = idx + 2;
        let sld_id = 256 + idx as u32; // sldId 数值规范要求 >=256，任意取值即可
        sld_id_list.push_str(&format!(
            r#"<p:sldId id="{sld_id}" r:id="rId{rid}"/>"#
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:sldIdLst>{sld_id_list}</p:sldIdLst>
<p:sldSz cx="{cx}" cy="{cy}"/>
</p:presentation>"#
    )
}

// ─── ppt/_rels/presentation.xml.rels ────────────────────────────────────────

fn write_presentation_rels(slide_count: usize) -> String {
    let theme = format!(
        r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="theme/theme1.xml"/>"#,
        slide_count + 2
    );
    write_presentation_rels_base(slide_count).replace("</Relationships>", &format!("{theme}\n</Relationships>"))
}

fn write_presentation_rels_base(slide_count: usize) -> String {
    let mut rels = String::new();
    rels.push_str(
        r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#,
    );
    for i in 1..=slide_count {
        let rid = i + 1; // rId1 被 slideMaster 占用，slide 从 rId2 起
        rels.push_str(&format!(
            r#"<Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{i}.xml"/>"#
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
{rels}
</Relationships>"#
    )
}

// ─── ppt/slideMasters/slideMaster1.xml（固定占位，不含内存模型数据）──────────

fn write_slide_master_xml() -> String {
    write_slide_master_xml_base()
        .replace("<p:cSld><p:spTree>", "<p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg><p:spTree>")
        .replace("</p:sldMaster>", "<p:txStyles><p:titleStyle/><p:bodyStyle/><p:otherStyle/></p:txStyles>\n</p:sldMaster>")
}

fn write_slide_master_xml_base() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
</p:spTree></p:cSld>
<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>"#
        .to_string()
}

fn write_slide_master_rels() -> String {
    write_slide_master_rels_base().replace(
        "</Relationships>",
        "<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"../theme/theme1.xml\"/>\n</Relationships>",
    )
}

fn write_slide_master_rels_base() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#
        .to_string()
}

// ─── ppt/slideLayouts/slideLayout1.xml（固定占位）──────────────────────────

fn write_slide_layout_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" type="blank">
<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
</p:spTree></p:cSld>
</p:sldLayout>"#
        .to_string()
}

fn write_slide_layout_rels() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#
        .to_string()
}

// ─── ppt/slides/slideN.xml ──────────────────────────────────────────────────

/// 返回 (slide_xml, image_rels: Vec<(rId, media文件名)>, images: Vec<(zip内路径, 字节数据)>)
fn write_slide_xml(
    slide: &Slide,
    media_counter: &mut u32,
) -> (String, Vec<(String, String)>, Vec<(String, Vec<u8>)>) {
    let mut shapes_xml = String::new();
    let mut image_rels: Vec<(String, String)> = Vec::new();
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rid_counter: u32 = 1;

    // v318p: 幻灯片背景写出（纯色 / 图片）。渐变与平铺暂不写，保持无 <p:bg>。
    let mut bg_xml = String::new();
    match &slide.background {
        Some(FillStyle::Solid(c)) => {
            bg_xml = format!(
                r#"<p:bg><p:bgPr><a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill><a:effectLst/></p:bgPr></p:bg>"#,
                c.r, c.g, c.b
            );
        }
        Some(FillStyle::Picture(img)) => {
            let ext = mime_to_ext(&img.mime);
            let filename = format!("image{}.{}", *media_counter, ext);
            *media_counter += 1;
            let rid = format!("rId{rid_counter}");
            rid_counter += 1;
            images.push((format!("ppt/media/{filename}"), img.data.clone()));
            image_rels.push((rid.clone(), filename));
            bg_xml = format!(
                r#"<p:bg><p:bgPr><a:blipFill dpi="0" rotWithShape="1"><a:blip r:embed="{rid}"/><a:srcRect/><a:stretch><a:fillRect/></a:stretch></a:blipFill><a:effectLst/></p:bgPr></p:bg>"#
            );
        }
        Some(FillStyle::Gradient { stops, kind, .. }) if !stops.is_empty() => {
            // v318p: 渐变背景写出，角度 = 度数 × 60000
            let mut gs = String::new();
            for s in stops {
                gs.push_str(&format!(
                    r#"<a:gs pos="{}"><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:gs>"#,
                    (s.pos.clamp(0.0, 1.0) * 100000.0).round() as i32,
                    s.color.r, s.color.g, s.color.b
                ));
            }
            let shade = match kind {
                roe_model::common::GradientKind::Linear { angle } => format!(
                    r#"<a:lin ang="{}" scaled="0"/>"#,
                    (angle.rem_euclid(360.0) * 60000.0).round() as i32
                ),
                roe_model::common::GradientKind::Radial => {
                    r#"<a:path path="circle"><a:fillToRect l="50000" t="50000" r="50000" b="50000"/></a:path>"#.to_string()
                }
                roe_model::common::GradientKind::Rect => {
                    r#"<a:path path="rect"><a:fillToRect l="50000" t="50000" r="50000" b="50000"/></a:path>"#.to_string()
                }
            };
            bg_xml = format!(
                r#"<p:bg><p:bgPr><a:gradFill rotWithShape="1"><a:gsLst>{gs}</a:gsLst>{shade}</a:gradFill><a:effectLst/></p:bgPr></p:bg>"#
            );
        }
        _ => {}
    }

    for shape in &slide.shapes {
        match &shape.content {
            ShapeContent::Image(roe_model::common::ImageData { data, mime, .. }) => {
                let ext = mime_to_ext(mime);
                let filename = format!("image{}.{}", *media_counter, ext);
                *media_counter += 1;
                let rid = format!("rId{rid_counter}");
                rid_counter += 1;

                images.push((format!("ppt/media/{filename}"), data.clone()));
                image_rels.push((rid.clone(), filename));

                shapes_xml.push_str(&write_pic_xml(shape, &rid));
            }
            // v183: 此前 Group 走 `_` 兜底分支，被当成普通 <p:sp> 写出，
            // children 被完全静默丢弃（写出后读回是一个空文本框，原有
            // 分组内容全部消失）。round-trip 测试挑真实样本
            // tdf141463_GroupTransform.pptx 时发现，pptx.rs parser 侧
            // test_pptx9b_group_shape_parsing 早已验证了 <p:grpSp> 的
            // 读入格式，此处对称写出，见 write_grp_sp_xml。
            ShapeContent::Group { children, ch_off, ch_ext } => {
                let (xml, rels, imgs) = write_grp_sp_xml(
                    shape, children, ch_off, ch_ext, media_counter, &mut rid_counter,
                );
                shapes_xml.push_str(&xml);
                image_rels.extend(rels);
                images.extend(imgs);
            }
            ShapeContent::Table(table) => {
                shapes_xml.push_str(&write_table_graphic_frame_xml(shape, table));
            }
            _ => {
                shapes_xml.push_str(&write_sp_xml(shape));
            }
        }
    }

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<p:cSld>{bg_xml}<p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr/>
{shapes_xml}</p:spTree></p:cSld>
</p:sld>"#
    );

    (xml, image_rels, images)
}

fn write_slide_rels(image_rels: &[(String, String)]) -> String {
    let mut rels = String::new();
    for (rid, filename) in image_rels {
        rels.push_str(&format!(
            r#"<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/{filename}"/>"#
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
{rels}
</Relationships>"#
    )
}

// ─── Shape → <p:sp> ──────────────────────────────────────────────────────────

// v183: <p:grpSp> 写出，与 pptx.rs parser 的 test_pptx9b_group_shape_parsing
// 读入格式严格对称（层级栈解析 vs 递归写出，是同一棵树的两种遍历方向）。
// 递归处理嵌套 Group（children 里可能还有 Group），与 parser 侧"嵌套分组
// 天然生效，不需要额外代码路径"的设计对称。
// media_counter/rid_counter 是外部（slide级）计数器的可变引用，保证同一
// slide 内所有图片（无论嵌套在哪层 Group 里）文件名/rId 不冲突。
fn write_grp_sp_xml(
    shape: &Shape,
    children: &[Shape],
    ch_off: &Option<(f32, f32)>,
    ch_ext: &Option<(f32, f32)>,
    media_counter: &mut u32,
    rid_counter: &mut u32,
) -> (String, Vec<(String, String)>, Vec<(String, Vec<u8>)>) {
    let mut image_rels: Vec<(String, String)> = Vec::new();
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();

    let x = (shape.position.x() * 12700.0).round() as i64;
    let y = (shape.position.y() * 12700.0).round() as i64;
    let w = (shape.position.w() * 12700.0).round() as i64;
    let h = (shape.position.h() * 12700.0).round() as i64;

    let mut xfrm_attrs = String::new();
    if let Some(rot) = shape.position.rotation {
        let rot_raw = (rot * 60000.0).round() as i64;
        xfrm_attrs.push_str(&format!(r#" rot="{rot_raw}""#));
    }
    if shape.position.flip_h.unwrap_or(false) {
        xfrm_attrs.push_str(r#" flipH="1""#);
    }
    if shape.position.flip_v.unwrap_or(false) {
        xfrm_attrs.push_str(r#" flipV="1""#);
    }

    // chOff/chExt 仅当 Some 时写出，None 时省略这两个子标签——与 parser
    // 侧对称：parser 未见 chOff/chExt 子元素时，group_ch_off/ch_ext 保持
    // None（见 pptx.rs 第2134/2141行 guard），不会凭空产生 (0,0) 之类的
    // 错误默认值。
    let ch_off_ext = match (ch_off, ch_ext) {
        (Some((cx_off, cy_off)), Some((cx_ext, cy_ext))) => {
            let cx_off_raw = (cx_off * 12700.0).round() as i64;
            let cy_off_raw = (cy_off * 12700.0).round() as i64;
            let cx_ext_raw = (cx_ext * 12700.0).round() as i64;
            let cy_ext_raw = (cy_ext * 12700.0).round() as i64;
            format!(
                r#"<a:chOff x="{cx_off_raw}" y="{cy_off_raw}"/><a:chExt cx="{cx_ext_raw}" cy="{cy_ext_raw}"/>"#
            )
        }
        _ => String::new(),
    };

    let mut children_xml = String::new();
    for child in children {
        match &child.content {
            ShapeContent::Image(roe_model::common::ImageData { data, mime, .. }) => {
                let ext = mime_to_ext(mime);
                let filename = format!("image{}.{}", *media_counter, ext);
                *media_counter += 1;
                let rid = format!("rId{rid_counter}");
                *rid_counter += 1;

                images.push((format!("ppt/media/{filename}"), data.clone()));
                image_rels.push((rid.clone(), filename));

                children_xml.push_str(&write_pic_xml(child, &rid));
            }
            ShapeContent::Group { children: nested, ch_off: nested_off, ch_ext: nested_ext } => {
                let (xml, rels, imgs) = write_grp_sp_xml(
                    child, nested, nested_off, nested_ext, media_counter, rid_counter,
                );
                children_xml.push_str(&xml);
                image_rels.extend(rels);
                images.extend(imgs);
            }
            _ => {
                children_xml.push_str(&write_sp_xml(child));
            }
        }
    }

    let xml = format!(
        r#"<p:grpSp>
<p:nvGrpSpPr><p:cNvPr id="{}" name="Group {}"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr><a:xfrm{xfrm_attrs}><a:off x="{x}" y="{y}"/><a:ext cx="{w}" cy="{h}"/>{ch_off_ext}</a:xfrm></p:grpSpPr>
{children_xml}</p:grpSp>
"#,
        shape.id, shape.id
    );

    (xml, image_rels, images)
}

fn write_sp_xml(shape: &Shape) -> String {
    let (off_ext, prst) = write_xfrm_and_geom(&shape.position, &shape.geometry);
    let fill_xml = write_fill_xml(&shape.style.fill);
    let stroke_xml = write_stroke_xml(&shape.style.stroke);
    let tx_body = write_tx_body(&shape.content, &shape.text_body);
    // v320c2: write placeholder identity
    let nv_pr = match &shape.placeholder {
        Some(ph) => {
            let mut a = String::new();
            if let Some(t) = &ph.ph_type { a.push_str(&format!(" type=\"{}\"", xml_escape_text(t))); }
            if let Some(i) = ph.index { a.push_str(&format!(" idx=\"{}\"", i)); }
            format!("<p:nvPr><p:ph{}/></p:nvPr>", a)
        }
        None => "<p:nvPr/>".to_string(),
    };

    format!(
        r#"<p:sp>
<p:nvSpPr><p:cNvPr id="{}" name="Shape {}"/><p:cNvSpPr/>{nv_pr}</p:nvSpPr>
<p:spPr>{off_ext}<a:prstGeom prst="{prst}"/>{fill_xml}{stroke_xml}</p:spPr>
{tx_body}</p:sp>
"#,
        shape.id, shape.id
    )
}

// ─── Shape(Image) → <p:pic> ──────────────────────────────────────────────────

/// v318r：Table 写成 <p:graphicFrame><a:tbl>。与 pptx.rs parse_table 对称：
/// gridCol w、tr h、tblPr 开关/tableStyleId、tc 的 gridSpan/rowSpan/hMerge/vMerge、
/// tcPr 背景色、txBody 段落。单元格内图片/嵌套表格（PPTX 不支持）忽略。
fn write_table_graphic_frame_xml(shape: &Shape, table: &roe_model::common::Table) -> String {
    use roe_model::common::CellContent;
    let x = (shape.position.x() * 12700.0).round() as i64;
    let y = (shape.position.y() * 12700.0).round() as i64;
    let w = (shape.position.w() * 12700.0).round() as i64;
    let h = (shape.position.h() * 12700.0).round() as i64;

    let b01 = |v: bool| if v { "1" } else { "0" };
    let mut tbl_pr = format!(
        r#"<a:tblPr firstRow="{}" firstCol="{}" lastRow="{}" lastCol="{}" bandRow="{}" bandCol="{}""#,
        b01(table.first_row), b01(table.first_col), b01(table.last_row),
        b01(table.last_col), b01(table.band_row), b01(table.band_col),
    );
    match &table.style_id {
        Some(id) => tbl_pr.push_str(&format!(r#"><a:tableStyleId>{}</a:tableStyleId></a:tblPr>"#, xml_escape_text(id))),
        None => tbl_pr.push_str("/>"),
    }

    let mut grid = String::from("<a:tblGrid>");
    for cw in &table.col_widths {
        grid.push_str(&format!(r#"<a:gridCol w="{}"/>"#, (cw * 12700.0).round() as i64));
    }
    grid.push_str("</a:tblGrid>");

    let mut rows_xml = String::new();
    for row in &table.rows {
        let rh = (row.row_height.unwrap_or(28.0) * 12700.0).round() as i64;
        rows_xml.push_str(&format!(r#"<a:tr h="{rh}">"#));
        for cell in &row.cells {
            let mut attrs = String::new();
            if cell.col_span > 1 { attrs.push_str(&format!(r#" gridSpan="{}""#, cell.col_span)); }
            if cell.row_span > 1 { attrs.push_str(&format!(r#" rowSpan="{}""#, cell.row_span)); }
            if cell.hidden { attrs.push_str(r#" hMerge="1""#); }
            let mut paras = String::new();
            for c in &cell.content {
                if let CellContent::Paragraph(p) = c {
                    paras.push_str(&write_paragraph_xml(p));
                }
            }
            if paras.is_empty() { paras.push_str("<a:p/>"); }
            let mut tc_inner = String::new();
            if let Some(bc) = &cell.border_color {
                let bw = (cell.border_width.unwrap_or(1.0) * 12700.0).round() as i64;
                for tag in ["lnL", "lnR", "lnT", "lnB"] {
                    tc_inner.push_str(&format!(
                        r#"<a:{tag} w="{bw}"><a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill></a:{tag}>"#,
                        bc.r, bc.g, bc.b
                    ));
                }
            }
            if let Some(c) = &cell.bg_color {
                tc_inner.push_str(&format!(
                    r#"<a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill>"#,
                    c.r, c.g, c.b
                ));
            }
            let tc_pr = if tc_inner.is_empty() { String::new() } else { format!("<a:tcPr>{tc_inner}</a:tcPr>") };
            rows_xml.push_str(&format!(
                r#"<a:tc{attrs}><a:txBody><a:bodyPr/><a:lstStyle/>{paras}</a:txBody>{tc_pr}</a:tc>"#
            ));
        }
        rows_xml.push_str("</a:tr>");
    }

    format!(
        r#"<p:graphicFrame>
<p:nvGraphicFramePr><p:cNvPr id="{id}" name="Table {id}"/><p:cNvGraphicFramePr><a:graphicFrameLocks noGrp="1"/></p:cNvGraphicFramePr><p:nvPr/></p:nvGraphicFramePr>
<p:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{w}" cy="{h}"/></p:xfrm>
<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl>{tbl_pr}{grid}{rows_xml}</a:tbl></a:graphicData></a:graphic>
</p:graphicFrame>
"#,
        id = shape.id
    )
}

fn write_pic_xml(shape: &Shape, rid: &str) -> String {
    let (off_ext, _prst) = write_xfrm_and_geom(&shape.position, &ShapeGeometry::Rect);

    format!(
        r#"<p:pic>
<p:nvPicPr><p:cNvPr id="{}" name="Picture {}"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>
<p:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>
<p:spPr>{off_ext}<a:prstGeom prst="rect"/></p:spPr>
</p:pic>
"#,
        shape.id, shape.id
    )
}

// ─── 共用：position/geometry → <a:xfrm>+<a:off>+<a:ext>，geometry → prst 字符串 ──

fn write_xfrm_and_geom(position: &roe_model::common::Transform, geometry: &ShapeGeometry) -> (String, String) {
    let x = (position.x() * 12700.0).round() as i64;
    let y = (position.y() * 12700.0).round() as i64;
    let w = (position.w() * 12700.0).round() as i64;
    let h = (position.h() * 12700.0).round() as i64;

    // v183: 此前 xfrm 只写 off/ext，rot/flipH/flipV 完全丢失（写出后读回
    // 恒为 None/false），是 pptx_writer.rs 文件头注释未提及的静默缺口。
    // 换算公式与 pptx.rs parser 对称：raw.rot(60000分之一度) <-> rotation(度)，
    // 见 pptx.rs 第3612行 `rotation: raw.rot.map(|r| r as f32 / 60000.0)`。
    let mut xfrm_attrs = String::new();
    if let Some(rot) = position.rotation {
        let rot_raw = (rot * 60000.0).round() as i64;
        xfrm_attrs.push_str(&format!(r#" rot="{rot_raw}""#));
    }
    if position.flip_h.unwrap_or(false) {
        xfrm_attrs.push_str(r#" flipH="1""#);
    }
    if position.flip_v.unwrap_or(false) {
        xfrm_attrs.push_str(r#" flipV="1""#);
    }

    let off_ext = format!(
        r#"<a:xfrm{xfrm_attrs}><a:off x="{x}" y="{y}"/><a:ext cx="{w}" cy="{h}"/></a:xfrm>"#
    );

    let prst = geometry_to_prst(geometry);

    (off_ext, prst)
}

/// ShapeGeometry → OOXML prstGeom 的 prst 属性值
/// 与 pptx.rs parser 的 raw_to_shape() 反向映射严格对称：
///   "ellipse"/"oval" → Ellipse（parser 把两者都映射到 Ellipse，写出时
///   统一选 "ellipse"，"oval" 是 parser 端的同义兼容，不需要 writer 区分）
///   "roundRect" → RoundRect{radius}（radius 字段 parser 不读，固定值，
///   写出时也不依赖 radius 数值，OOXML 用 a:avLst 里的 adj 控制，本版本
///   不写 adj，使用 PowerPoint 默认圆角）
///   "triangle"/"rtTriangle" → Triangle（parser 两者都映射到 Triangle，
///   写出时统一选 "triangle"）
///   "line" → Line
///   Custom(s) → 原样写回 s（parser 对未知 preset 字符串原样存进 Custom，
///   写出时原样写回，对称）
///   默认/无 → Rect → "rect"
fn geometry_to_prst(geometry: &ShapeGeometry) -> String {
    match geometry {
        ShapeGeometry::Rect => "rect".to_string(),
        ShapeGeometry::RoundRect { .. } => "roundRect".to_string(),
        ShapeGeometry::Ellipse => "ellipse".to_string(),
        ShapeGeometry::Triangle => "triangle".to_string(),
        ShapeGeometry::Line => "line".to_string(),
        ShapeGeometry::Custom(s) => s.clone(),
        ShapeGeometry::PresetPath(_) => "custom".to_string(),
    }
}

// ─── Fill ──────────────────────────────────────────────────────────────────

fn write_fill_xml(fill: &Option<FillStyle>) -> String {
    match fill {
        Some(FillStyle::Solid(c)) => {
            format!(
                r#"<a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill>"#,
                c.r, c.g, c.b
            )
        }
        // None 变体 与 fill 字段本身是 Option::None 都写 noFill：
        // parser 读 noFill 标签时产出 Some(FillStyle::None)，不是 Option::None
        // （pptx.rs 第419-422行），所以两种"无填充"输入都归一化成同一种输出，
        // 下次 parse 读回的都是 Some(FillStyle::None)，与 Option::None 输入
        // 不能 roundtrip 回 Option::None——这是有意选择：parser 本就无法
        // 区分"完全没写 spPr 填充标签"和"显式写了 noFill"，统一按 noFill 处理
        // 更安全（不会让 PowerPoint 默认填充意外出现）。
        Some(FillStyle::None) | None => "<a:noFill/>".to_string(),
        // Gradient：parser 不支持解析，写出时退化为 noFill（保底，不报错）。
        Some(FillStyle::Gradient { .. }) => "<a:noFill/>".to_string(),
        // PROOT-005: FillStyle::Picture 目前只会出现在 Slide/MasterSlide.background
        // （<p:bg> 场景），本函数是普通 shape 的 <p:spPr> 填充写出，不会实际遇到
        // 这个变体（背景走独立的写出路径，未接入本函数）。写在这里只是让 match
        // 穷尽，降级为 noFill 保底，不报错。
        Some(FillStyle::Picture(_)) => "<a:noFill/>".to_string(),
        // ROE-TILE-001: FillStyle::PictureTiled 同 Picture，只出现在
        // Slide/MasterSlide.background（<p:bg> 场景），背景走独立写出路径，
        // 本函数（普通 shape 的 <p:spPr> 填充）不会真的遇到。写出功能本次
        // 未实现（本次任务只做读取解析+渲染显示），降级为 noFill 保底，
        // 不报错，与 Picture 分支保持同样处理方式。
        Some(FillStyle::PictureTiled { .. }) => "<a:noFill/>".to_string(),
    }
}

// ─── Stroke ──────────────────────────────────────────────────────────────────

fn write_stroke_xml(stroke: &Option<roe_model::impress::StrokeStyle>) -> String {
    match stroke {
        Some(s) => {
            let w = (s.width * 12700.0).round() as i64;
            // <a:ln> 必须写成自闭合标签：pptx.rs parser 处理 b"ln" 只在
            // Event::Empty 分支（自闭合 <tag/> 才会触发），Event::Start
            // 分支（<tag>...</tag> 展开形式）没有对应处理，写成展开形式会
            // 导致 parser 完全读不到这个标签（2026-06-20 实测踩坑，见
            // test_roundtrip_stroke_width 修复记录）。
            // 颜色子元素本就去掉：parser 读 stroke 颜色硬编码黑色
            // （pptx.rs 第429行），写真实颜色没有验证意义，且写成子元素
            // 还会把 <a:ln> 变成展开标签，反而读不到宽度，因小失大。
            format!(r#"<a:ln w="{w}"/>"#)
        }
        None => String::new(),
    }
}

// ─── TextBody ──────────────────────────────────────────────────────────────

/// v219: 把 TextBodyProps 写成 <a:bodyPr ...>（与 pptx.rs 读入端对应）。
fn write_body_pr(tb: &roe_model::impress::TextBodyProps) -> String {
    use roe_model::impress::{TextAnchor, TextAutofit, TextVert};
    let mut attrs = String::new();
    match tb.vert {
        TextVert::Horizontal => {}
        TextVert::Vert => attrs.push_str(r#" vert="vert""#),
        TextVert::Vert270 => attrs.push_str(r#" vert="vert270""#),
        TextVert::EaVert => attrs.push_str(r#" vert="eaVert""#),
    }
    if !tb.wrap { attrs.push_str(r#" wrap="none""#); }
    for (name, v) in [("lIns", tb.ins_l), ("tIns", tb.ins_t), ("rIns", tb.ins_r), ("bIns", tb.ins_b)] {
        if let Some(x) = v {
            attrs.push_str(&format!(r#" {name}="{}""#, (x * 12700.0).round() as i64));
        }
    }
    match tb.anchor {
        TextAnchor::Top => {}
        TextAnchor::Middle => attrs.push_str(r#" anchor="ctr""#),
        TextAnchor::Bottom => attrs.push_str(r#" anchor="b""#),
    }
    let child = match &tb.autofit {
        TextAutofit::NoAutofit => String::new(),
        TextAutofit::SpAutoFit => "<a:spAutoFit/>".to_string(),
        TextAutofit::NormAutofit { font_scale, ln_spc_reduction } => {
            let mut a = String::new();
            if let Some(f) = font_scale { a.push_str(&format!(r#" fontScale="{}""#, (f * 100000.0).round() as i64)); }
            if let Some(l) = ln_spc_reduction { a.push_str(&format!(r#" lnSpcReduction="{}""#, (l * 100000.0).round() as i64)); }
            format!("<a:normAutofit{a}/>")
        }
    };
    if child.is_empty() { format!("<a:bodyPr{attrs}/>") } else { format!("<a:bodyPr{attrs}>{child}</a:bodyPr>") }
}

/// v219: 最小合法主题（颜色/字体/格式三件套，WPS 和 OnlyOffice 要求母版必须挂主题）。
fn write_theme_xml() -> String {
    let ln = |w: u32| format!(r#"<a:ln w="{w}"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>"#);
    let fills = r#"<a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill>"#;
    let eff = r#"<a:effectStyle><a:effectLst/></a:effectStyle>"#;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office Theme"><a:themeElements><a:clrScheme name="Office"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="44546A"/></a:dk2><a:lt2><a:srgbClr val="E7E6E6"/></a:lt2><a:accent1><a:srgbClr val="5B9BD5"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="4472C4"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="Office"><a:majorFont><a:latin typeface="Arial"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Arial"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Office"><a:fillStyleLst>{fills}</a:fillStyleLst><a:lnStyleLst>{}{}{}</a:lnStyleLst><a:effectStyleLst>{eff}{eff}{eff}</a:effectStyleLst><a:bgFillStyleLst>{fills}</a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#,
        ln(9525), ln(25400), ln(38100)
    )
}

fn write_core_props_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><dc:title></dc:title><dc:creator>office-rs2</dc:creator></cp:coreProperties>"#
        .to_string()
}

fn write_app_props_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>office-rs2</Application></Properties>"#
        .to_string()
}

fn write_tx_body(content: &ShapeContent, tb: &roe_model::impress::TextBodyProps) -> String {
    match content {
        ShapeContent::Text(paras) => {
            let mut paras_xml = String::new();
            for p in paras {
                paras_xml.push_str(&write_paragraph_xml(p));
            }
            format!("<p:txBody>{}<a:lstStyle/>{paras_xml}</p:txBody>\n", write_body_pr(tb))
        }
        // Empty / Image / Chart：不写 txBody（parser 对没有 txBody 的 shape
        // 产出 ShapeContent::Empty，对称）
        _ => String::new(),
    }
}

fn write_paragraph_xml(p: &Paragraph) -> String {
    let mut runs_xml = String::new();
    for run in &p.runs {
        if run.text.is_empty() {
            continue;
        }
        // "\n" 文本（parser 把 <a:br/> 换行符解析成文字 "\n" 塞进 run，
        // pptx.rs 第434-437行）写出时还原成 <a:br/> 标签，保持对称；
        // 其余文字内容写进 <a:t>。
        if run.text == "\n" {
            runs_xml.push_str("<a:br/>");
        } else {
            runs_xml.push_str(&format!(
                r#"<a:r>{}<a:t>{}</a:t></a:r>"#,
                write_rpr_xml_v319a(run),
                xml_escape_text(&run.text)
            ));
        }
    }
    format!("<a:p>{}{runs_xml}</a:p>", write_ppr_xml_v319a(p))
}

// ─── mime → 文件扩展名 ─────────────────────────────────────────────────────────

/// 与 pptx.rs parser 的 raw_to_shape() mime 推断逻辑对称（按文件 magic bytes
/// 推断 mime，这里反向从 mime 推文件扩展名，用于 ppt/media/ 文件命名）
fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/gif" => "gif",
        _ => "bin", // image/unknown 等，扩展名退化为 .bin
    }
}

// ─── XML 转义 ──────────────────────────────────────────────────────────────────

fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ─── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::pptx::PptxParser;
    use roe_model::common::Color;
    use roe_model::impress::{AnimationNode, MasterSlide, ShapeStyle, StrokeStyle};
    use roe_model::writer::Run;

    fn make_shape(id: u32, geometry: ShapeGeometry, content: ShapeContent) -> Shape {
        Shape {
            id,
            text_body: roe_model::impress::TextBodyProps::default(),
            geometry,
            position: Transform::from_rect(36.0, 36.0, 200.0, 100.0),
            style: ShapeStyle::default(),
            content,
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        }
    }

    fn make_text_shape(id: u32, text: &str) -> Shape {
        let para = Paragraph {
            style: roe_model::common::StyleRef("".to_string()),
            runs: vec![Run { text: text.to_string(), ..Default::default() }],
            list_info: None,
            ..Default::default()
        };
        make_shape(id, ShapeGeometry::Rect, ShapeContent::Text(vec![para]))
    }

    fn make_slide(shapes: Vec<Shape>) -> Slide {
        Slide { layout_idx: None,
            shapes,
            master_idx: None,
            animation_tree: AnimationNode::default(),
            transition: None,
            background: None,
            layout_shapes: vec![],
            notes: None,
        }
    }

    fn make_presentation(slides: Vec<Slide>) -> Presentation {
        Presentation {
            styles: Default::default(),
            slide_size: (720.0, 540.0), // 10in x 7.5in in pt
            slides,
            master_slides: vec![MasterSlide { theme_xml: None, tx_styles_xml: None, layouts: Vec::new(), color_map: None, raw_ext: None, name: "dummy".to_string(), shapes: vec![], background: None }],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        }
    }

    #[test]
    fn test_roundtrip_slide_count() {
        let prs = make_presentation(vec![
            make_slide(vec![make_text_shape(1, "Slide 1")]),
            make_slide(vec![make_text_shape(1, "Slide 2")]),
            make_slide(vec![make_text_shape(1, "Slide 3")]),
        ]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");
        assert_eq!(parsed.slides.len(), 3);
    }

    #[test]
    fn test_roundtrip_slide_size() {
        let prs = make_presentation(vec![make_slide(vec![])]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");
        assert!((parsed.slide_size.0 - 720.0).abs() < 0.01);
        assert!((parsed.slide_size.1 - 540.0).abs() < 0.01);
    }

    #[test]
    fn test_roundtrip_shape_text() {
        let prs = make_presentation(vec![make_slide(vec![make_text_shape(1, "Hello PPTX")])]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let shape = &parsed.slides[0].shapes[0];
        match &shape.content {
            ShapeContent::Text(paras) => {
                let text: String = paras.iter()
                    .flat_map(|p| p.runs.iter().map(|r| r.text.as_str()))
                    .collect();
                assert!(text.contains("Hello PPTX"), "got: {text}");
            }
            other => panic!("expected Text content, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_shape_position() {
        let prs = make_presentation(vec![make_slide(vec![make_text_shape(1, "Pos test")])]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let shape = &parsed.slides[0].shapes[0];
        assert!((shape.position.x() - 36.0).abs() < 0.1, "x={}", shape.position.x());
        assert!((shape.position.y() - 36.0).abs() < 0.1, "y={}", shape.position.y());
        assert!((shape.position.w() - 200.0).abs() < 0.1, "w={}", shape.position.w());
        assert!((shape.position.h() - 100.0).abs() < 0.1, "h={}", shape.position.h());
    }

    #[test]
    fn test_roundtrip_geometry() {
        let prs = make_presentation(vec![make_slide(vec![
            make_shape(1, ShapeGeometry::Ellipse, ShapeContent::Empty),
        ])]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let shape = &parsed.slides[0].shapes[0];
        assert!(matches!(shape.geometry, ShapeGeometry::Ellipse));
    }

    #[test]
    fn test_roundtrip_fill_solid() {
        let mut shape = make_shape(1, ShapeGeometry::Rect, ShapeContent::Empty);
        shape.style.fill = Some(FillStyle::Solid(Color { r: 0xFF, g: 0x57, b: 0x33, a: 255 }));
        let prs = make_presentation(vec![make_slide(vec![shape])]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let s = &parsed.slides[0].shapes[0];
        match &s.style.fill {
            Some(FillStyle::Solid(c)) => {
                assert_eq!(c.r, 0xFF);
                assert_eq!(c.g, 0x57);
                assert_eq!(c.b, 0x33);
            }
            other => panic!("expected Solid fill, got {:?}", other),
        }
    }

    // v318p: 渐变背景往返
    #[test]
    fn test_roundtrip_slide_background_gradient_linear() {
        let mut slide = make_slide(vec![make_text_shape(1, "bg")]);
        slide.background = Some(FillStyle::Gradient {
            stops: vec![
                roe_model::common::GradientStop { pos: 0.0, color: Color { r: 0xFF, g: 0x00, b: 0x00, a: 255 } },
                roe_model::common::GradientStop { pos: 1.0, color: Color { r: 0x00, g: 0x00, b: 0xFF, a: 255 } },
            ],
            kind: roe_model::common::GradientKind::Linear { angle: 90.0 },
            rotate_with_shape: true,
        });
        let prs = make_presentation(vec![slide]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        match &parsed.slides[0].background {
            Some(FillStyle::Gradient { stops, kind, .. }) => {
                assert_eq!(stops.len(), 2);
                assert_eq!((stops[0].color.r, stops[0].color.g, stops[0].color.b), (0xFF, 0, 0));
                assert_eq!((stops[1].color.r, stops[1].color.g, stops[1].color.b), (0, 0, 0xFF));
                match kind {
                    roe_model::common::GradientKind::Linear { angle } => {
                        assert!((angle - 90.0).abs() < 0.01, "angle={}", angle);
                    }
                    _ => panic!("expected linear"),
                }
            }
            other => panic!("expected gradient background, got is_some={}", other.is_some()),
        }
    }

    #[test]
    fn test_roundtrip_slide_background_gradient_radial() {
        let mut slide = make_slide(vec![make_text_shape(1, "bg")]);
        slide.background = Some(FillStyle::Gradient {
            stops: vec![
                roe_model::common::GradientStop { pos: 0.0, color: Color { r: 1, g: 2, b: 3, a: 255 } },
                roe_model::common::GradientStop { pos: 1.0, color: Color { r: 4, g: 5, b: 6, a: 255 } },
            ],
            kind: roe_model::common::GradientKind::Radial,
            rotate_with_shape: true,
        });
        let prs = make_presentation(vec![slide]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");
        match &parsed.slides[0].background {
            Some(FillStyle::Gradient { kind, .. }) => {
                assert!(matches!(kind, roe_model::common::GradientKind::Radial));
            }
            other => panic!("expected gradient background, got is_some={}", other.is_some()),
        }
    }

    // v318p: 幻灯片背景往返（纯色 / 图片）
    #[test]
    fn test_roundtrip_slide_background_solid() {
        let mut slide = make_slide(vec![make_text_shape(1, "bg")]);
        slide.background = Some(FillStyle::Solid(Color { r: 0xFF, g: 0xC0, b: 0x00, a: 255 }));
        let prs = make_presentation(vec![slide]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        match &parsed.slides[0].background {
            Some(FillStyle::Solid(c)) => {
                assert_eq!((c.r, c.g, c.b), (0xFF, 0xC0, 0x00));
            }
            other => panic!("expected solid background, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_slide_background_picture() {
        let png_bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        ];
        let mut slide = make_slide(vec![make_text_shape(1, "bg")]);
        slide.background = Some(FillStyle::Picture(roe_model::common::ImageData {
            data: png_bytes.clone(),
            mime: "image/png".to_string(),
            crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
        }));
        let prs = make_presentation(vec![slide]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        match &parsed.slides[0].background {
            Some(FillStyle::Picture(img)) => {
                assert_eq!(img.data, png_bytes);
                assert_eq!(img.mime, "image/png");
            }
            other => panic!("expected picture background, got is_some={}", other.is_some()),
        }
    }

    #[test]
    fn test_roundtrip_fill_none() {
        let mut shape = make_shape(1, ShapeGeometry::Rect, ShapeContent::Empty);
        shape.style.fill = Some(FillStyle::None);
        let prs = make_presentation(vec![make_slide(vec![shape])]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let s = &parsed.slides[0].shapes[0];
        assert!(matches!(s.style.fill, Some(FillStyle::None)));
    }

    #[test]
    fn test_roundtrip_stroke_width() {
        // 颜色读回固定是黑色（parser 限制，见文件头注释），这里只验证宽度。
        let mut shape = make_shape(1, ShapeGeometry::Rect, ShapeContent::Empty);
        shape.style.stroke = Some(StrokeStyle {
            color: Color { r: 10, g: 20, b: 30, a: 255 },
            width: 2.0,
        });
        let prs = make_presentation(vec![make_slide(vec![shape])]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let s = &parsed.slides[0].shapes[0];
        let stroke = s.style.stroke.as_ref().expect("expected stroke");
        assert!((stroke.width - 2.0).abs() < 0.01, "width={}", stroke.width);
        // 颜色固定黑色是 parser 现状，不是 writer bug：
        assert_eq!(stroke.color.r, 0);
        assert_eq!(stroke.color.g, 0);
        assert_eq!(stroke.color.b, 0);
    }

    #[test]
    fn test_roundtrip_image() {
        // 1x1 红色 PNG（最小合法 PNG 字节）
        let png_bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk start
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
            0xDE,
        ];
        let shape = make_shape(
            1,
            ShapeGeometry::Rect,
            ShapeContent::Image(roe_model::common::ImageData { data: png_bytes.clone(), mime: "image/png".to_string(), ..Default::default() }),
        );
        let prs = make_presentation(vec![make_slide(vec![shape])]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let s = &parsed.slides[0].shapes[0];
        match &s.content {
            ShapeContent::Image(roe_model::common::ImageData { data, mime, .. }) => {
                assert_eq!(data, &png_bytes);
                assert_eq!(mime, "image/png");
            }
            other => panic!("expected Image content, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_table_v318r() {
        use roe_model::common::{CellContent, Table, TableCell, TableRow};
        let mk = |text: &str| {
            let mut p = Paragraph::default();
            if !text.is_empty() {
                p.runs.push(roe_model::writer::Run { text: text.to_string(), ..Default::default() });
            }
            TableCell {
                content: vec![CellContent::Paragraph(p)],
                col_span: 1, row_span: 1, style: None, hidden: false,
                bg_color: None, border_color: None, border_width: None,
                pad_left: None, pad_right: None, pad_top: None, pad_bottom: None,
            }
        };
        let table = Table {
            rows: vec![
                TableRow { cells: vec![mk("A1"), mk("B1"), mk("")], row_height: Some(30.0) },
                TableRow { cells: vec![mk("A2"), mk("B2"), mk("C2")], row_height: Some(30.0) },
            ],
            col_widths: vec![100.0, 120.0, 80.0],
            style_id: Some("{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}".to_string()),
            first_row: true, first_col: false, last_row: false, last_col: false,
            band_row: true, band_col: false,
            default_border_color: None, default_border_width: None,
            inside_h_color: None, inside_h_width: None,
            inside_v_color: None, inside_v_width: None,
            float_pos: None,
        };
        let mut shape = make_shape(1, ShapeGeometry::Rect, ShapeContent::Table(table));
        shape.position = roe_model::common::Transform::from_rect(50.0, 60.0, 300.0, 60.0);
        let prs = make_presentation(vec![make_slide(vec![shape])]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let s = &parsed.slides[0].shapes[0];
        assert!((s.position.x() - 50.0).abs() < 0.5, "x={}", s.position.x());
        assert!((s.position.y() - 60.0).abs() < 0.5, "y={}", s.position.y());
        match &s.content {
            ShapeContent::Table(t) => {
                assert_eq!(t.rows.len(), 2);
                assert_eq!(t.rows[0].cells.len(), 3);
                assert_eq!(t.col_widths.len(), 3);
                assert!((t.col_widths[1] - 120.0).abs() < 0.1);
                assert!(t.first_row && t.band_row && !t.first_col);
                assert_eq!(t.style_id.as_deref(), Some("{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}"));
                let text_of = |r: usize, c: usize| -> String {
                    t.rows[r].cells[c].content.iter().filter_map(|cc| match cc {
                        CellContent::Paragraph(p) => Some(p.runs.iter().map(|r| r.text.clone()).collect::<String>()),
                        _ => None,
                    }).collect::<Vec<_>>().join("")
                };
                assert_eq!(text_of(0, 0), "A1");
                assert_eq!(text_of(1, 2), "C2");
                assert_eq!(text_of(0, 2), "");
            }
            other => panic!("expected Table content, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_multiple_shapes_per_slide() {
        let prs = make_presentation(vec![make_slide(vec![
            make_text_shape(1, "First"),
            make_text_shape(2, "Second"),
        ])]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        assert_eq!(parsed.slides[0].shapes.len(), 2);
    }

    #[test]
    fn test_roundtrip_empty_presentation() {
        let prs = make_presentation(vec![]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");
        assert_eq!(parsed.slides.len(), 0);
    }

    #[test]
    fn test_roundtrip_group_shape_basic() {
        // v183: 验证 Group 不再被 `_` 兜底分支静默丢弃子内容。
        let child1 = make_text_shape(10, "Child A");
        let child2 = make_shape(11, ShapeGeometry::Ellipse, ShapeContent::Empty);
        let mut group_shape = make_shape(
            1,
            ShapeGeometry::Rect, // Group 的 geometry 字段不使用，写出/读回都忽略
            ShapeContent::Group {
                children: vec![child1, child2],
                ch_off: Some((0.0, 0.0)),
                ch_ext: Some((200.0, 100.0)),
            },
        );
        group_shape.position = Transform::from_rect(50.0, 60.0, 200.0, 100.0);

        let prs = make_presentation(vec![make_slide(vec![group_shape])]);
        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let s = &parsed.slides[0].shapes[0];
        match &s.content {
            ShapeContent::Group { children, ch_off, ch_ext } => {
                assert_eq!(children.len(), 2, "分组子形状数量应保持为2，而非被丢弃成空文本框");
                let text: String = match &children[0].content {
                    ShapeContent::Text(paras) => paras.iter()
                        .flat_map(|p| p.runs.iter().map(|r| r.text.as_str()))
                        .collect(),
                    other => panic!("child[0] expected Text, got {:?}", other),
                };
                assert!(text.contains("Child A"), "got: {text}");
                assert!(matches!(children[1].geometry, ShapeGeometry::Ellipse));

                let (cx, cy) = ch_off.expect("ch_off should roundtrip");
                assert!((cx - 0.0).abs() < 0.01 && (cy - 0.0).abs() < 0.01);
                let (ex, ey) = ch_ext.expect("ch_ext should roundtrip");
                assert!((ex - 200.0).abs() < 0.01 && (ey - 100.0).abs() < 0.01);
            }
            other => panic!("expected Group content, got {:?}", other),
        }

        // 分组自身位置也应正确写出/读回（此前 Group 被当成普通 sp 写出时，
        // position 本身是对的，只有 children 丢失；这里一并确认没有回归）。
        assert!((s.position.x() - 50.0).abs() < 0.1);
        assert!((s.position.y() - 60.0).abs() < 0.1);
        assert!((s.position.w() - 200.0).abs() < 0.1);
        assert!((s.position.h() - 100.0).abs() < 0.1);
    }

    #[test]
    fn test_roundtrip_nested_group_shape() {
        // 嵌套 Group（Group 内还有 Group）应递归写出/读回，与 parser 侧
        // "嵌套分组天然生效"的设计对称验证。
        let inner_child = make_text_shape(20, "Nested");
        let inner_group = make_shape(
            2,
            ShapeGeometry::Rect,
            ShapeContent::Group { children: vec![inner_child], ch_off: None, ch_ext: None },
        );
        let outer_group = make_shape(
            1,
            ShapeGeometry::Rect,
            ShapeContent::Group { children: vec![inner_group], ch_off: None, ch_ext: None },
        );
        let prs = make_presentation(vec![make_slide(vec![outer_group])]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let outer = &parsed.slides[0].shapes[0];
        match &outer.content {
            ShapeContent::Group { children: outer_children, .. } => {
                assert_eq!(outer_children.len(), 1);
                match &outer_children[0].content {
                    ShapeContent::Group { children: inner_children, .. } => {
                        assert_eq!(inner_children.len(), 1);
                        let text: String = match &inner_children[0].content {
                            ShapeContent::Text(paras) => paras.iter()
                                .flat_map(|p| p.runs.iter().map(|r| r.text.as_str()))
                                .collect(),
                            other => panic!("expected Text, got {:?}", other),
                        };
                        assert!(text.contains("Nested"), "got: {text}");
                    }
                    other => panic!("expected inner Group, got {:?}", other),
                }
            }
            other => panic!("expected outer Group, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_group_with_image() {
        // Group 内含图片：验证嵌套场景下 media_counter/rid_counter 正确
        // 递增、图片数据完整写出/读回，不与外层图片冲突。
        let png_bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
            0xDE,
        ];
        let img_shape = make_shape(
            30,
            ShapeGeometry::Rect,
            ShapeContent::Image(roe_model::common::ImageData {
                data: png_bytes.clone(), mime: "image/png".to_string(), ..Default::default()
            }),
        );
        let group_shape = make_shape(
            1,
            ShapeGeometry::Rect,
            ShapeContent::Group { children: vec![img_shape], ch_off: None, ch_ext: None },
        );
        let prs = make_presentation(vec![make_slide(vec![group_shape])]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        match &parsed.slides[0].shapes[0].content {
            ShapeContent::Group { children, .. } => {
                match &children[0].content {
                    ShapeContent::Image(roe_model::common::ImageData { data, mime, .. }) => {
                        assert_eq!(data, &png_bytes);
                        assert_eq!(mime, "image/png");
                    }
                    other => panic!("expected Image, got {:?}", other),
                }
            }
            other => panic!("expected Group, got {:?}", other),
        }
    }

    #[test]
    fn test_roundtrip_rotation_and_flip() {
        // v183: 此前 rot/flipH/flipV 完全不写出，本测试确认修复生效。
        let mut shape = make_shape(1, ShapeGeometry::Rect, ShapeContent::Empty);
        shape.position.rotation = Some(45.0);
        shape.position.flip_h = Some(true);
        shape.position.flip_v = Some(true);
        let prs = make_presentation(vec![make_slide(vec![shape])]);

        let bytes = PptxWriter::write(&prs).expect("write failed");
        let parsed = PptxParser::parse(&bytes).expect("roundtrip parse failed");

        let s = &parsed.slides[0].shapes[0];
        assert!((s.position.rotation.expect("rotation should roundtrip") - 45.0).abs() < 0.01);
        assert_eq!(s.position.flip_h, Some(true));
        assert_eq!(s.position.flip_v, Some(true));
    }

    #[test]
    fn test_geometry_to_prst_mapping() {
        assert_eq!(geometry_to_prst(&ShapeGeometry::Rect), "rect");
        assert_eq!(geometry_to_prst(&ShapeGeometry::Ellipse), "ellipse");
        assert_eq!(geometry_to_prst(&ShapeGeometry::Triangle), "triangle");
        assert_eq!(geometry_to_prst(&ShapeGeometry::Line), "line");
        assert_eq!(
            geometry_to_prst(&ShapeGeometry::RoundRect { radius: 5.0 }),
            "roundRect"
        );
        assert_eq!(
            geometry_to_prst(&ShapeGeometry::Custom("star5".to_string())),
            "star5"
        );
    }

    // ─── v183: 真实文件 round-trip 测试 ─────────────────────────────────────
    //
    // 与上面所有 test_roundtrip_* 的本质区别：上面的测试全部用 make_shape/
    // make_presentation 手工构造"干净"的合成内存模型，只验证单一维度
    // （只有文字/只有一个fill/...），从未验证过"真实世界文件解析出的、
    // 多 shape 混合多种 geometry/fill/text 的复杂模型"过一遍
    // write→reparse 是否还能保持语义一致。roe-save-design.md §4.2 记录
    // 的关键缺口就是这类测试完全空白。
    //
    // 断言范围限定在 pptx_writer.rs 文件头注释声明的"写出的内容"以内
    // （slide_size / shape id・position・geometry(preset) / fill(Solid・
    // None) / stroke宽度 / text文字 / image），不做整体 struct 相等比较：
    // - animation_tree/transition/notes/master 真实内容：parser 恒定
    //   default，两次 parse 结果本来就一样，比较没有意义，不比较
    // - FillStyle::Gradient/Picture/PictureTiled：writer 退化成 noFill，
    //   比较会必然失败，跳过这类 shape 的 fill 断言（仍断言其余字段）
    // - stroke 颜色：parser 读回恒为黑色（parser 自身限制），只断言宽度
    // - Paragraph/Run 除 text 外字段：parser 不读，不比较
    //
    // 用"第一次 parse（真实文件）"作为 expected，"write→reparse"作为
    // actual，逐 shape 断言，而非对两个 Presentation 做 assert_eq!。

    /// 递归比较两组 shapes 在 writer 支持范围内的字段是否一致。
    /// `path` 用于失败时定位是哪个 slide/shape（含嵌套 Group 路径）。
    fn assert_shapes_match_within_writer_scope(expected: &[Shape], actual: &[Shape], path: &str) {
        assert_eq!(
            expected.len(), actual.len(),
            "{path}: shape 数量不一致（可能是某个 shape 类型被静默丢弃）"
        );
        for (i, (exp, act)) in expected.iter().zip(actual.iter()).enumerate() {
            let shape_path = format!("{path}[{i}] id={}", exp.id);

            // position：writer 全量支持，四个维度都应保持一致
            assert!(
                (exp.position.x() - act.position.x()).abs() < 0.5,
                "{shape_path}: x mismatch exp={} act={}", exp.position.x(), act.position.x()
            );
            assert!(
                (exp.position.y() - act.position.y()).abs() < 0.5,
                "{shape_path}: y mismatch exp={} act={}", exp.position.y(), act.position.y()
            );
            assert!(
                (exp.position.w() - act.position.w()).abs() < 0.5,
                "{shape_path}: w mismatch exp={} act={}", exp.position.w(), act.position.w()
            );
            assert!(
                (exp.position.h() - act.position.h()).abs() < 0.5,
                "{shape_path}: h mismatch exp={} act={}", exp.position.h(), act.position.h()
            );

            match (&exp.content, &act.content) {
                (ShapeContent::Text(exp_paras), ShapeContent::Text(act_paras)) => {
                    let exp_text: String = exp_paras.iter()
                        .flat_map(|p| p.runs.iter().map(|r| r.text.as_str())).collect();
                    let act_text: String = act_paras.iter()
                        .flat_map(|p| p.runs.iter().map(|r| r.text.as_str())).collect();
                    assert_eq!(exp_text, act_text, "{shape_path}: text mismatch");
                }
                (ShapeContent::Image(exp_img), ShapeContent::Image(act_img)) => {
                    assert_eq!(exp_img.data, act_img.data, "{shape_path}: image bytes mismatch");
                    assert_eq!(exp_img.mime, act_img.mime, "{shape_path}: image mime mismatch");
                }
                (
                    ShapeContent::Group { children: exp_c, ch_off: exp_off, ch_ext: exp_ext },
                    ShapeContent::Group { children: act_c, ch_off: act_off, ch_ext: act_ext },
                ) => {
                    assert_shapes_match_within_writer_scope(
                        exp_c, act_c, &format!("{shape_path}/group"),
                    );
                    match (exp_off, act_off) {
                        (Some((ex, ey)), Some((ax, ay))) => {
                            assert!((ex - ax).abs() < 0.5 && (ey - ay).abs() < 0.5,
                                "{shape_path}: ch_off mismatch exp=({ex},{ey}) act=({ax},{ay})");
                        }
                        (None, None) => {}
                        _ => panic!("{shape_path}: ch_off Some/None 不一致 exp={exp_off:?} act={act_off:?}"),
                    }
                    match (exp_ext, act_ext) {
                        (Some((ex, ey)), Some((ax, ay))) => {
                            assert!((ex - ax).abs() < 0.5 && (ey - ay).abs() < 0.5,
                                "{shape_path}: ch_ext mismatch exp=({ex},{ey}) act=({ax},{ay})");
                        }
                        (None, None) => {}
                        _ => panic!("{shape_path}: ch_ext Some/None 不一致 exp={exp_ext:?} act={act_ext:?}"),
                    }
                }
                // Table/Chart：writer 不支持，parser 侧此前就恒定解析成
                // 对应变体，写出后重新 parse 只能是 Empty（内容丢失是
                // 已知的、超出本轮任务范围的 writer 能力边界，见
                // roe-save-design.md §4.2"暂不实现"列表，不在此断言
                // 内容一致，只跳过，避免真实样本探测阶段已经过滤掉这类
                // 文件，这里理论上不会走到，加上防止未来样本池变化时
                // 误报"回归"）。
                (ShapeContent::Table(_), _) | (ShapeContent::Chart(_), _) => {}
                (ShapeContent::Empty, ShapeContent::Empty) => {}
                (exp_variant, act_variant) => {
                    panic!(
                        "{shape_path}: content variant mismatch exp={:?} act={:?}",
                        std::mem::discriminant(exp_variant), std::mem::discriminant(act_variant)
                    );
                }
            }

            // geometry：仅比较 preset 系列（Rect/Ellipse/Triangle/Line/
            // RoundRect/Custom），PresetPath 写出时退化成 "custom" 字符串
            // 写回，reparse 后不再是 PresetPath 变体，是已知的 writer
            // 能力边界（geometry_to_prst 里 PresetPath(_) => "custom"），
            // 不在此断言其内部路径数据一致。
            //
            // v183 技术债（见 roe-save-design.md §七）：pptx.rs parser
            // 对显式 prst="rect"/"roundRect"/"triangle" 会查176种预设
            // 公式表命中，解析成 PresetPath 而非专门枚举 Rect/RoundRect/
            // Triangle——这三个专门枚举只有 XML 完全省略 prstGeom 标签
            // 时才会产生。writer 写出 Rect 时用的是显式 prst="rect"，
            // 于是 reparse 后必然变成 PresetPath("rect")，与"省略标签"
            // 产生的 Rect 类型判别式不同，但两者渲染语义完全等价（都是
            // 矩形）。这不是 writer 引入的新问题，是既有 parser 分类
            // 现状，此处按语义等价处理，不视为 round-trip 不一致；
            // 专项修复时机见 roe-save-design.md §7.5，不在本轮任务内。
            fn preset_name_of(g: &ShapeGeometry) -> Option<&str> {
                match g {
                    ShapeGeometry::PresetPath(pd) => Some(pd.preset_name.as_str()),
                    _ => None,
                }
            }
            fn semantic_prst(g: &ShapeGeometry) -> String {
                // Rect/RoundRect/Triangle 与同名 PresetPath 视为同一
                // 语义类别；其余情况退回 geometry_to_prst 原始判别。
                match preset_name_of(g) {
                    Some(name) => name.to_string(),
                    None => geometry_to_prst(g),
                }
            }
            let geometry_equiv = |e: &ShapeGeometry, a: &ShapeGeometry| -> bool {
                semantic_prst(e) == semantic_prst(a)
            };
            match &exp.geometry {
                ShapeGeometry::PresetPath(_) => {}
                _ => {
                    assert!(
                        geometry_equiv(&exp.geometry, &act.geometry),
                        "{shape_path}: geometry semantic mismatch exp={:?} act={:?}",
                        geometry_to_prst(&exp.geometry), geometry_to_prst(&act.geometry)
                    );
                }
            }

            // fill：跳过 Gradient/Picture/PictureTiled（writer 已知退化为
            // noFill，不保真，断言会必然失败且不代表新增回归）
            let exp_fill_comparable = !matches!(
                exp.style.fill,
                Some(FillStyle::Gradient { .. }) | Some(FillStyle::Picture(_)) | Some(FillStyle::PictureTiled { .. })
            );
            if exp_fill_comparable {
                match (&exp.style.fill, &act.style.fill) {
                    (Some(FillStyle::Solid(ec)), Some(FillStyle::Solid(ac))) => {
                        assert_eq!(ec.r, ac.r, "{shape_path}: fill.r mismatch");
                        assert_eq!(ec.g, ac.g, "{shape_path}: fill.g mismatch");
                        assert_eq!(ec.b, ac.b, "{shape_path}: fill.b mismatch");
                    }
                    (Some(FillStyle::None), Some(FillStyle::None)) => {}
                    (None, None) => {}
                    // parser 对无 spPr/fill 的 shape 可能给 None 也可能给
                    // Some(FillStyle::None)，两者渲染语义等价（都是无填充），
                    // 不视为不一致。
                    (None, Some(FillStyle::None)) | (Some(FillStyle::None), None) => {}
                    (e, a) => panic!("{shape_path}: fill variant mismatch exp={e:?} act={a:?}"),
                }
            }

            // stroke：只比较宽度，颜色 parser 读回恒为黑色（见文件头注释），
            // 不比较颜色。
            match (&exp.style.stroke, &act.style.stroke) {
                (Some(es), Some(actual_s)) => {
                    assert!(
                        (es.width - actual_s.width).abs() < 0.05,
                        "{shape_path}: stroke width mismatch exp={} act={}", es.width, actual_s.width
                    );
                }
                (None, None) => {}
                // 同 fill，None vs 未产生任何差异的默认 stroke 均视为等价，
                // 只要不是"有 stroke 变没有"或反过来才报错。
                (Some(_), None) | (None, Some(_)) => {
                    panic!("{shape_path}: stroke Some/None 不一致 exp={:?} act={:?}",
                        exp.style.stroke.is_some(), act.style.stroke.is_some());
                }
            }
        }
    }

    /// 真实文件 round-trip 测试的公共驱动逻辑：读文件 → parse(expected)
    /// → write → parse(actual) → 逐 shape 比较。
    fn run_real_file_roundtrip(path: &str) {
        let data = std::fs::read(path)
            .unwrap_or_else(|e| panic!("读取样本文件失败 {path}: {e}"));
        let expected = PptxParser::parse(&data)
            .unwrap_or_else(|e| panic!("首次 parse 失败 {path}: {e:?}"));

        let bytes = PptxWriter::write(&expected)
            .unwrap_or_else(|e| panic!("write 失败 {path}: {e:?}"));
        let actual = PptxParser::parse(&bytes)
            .unwrap_or_else(|e| panic!("write→reparse 失败 {path}: {e:?}"));

        assert_eq!(
            expected.slides.len(), actual.slides.len(),
            "{path}: slide 数量不一致"
        );
        assert!(
            (expected.slide_size.0 - actual.slide_size.0).abs() < 0.5
                && (expected.slide_size.1 - actual.slide_size.1).abs() < 0.5,
            "{path}: slide_size mismatch exp={:?} act={:?}", expected.slide_size, actual.slide_size
        );
        for (si, (exp_slide, act_slide)) in expected.slides.iter().zip(actual.slides.iter()).enumerate() {
            assert_shapes_match_within_writer_scope(
                &exp_slide.shapes, &act_slide.shapes, &format!("{path} slide[{si}]"),
            );
        }
    }

    #[test]
    fn test_real_file_roundtrip_tdf111863() {
        // 探测脚本 scan_pptx_roundtrip_candidates 挑选：3 shapes / 1 slide，
        // 全部是 text+preset geometry+solid fill+stroke，无 group/table/
        // chart/gradient，是 writer 支持范围内最干净的中等复杂度真实样本，
        // 用作 round-trip 测试的主力基线。
        run_real_file_roundtrip(
            "/home/yong/Projects/roe/test_samples/curated/pptx/mid__sd_data_pptx__tdf111863.pptx",
        );
    }

    #[test]
    fn test_real_file_roundtrip_group_transform() {
        // 探测脚本挑选：含 1 个 Group（3 shapes/1 slide），专门覆盖本轮
        // 新修复的 Group 写出路径（v183 之前会静默丢弃 Group 子内容，
        // 此测试若回归会立即失败在 shape 数量不一致上）。
        run_real_file_roundtrip(
            "/home/yong/Projects/roe/test_samples/curated/pptx/small__oox_unit_data__tdf141463_GroupTransform.pptx",
        );
    }

    #[test]
    fn test_real_file_roundtrip_candy_light_large() {
        // 探测脚本挑选：45 shapes / 2 slides / 9 个 group，是"干净"
        // （无table/chart/gradient）样本里规模最大的一个，作为压力测试：
        // 验证大量 shape、多层 group 混合场景下 write→reparse 不会因为
        // 计数器错位（media_counter/rid_counter）或类似问题在大规模下
        // 才暴露的 bug。
        run_real_file_roundtrip(
            "/home/yong/Projects/roe/test_samples/curated/pptx/small__candy_light.pptx",
        );
    }

    #[test]
    fn test_mime_to_ext() {
        assert_eq!(mime_to_ext("image/png"), "png");
        assert_eq!(mime_to_ext("image/jpeg"), "jpeg");
        assert_eq!(mime_to_ext("image/gif"), "gif");
        assert_eq!(mime_to_ext("image/unknown"), "bin");
    }
}


#[cfg(test)]
mod body_pr_v219_tests {
    use super::*;
    use roe_model::impress::{TextAnchor, TextBodyProps, TextVert};

    #[test]
    fn body_pr_writes_vert_and_props() {
        let mut tb = TextBodyProps::default();
        assert_eq!(write_body_pr(&tb), "<a:bodyPr/>");
        tb.vert = TextVert::EaVert;
        tb.wrap = false;
        tb.anchor = TextAnchor::Middle;
        tb.ins_l = Some(10.0);
        let x = write_body_pr(&tb);
        assert!(x.contains(r#"vert="eaVert""#), "{x}");
        assert!(x.contains(r#"wrap="none""#), "{x}");
        assert!(x.contains(r#"anchor="ctr""#), "{x}");
        assert!(x.contains(r#"lIns="127000""#), "{x}");
        tb.vert = TextVert::Vert270;
        assert!(write_body_pr(&tb).contains(r#"vert="vert270""#));
    }
}


#[cfg(test)]
mod package_v219_tests {
    use super::*;
    #[test]
    fn package_has_theme_and_docprops() {
        let ct = write_content_types(1);
        assert!(ct.contains("/ppt/theme/theme1.xml") && ct.contains("/docProps/core.xml") && ct.contains("/docProps/app.xml"));
        assert!(write_presentation_rels(2).contains("theme/theme1.xml"));
        assert!(write_slide_master_rels().contains("theme1.xml"));
        assert!(write_presentation_xml(&Presentation {
            styles: Default::default(), slide_size: (960.0, 540.0), slides: vec![],
            master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default(),
        }).contains("notesSz"));
        assert!(write_slide_master_xml().contains("txStyles"));
        assert!(write_theme_xml().contains("fmtScheme"));
    }
}


// ─── v319a：文字/段落属性写出（此前只写了 <a:t>，保存后全部丢失）─────────────

fn write_rpr_xml_v319a(run: &roe_model::writer::Run) -> String {
    let mut attrs = String::new();
    if let Some(sz) = run.font_size {
        attrs.push_str(&format!(r#" sz="{}""#, (sz * 100.0).round() as i64));
    }
    if let Some(b) = run.bold {
        attrs.push_str(if b { r#" b="1""# } else { r#" b="0""# });
    }
    if let Some(i) = run.italic {
        attrs.push_str(if i { r#" i="1""# } else { r#" i="0""# });
    }
    if let Some(u) = run.underline {
        attrs.push_str(if u { r#" u="sng""# } else { r#" u="none""# });
    }
    if let Some(true) = run.strikethrough {
        attrs.push_str(r#" strike="sngStrike""#);
    }
    match run.script {
        Some(roe_model::writer::ScriptPosition::Superscript) => attrs.push_str(r#" baseline="30000""#),
        Some(roe_model::writer::ScriptPosition::Subscript) => attrs.push_str(r#" baseline="-25000""#),
        _ => {}
    }
    let mut children = String::new();
    if let Some(c) = run.color {
        children.push_str(&format!(
            r#"<a:solidFill><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:solidFill>"#,
            c.r, c.g, c.b
        ));
    }
    if let Some(c) = run.background {
        children.push_str(&format!(r#"<a:highlight><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:highlight>"#, c.r, c.g, c.b));
    }
    let fam = run.font_family.as_ref().or(run.font_family_ascii.as_ref());
    if let Some(f) = fam {
        if !f.is_empty() {
            children.push_str(&format!(r#"<a:latin typeface="{}"/>"#, xml_escape_text(f)));
        }
    }
    if attrs.is_empty() && children.is_empty() {
        String::new()
    } else if children.is_empty() {
        format!("<a:rPr{attrs}/>")
    } else {
        format!("<a:rPr{attrs}>{children}</a:rPr>")
    }
}

fn write_ppr_xml_v319a(p: &Paragraph) -> String {
    let mut attrs = String::new();
    if let Some(al) = p.alignment {
        let v = match al {
            roe_model::style::Alignment::Left => "l",
            roe_model::style::Alignment::Center => "ctr",
            roe_model::style::Alignment::Right => "r",
            roe_model::style::Alignment::Justify => "just",
        };
        attrs.push_str(&format!(r#" algn="{v}""#));
    }
    let mut children = String::new();
    if let Some(ls) = p.line_height {
        children.push_str(&format!(r#"<a:lnSpc><a:spcPct val="{}"/></a:lnSpc>"#, (ls * 100000.0).round() as i64));
    }
    if let Some(sb) = p.space_before {
        children.push_str(&format!(r#"<a:spcBef><a:spcPts val="{}"/></a:spcBef>"#, (sb * 100.0).round() as i64));
    }
    if let Some(sa) = p.space_after {
        children.push_str(&format!(r#"<a:spcAft><a:spcPts val="{}"/></a:spcAft>"#, (sa * 100.0).round() as i64));
    }
    if let Some(b) = &p.bullet {
        if let Some(m) = b.mar_l {
            attrs.push_str(&format!(r#" marL="{}""#, (m * 12700.0).round() as i64));
        }
        if let Some(i) = b.indent {
            attrs.push_str(&format!(r#" indent="{}""#, (i * 12700.0).round() as i64));
        }
        if let Some(c) = b.color {
            children.push_str(&format!(
                r#"<a:buClr><a:srgbClr val="{:02X}{:02X}{:02X}"/></a:buClr>"#,
                c.r, c.g, c.b
            ));
        }
        match &b.kind {
            roe_model::impress::BulletKind::None => children.push_str("<a:buNone/>"),
            roe_model::impress::BulletKind::Char { ch, font } => {
                if let Some(f) = font {
                    children.push_str(&format!(r#"<a:buFont typeface="{}"/>"#, xml_escape_text(f)));
                }
                children.push_str(&format!(r#"<a:buChar char="{}"/>"#, xml_escape_text(ch)));
            }
            roe_model::impress::BulletKind::AutoNum { scheme, start_at } => {
                match start_at {
                    Some(s) => children.push_str(&format!(
                        r#"<a:buAutoNum type="{}" startAt="{}"/>"#, xml_escape_text(scheme), s)),
                    None => children.push_str(&format!(
                        r#"<a:buAutoNum type="{}"/>"#, xml_escape_text(scheme))),
                }
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    if attrs.is_empty() && children.is_empty() {
        String::new()
    } else if children.is_empty() {
        format!("<a:pPr{attrs}/>")
    } else {
        format!("<a:pPr{attrs}>{children}</a:pPr>")
    }
}

#[cfg(test)]
mod write_para_tests_v319a {
    use super::*;

    #[test]
    fn writes_run_and_para_props() {
        let run = roe_model::writer::Run {
            text: "hi".into(),
            bold: Some(true),
            italic: Some(true),
            font_size: Some(24.0),
            color: Some(roe_model::common::Color { r: 255, g: 0, b: 0, a: 255 }),
            font_family: Some("Arial".into()),
            ..Default::default()
        };
        let mut para: Paragraph = Default::default();
        para.runs.push(run);
        para.alignment = Some(roe_model::style::Alignment::Center);
        para.bullet = Some(roe_model::impress::BulletInfo {
            kind: roe_model::impress::BulletKind::Char { ch: "•".into(), font: None },
            mar_l: Some(27.0),
            indent: Some(-27.0),
            color: None,
        });
        let xml = write_paragraph_xml(&para);
        assert!(xml.contains(r#"algn="ctr""#), "{xml}");
        assert!(xml.contains(r#"marL="342900""#), "{xml}");
        assert!(xml.contains(r#"<a:buChar char="•"/>"#), "{xml}");
        assert!(xml.contains(r#"sz="2400""#), "{xml}");
        assert!(xml.contains(r#"b="1""#) && xml.contains(r#"i="1""#), "{xml}");
        assert!(xml.contains(r#"val="FF0000""#), "{xml}");
        assert!(xml.contains(r#"typeface="Arial""#), "{xml}");
    }
}


// ---- v320c1: master / layout aware writing ----

fn fill_masters(prs: &Presentation) -> Vec<roe_model::impress::MasterSlide> {
    let mut v: Vec<roe_model::impress::MasterSlide> = prs.master_slides.clone();
    if v.is_empty() {
        v.push(roe_model::impress::MasterSlide {
            theme_xml: None,
            tx_styles_xml: None,
            layouts: Vec::new(),
            color_map: None,
            raw_ext: None,
            name: "Office Theme".to_string(),
            shapes: Vec::new(),
            background: None,
        });
    }
    for m in v.iter_mut() {
        if m.layouts.is_empty() {
            m.layouts.push(roe_model::impress::SlideLayout {
                name: "Blank".to_string(),
                kind: roe_model::impress::SlideLayoutKind::Blank,
                shapes: Vec::new(),
                background: None,
                show_master_shapes: true,
                color_map: None,
                raw_ext: None,
            });
        }
    }
    v
}

fn master_layout_ids(masters: &[roe_model::impress::MasterSlide]) -> (Vec<u32>, Vec<Vec<u32>>) {
    let mut c: u32 = 2147483648;
    let mut mids: Vec<u32> = Vec::new();
    let mut lids: Vec<Vec<u32>> = Vec::new();
    for m in masters {
        mids.push(c);
        c += 1;
        let mut row: Vec<u32> = Vec::new();
        for _ in &m.layouts {
            row.push(c);
            c += 1;
        }
        lids.push(row);
    }
    (mids, lids)
}

fn slide_layout_number(masters: &[roe_model::impress::MasterSlide], slide: &Slide) -> usize {
    let mi = match slide.master_idx {
        Some(i) if i < masters.len() => i,
        _ => 0,
    };
    let li = match slide.layout_idx {
        Some(i) if i < masters[mi].layouts.len() => i,
        _ => 0,
    };
    let before: usize = masters[..mi].iter().map(|m| m.layouts.len()).sum();
    before + li + 1
}

fn w320_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn w320_clr_attrs(cm: &std::collections::HashMap<String, String>) -> String {
    let mut s = String::new();
    for k in ["bg1", "tx1", "bg2", "tx2", "accent1", "accent2", "accent3", "accent4", "accent5", "accent6", "hlink", "folHlink"] {
        if let Some(v) = cm.get(k) {
            s.push_str(&format!(" {}=\"{}\"", k, w320_esc(v)));
        }
    }
    s
}

/// Reuse write_slide_xml for shapes and background, cut out the cSld element.
/// Placeholders are skipped in this step (written in a later step).
fn w320_csld_part(
    shapes: &[Shape],
    bg: &Option<FillStyle>,
    name: &str,
    default_bg: bool,
    media_counter: &mut u32,
) -> (String, Vec<(String, String)>, Vec<(String, Vec<u8>)>) {
    let tmp = Slide {
        layout_idx: None,
        shapes: shapes.to_vec(), // v320c2: placeholders written too
        master_idx: None,
        animation_tree: roe_model::impress::AnimationNode::default(),
        transition: None,
        notes: None,
        background: bg.clone(),
        layout_shapes: Vec::new(),
    };
    let (xml, rels, imgs) = write_slide_xml(&tmp, media_counter);
    let mut csld = match (xml.find("<p:cSld>"), xml.find("</p:cSld>")) {
        (Some(s), Some(e)) => xml[s..e + 9].to_string(),
        _ => "<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/></p:spTree></p:cSld>".to_string(),
    };
    if default_bg && !csld.contains("<p:bg>") {
        csld = csld.replacen("<p:cSld>", "<p:cSld><p:bg><p:bgRef idx=\"1001\"><a:schemeClr val=\"bg1\"/></p:bgRef></p:bg>", 1);
    }
    if !name.is_empty() {
        csld = csld.replacen("<p:cSld>", &format!("<p:cSld name=\"{}\">", w320_esc(name)), 1);
    }
    (csld, rels, imgs)
}

fn write_masters_and_layouts<W: Write + std::io::Seek>(
    zw: &mut zip::ZipWriter<W>,
    opts: zip::write::SimpleFileOptions,
    masters: &[roe_model::impress::MasterSlide],
    media_counter: &mut u32,
) -> Result<(), ParseError> {
    use roe_model::impress::SlideLayoutKind as K;
    let (_mids, lids) = master_layout_ids(masters);
    let ns = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let default_clr = r#"<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>"#;
    let mut k: usize = 0;
    for (mi, m) in masters.iter().enumerate() {
        let mnum = mi + 1;
        let mut master_rels_extra = String::new();
        let mut id_list = String::new();
        for (li, l) in m.layouts.iter().enumerate() {
            k += 1;
            let (csld, rels, imgs) = w320_csld_part(&l.shapes, &l.background, &l.name, false, media_counter);
            let mut attrs = String::new();
            let t = match l.kind {
                K::Title => "title",
                K::TitleAndContent => "obj",
                K::SectionHeader => "secHead",
                K::TwoContent => "twoObj",
                K::TitleOnly => "titleOnly",
                K::Blank => "blank",
                K::Custom => "",
            };
            if !t.is_empty() {
                attrs.push_str(&format!(" type=\"{}\"", t));
            }
            if !l.show_master_shapes {
                attrs.push_str(" showMasterSp=\"0\"");
            }
            let ovr = match &l.color_map {
                Some(cm) => format!("<p:clrMapOvr><a:overrideClrMapping{}/></p:clrMapOvr>", w320_clr_attrs(cm)),
                None => "<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>".to_string(),
            };
            let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sldLayout {ns}{attrs} preserve=\"1\">{csld}{ovr}</p:sldLayout>");
            let master_rel = format!("<Relationship Id=\"rIdM\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"../slideMasters/slideMaster{}.xml\"/></Relationships>", mnum);
            let lrels = write_slide_rels(&rels).replacen("</Relationships>", &master_rel, 1);
            write_zip_text(zw, opts, &format!("ppt/slideLayouts/slideLayout{}.xml", k), &xml)?;
            write_zip_text(zw, opts, &format!("ppt/slideLayouts/_rels/slideLayout{}.xml.rels", k), &lrels)?;
            for (path, data) in imgs {
                write_zip_bytes(zw, opts, &path, &data)?;
            }
            master_rels_extra.push_str(&format!("<Relationship Id=\"rIdL{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout\" Target=\"../slideLayouts/slideLayout{}.xml\"/>", li + 1, k));
            id_list.push_str(&format!("<p:sldLayoutId id=\"{}\" r:id=\"rIdL{}\"/>", lids[mi][li], li + 1));
        }

        let (mcsld, mrels, mimgs) = w320_csld_part(&m.shapes, &m.background, "", false, media_counter);
        let clrmap = match &m.color_map {
            Some(cm) if cm.len() >= 12 => format!("<p:clrMap{}/>", w320_clr_attrs(cm)),
            _ => default_clr.to_string(),
        };
        let tx = m.tx_styles_xml.clone().unwrap_or_else(|| "<p:txStyles><p:titleStyle/><p:bodyStyle/><p:otherStyle/></p:txStyles>".to_string());
        let mxml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<p:sldMaster {ns}>{mcsld}{clrmap}<p:sldLayoutIdLst>{id_list}</p:sldLayoutIdLst>{tx}</p:sldMaster>");
        let theme_rel = format!("<Relationship Id=\"rIdT\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"../theme/theme{}.xml\"/>", mnum);
        let mrels_xml = write_slide_rels(&mrels).replacen("</Relationships>", &format!("{master_rels_extra}{theme_rel}</Relationships>"), 1);
        let theme = m.theme_xml.clone().unwrap_or_else(write_theme_xml);
        write_zip_text(zw, opts, &format!("ppt/slideMasters/slideMaster{}.xml", mnum), &mxml)?;
        write_zip_text(zw, opts, &format!("ppt/slideMasters/_rels/slideMaster{}.xml.rels", mnum), &mrels_xml)?;
        write_zip_text(zw, opts, &format!("ppt/theme/theme{}.xml", mnum), &theme)?;
        for (path, data) in mimgs {
            write_zip_bytes(zw, opts, &path, &data)?;
        }
    }
    Ok(())
}

fn w320_content_types(slide_count: usize, masters: &[roe_model::impress::MasterSlide]) -> String {
    let mut ov = String::new();
    ov.push_str(r#"<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>"#);
    let mut k: usize = 0;
    for (mi, m) in masters.iter().enumerate() {
        ov.push_str(&format!(r#"<Override PartName="/ppt/slideMasters/slideMaster{}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>"#, mi + 1));
        ov.push_str(&format!(r#"<Override PartName="/ppt/theme/theme{}.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>"#, mi + 1));
        for _ in &m.layouts {
            k += 1;
            ov.push_str(&format!(r#"<Override PartName="/ppt/slideLayouts/slideLayout{}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>"#, k));
        }
    }
    ov.push_str(r#"<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>"#);
    ov.push_str(r#"<Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>"#);
    for i in 1..=slide_count {
        ov.push_str(&format!(r#"<Override PartName="/ppt/slides/slide{}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#, i));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Default Extension="png" ContentType="image/png"/>
<Default Extension="jpeg" ContentType="image/jpeg"/>
<Default Extension="jpg" ContentType="image/jpeg"/>
<Default Extension="gif" ContentType="image/gif"/>
{ov}
</Types>"#
    )
}

fn w320_presentation_xml(prs: &Presentation, masters: &[roe_model::impress::MasterSlide]) -> String {
    let cx = (prs.slide_size.0 * 12700.0).round() as i64;
    let cy = (prs.slide_size.1 * 12700.0).round() as i64;
    let (mids, _l) = master_layout_ids(masters);
    let mut ml = String::new();
    for (i, id) in mids.iter().enumerate() {
        ml.push_str(&format!("<p:sldMasterId id=\"{}\" r:id=\"rId{}\"/>", id, i + 1));
    }
    let mc = masters.len();
    let mut sl = String::new();
    for idx in 0..prs.slides.len() {
        sl.push_str(&format!("<p:sldId id=\"{}\" r:id=\"rId{}\"/>", 256 + idx as u32, mc + 1 + idx));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<p:sldMasterIdLst>{ml}</p:sldMasterIdLst>
<p:sldIdLst>{sl}</p:sldIdLst>
<p:sldSz cx="{cx}" cy="{cy}"/>
<p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#
    )
}

fn w320_presentation_rels(slide_count: usize, master_count: usize) -> String {
    let mut r = String::new();
    for i in 0..master_count {
        r.push_str(&format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster\" Target=\"slideMasters/slideMaster{}.xml\"/>", i + 1, i + 1));
    }
    for i in 0..slide_count {
        r.push_str(&format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide{}.xml\"/>", master_count + 1 + i, i + 1));
    }
    r.push_str(&format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme\" Target=\"theme/theme1.xml\"/>", master_count + slide_count + 1));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
{r}
</Relationships>"#
    )
}

#[cfg(test)]
mod tests_v320i_probe {
    use super::*;
    use roe_model::common::{Color, FillStyle, ImageData};
    use roe_model::impress::ShapeContent;

    const PNG: [u8; 67] = [
        0x89,0x50,0x4E,0x47,0x0D,0x0A,0x1A,0x0A,0x00,0x00,0x00,0x0D,0x49,0x48,0x44,0x52,
        0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x01,0x08,0x06,0x00,0x00,0x00,0x1F,0x15,0xC4,
        0x89,0x00,0x00,0x00,0x0D,0x49,0x44,0x41,0x54,0x78,0x9C,0x63,0x00,0x01,0x00,0x00,
        0x05,0x00,0x01,0x0D,0x0A,0x2D,0xB4,0x00,0x00,0x00,0x00,0x49,0x45,0x4E,0x44,0xAE,
        0x42,0x60,0x82,
    ];

    /// 手动运行：cargo test -p roe-parser --lib roundtrip_master_edit -- --ignored --nocapture
    #[test]
    #[ignore]
    fn roundtrip_master_edit() {
        let path = std::env::var("ROE_PLANT_PPTX")
            .unwrap_or_else(|_| "/home/yong/桌面/需求文档/plant.pptx".to_string());
        let data = std::fs::read(&path).expect("读取样本失败");
        let mut prs = crate::ooxml::pptx::PptxParser::parse(&data).expect("解析样本失败");

        let li = prs.master_slides[0].layouts.iter().position(|l| !l.shapes.is_empty())
            .expect("没有带形状的版式");
        let n_layouts = prs.master_slides[0].layouts.len();
        let n_master_shapes = prs.master_slides[0].shapes.len();
        let n_layout_shapes = prs.master_slides[0].layouts[li].shapes.len();
        println!("样本: {} 个版式, 母版形状 {}, 版式[{}] 形状 {}", n_layouts, n_master_shapes, li, n_layout_shapes);

        // 改动
        let red = Color { r: 220, g: 80, b: 80, a: 255 };
        let new_id;
        {
            let l = &mut prs.master_slides[0].layouts[li];
            l.background = Some(FillStyle::Solid(red));
            l.shapes[0].position.offset = (123.0, 45.0);
            let mut img = l.shapes[0].clone();
            new_id = l.shapes.iter().map(|s| s.id).max().unwrap_or(0) + 1;
            img.id = new_id;
            img.position = roe_model::common::Transform::from_rect(10.0, 10.0, 50.0, 50.0);
            img.content = ShapeContent::Image(ImageData {
                data: PNG.to_vec(), mime: "image/png".to_string(),
                crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
            });
            l.shapes.push(img);
        }
        let moved_master = if n_master_shapes > 0 {
            prs.master_slides[0].shapes[0].position.offset = (33.0, 44.0);
            true
        } else { false };

        let out = PptxWriter::write(&prs).expect("写出失败");
        std::fs::write("/tmp/v320i_probe.pptx", &out).ok();
        let back = crate::ooxml::pptx::PptxParser::parse(&out).expect("重新解析失败");

        let mut fails: Vec<String> = Vec::new();
        let mut check = |name: &str, ok: bool| {
            println!("[{}] {}", if ok { "PASS" } else { "FAIL" }, name);
            if !ok { fails.push(name.to_string()); }
        };
        check("母版数量不变", back.master_slides.len() == prs.master_slides.len());
        let bm = &back.master_slides[0];
        check("版式数量不变", bm.layouts.len() == n_layouts);
        let bl = &bm.layouts[li];
        check("版式形状数 +1", bl.shapes.len() == n_layout_shapes + 1);
        check("版式背景色保留", matches!(&bl.background, Some(FillStyle::Solid(c)) if c.r == 220 && c.g == 80 && c.b == 80));
        check("版式首个形状位置保留", (bl.shapes[0].position.x() - 123.0).abs() < 0.2 && (bl.shapes[0].position.y() - 45.0).abs() < 0.2);
        let img_ok = bl.shapes.iter().any(|s| matches!(&s.content, ShapeContent::Image(d) if d.data.len() == PNG.len()));
        check("版式里插入的图片保留", img_ok);
        if moved_master {
            check("母版首个形状位置保留", (bm.shapes[0].position.x() - 33.0).abs() < 0.2 && (bm.shapes[0].position.y() - 44.0).abs() < 0.2);
        }
        check("母版形状数不变", bm.shapes.len() == n_master_shapes);
        let _ = new_id;
        assert!(fails.is_empty(), "失败项: {:?}", fails);
    }
}

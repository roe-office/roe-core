//! Hybrid PDF 导出
//!
//! 规格：PDF/A-3b 兼容，正文为每页 JPEG 图像，附件为 ODF Flat XML（FODP/FODT）
//! LibreOffice / ROE 打开此 PDF 时优先读取附件，恢复完整内存模型。
//!
//! 实现分三步：
//!   1. 把内存模型序列化为 ODF Flat XML bytes（通过 roe-parser export）
//!   2. 用 tiny-skia 把每页渲染成 RGBA → 转 JPEG bytes
//!   3. 用 lopdf 组装 PDF：图像页 + EmbeddedFile 附件

use crate::ExportError;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

// ── 公开接口 ──────────────────────────────────────────────────────────────

pub struct PdfExporter;

impl PdfExporter {
    /// 从 Impress PresentationModel 导出 Hybrid PDF bytes
    pub fn export_presentation(
        model: &roe_model::impress::Presentation,
        odf_bytes: Vec<u8>,   // FODP XML bytes（调用方用 roe-parser 生成）
    ) -> Result<Vec<u8>, ExportError> {
        let pages = render_presentation_pages(model)?;
        assemble_pdf(pages, odf_bytes, "presentation.fodp")
    }

    /// 从 Writer DocumentModel 导出 Hybrid PDF bytes
    pub fn export_writer(
        model: &roe_model::writer::WriterDocument,
        odf_bytes: Vec<u8>,   // FODT XML bytes
    ) -> Result<Vec<u8>, ExportError> {
        let _ = model;
        // Writer tile 渲染在 Step 16 完成后接入，当前返回空白页占位
        let pages = vec![blank_page(595, 842)];
        assemble_pdf(pages, odf_bytes, "document.fodt")
    }
}

// ── 页面渲染 ──────────────────────────────────────────────────────────────

struct PageImage {
    width:  u32,
    height: u32,
    jpeg:   Vec<u8>,   // JPEG bytes
}

/// 把 Presentation 每张幻灯片渲染成 RGBA → JPEG
fn render_presentation_pages(
    model: &roe_model::impress::Presentation,
) -> Result<Vec<PageImage>, ExportError> {
    
    
    let (sw, sh) = model.slide_size;
    let width  = sw as u32;
    let height = sh as u32;

    let mut renderer = roe_impress::slide_renderer::SlideRenderer::new(sw, sh);
    let mut pages = Vec::new();
    for slide in &model.slides {
        let buf_size = (width * height * 4) as usize;
        let mut buf = vec![0u8; buf_size];
        renderer.render_slide(slide, &model.styles, None, 1.0, &mut buf, &model.theme_colors, Some(&model.master_slides))
            .map_err(ExportError::Pdf)?;
        let jpeg = rgba_to_jpeg(&buf, width, height)?;
        pages.push(PageImage { width, height, jpeg });
    }
    Ok(pages)
}

fn blank_page(width: u32, height: u32) -> PageImage {
    // 白色空白页 JPEG
    let rgba = vec![0xffu8; (width * height * 4) as usize];
    let jpeg = rgba_to_jpeg(&rgba, width, height).unwrap_or_default();
    PageImage { width, height, jpeg }
}

/// RGBA8 → JPEG bytes（用 tiny-skia Pixmap 的 encode_png 替代，
/// 当前用 PNG 格式存入 PDF XObject，后续可换真正的 JPEG 编码器）
fn rgba_to_jpeg(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ExportError> {
    // 用 PNG 代替 JPEG（lopdf 支持 PNG 内联图像流）
    // 直接把 RGBA 转成 RGB 用于 PDF Image XObject
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for chunk in rgba.chunks(4) {
        rgb.push(chunk[0]);
        rgb.push(chunk[1]);
        rgb.push(chunk[2]);
    }
    Ok(rgb)
}

// ── PDF 组装 ──────────────────────────────────────────────────────────────

/// 用 lopdf 组装 PDF：图像页 + EmbeddedFile 附件
fn assemble_pdf(
    pages: Vec<PageImage>,
    odf_bytes: Vec<u8>,
    attachment_name: &str,
) -> Result<Vec<u8>, ExportError> {
    let mut doc = Document::with_version("1.7");

    // Pages 根节点
    let pages_id = doc.new_object_id();

    // 每页
    let mut page_ids: Vec<Object> = Vec::new();
    for page in &pages {
        let page_id = add_image_page(&mut doc, pages_id, page)?;
        page_ids.push(Object::Reference(page_id));
    }

    // Pages 字典
    let pages_dict = Dictionary::from_iter(vec![
        ("Type",     Object::Name(b"Pages".to_vec())),
        ("Kids",     Object::Array(page_ids)),
        ("Count",    Object::Integer(pages.len() as i64)),
    ]);
    doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

    // Catalog
    let catalog_id = doc.add_object(Object::Dictionary(
        Dictionary::from_iter(vec![
            ("Type",  Object::Name(b"Catalog".to_vec())),
            ("Pages", Object::Reference(pages_id)),
        ])
    ));

    // 嵌入 ODF 附件
    embed_attachment(&mut doc, catalog_id, odf_bytes, attachment_name)?;

    doc.trailer.set("Root", Object::Reference(catalog_id));

    // 序列化到 bytes
    let mut buf = Vec::new();
    doc.save_to(&mut buf)
        .map_err(|e| ExportError::Pdf(e.to_string()))?;
    Ok(buf)
}

/// 把一张图像页加入 PDF，返回 Page ObjectId
fn add_image_page(
    doc: &mut Document,
    pages_id: ObjectId,
    page: &PageImage,
) -> Result<ObjectId, ExportError> {
    let w = page.width as i64;
    let h = page.height as i64;

    // Image XObject（Raw RGB，无压缩）
    let img_stream = Stream::new(
        Dictionary::from_iter(vec![
            ("Type",             Object::Name(b"XObject".to_vec())),
            ("Subtype",          Object::Name(b"Image".to_vec())),
            ("Width",            Object::Integer(w)),
            ("Height",           Object::Integer(h)),
            ("ColorSpace",       Object::Name(b"DeviceRGB".to_vec())),
            ("BitsPerComponent", Object::Integer(8)),
            ("Length",           Object::Integer(page.jpeg.len() as i64)),
        ]),
        page.jpeg.clone(),
    );
    let img_id = doc.add_object(img_stream);

    // 页面尺寸（pt）：1pt = 1/72 inch，幻灯片尺寸单位 px，假设 96dpi
    let pt_w = w as f64 * 72.0 / 96.0;
    let pt_h = h as f64 * 72.0 / 96.0;

    // 内容流：把图像铺满页面
    let content = format!(
        "q {pt_w:.2} 0 0 {pt_h:.2} 0 0 cm /Im0 Do Q\n"
    );
    let content_stream = Stream::new(
        Dictionary::from_iter(vec![
            ("Length", Object::Integer(content.len() as i64)),
        ]),
        content.into_bytes(),
    );
    let content_id = doc.add_object(content_stream);

    // Resources
    let resources = Dictionary::from_iter(vec![
        ("XObject", Object::Dictionary(
            Dictionary::from_iter(vec![
                ("Im0", Object::Reference(img_id)),
            ])
        )),
    ]);

    // Page 字典
    let page_dict = Dictionary::from_iter(vec![
        ("Type",      Object::Name(b"Page".to_vec())),
        ("Parent",    Object::Reference(pages_id)),
        ("MediaBox",  Object::Array(vec![
            Object::Integer(0), Object::Integer(0),
            Object::Real(pt_w as f32), Object::Real(pt_h as f32),
        ])),
        ("Contents",  Object::Reference(content_id)),
        ("Resources", Object::Dictionary(resources)),
    ]);
    Ok(doc.add_object(Object::Dictionary(page_dict)))
}

/// 把 ODF bytes 作为 EmbeddedFile 附件加入 PDF Catalog
fn embed_attachment(
    doc: &mut Document,
    catalog_id: ObjectId,
    data: Vec<u8>,
    name: &str,
) -> Result<(), ExportError> {
    let len = data.len() as i64;

    // EmbeddedFile stream
    let ef_stream = Stream::new(
        Dictionary::from_iter(vec![
            ("Type",    Object::Name(b"EmbeddedFile".to_vec())),
            ("Subtype", Object::Name(b"application/vnd.oasis.opendocument.presentation".to_vec())),
            ("Length",  Object::Integer(len)),
        ]),
        data,
    );
    let ef_id = doc.add_object(ef_stream);

    // Filespec
    let name_bytes = name.as_bytes().to_vec();
    let filespec = Dictionary::from_iter(vec![
        ("Type", Object::Name(b"Filespec".to_vec())),
        ("F",    Object::String(name_bytes.clone(), lopdf::StringFormat::Literal)),
        ("UF",   Object::String(name_bytes.clone(), lopdf::StringFormat::Literal)),
        ("EF",   Object::Dictionary(
            Dictionary::from_iter(vec![
                ("F", Object::Reference(ef_id)),
            ])
        )),
        ("AFRelationship", Object::Name(b"Source".to_vec())),
    ]);
    let filespec_id = doc.add_object(Object::Dictionary(filespec));

    // Names tree → EmbeddedFiles
    let names_dict = Dictionary::from_iter(vec![
        ("EmbeddedFiles", Object::Dictionary(
            Dictionary::from_iter(vec![
                ("Names", Object::Array(vec![
                    Object::String(name_bytes, lopdf::StringFormat::Literal),
                    Object::Reference(filespec_id),
                ])),
            ])
        )),
    ]);
    let names_id = doc.add_object(Object::Dictionary(names_dict));

    // 更新 Catalog
    if let Ok(catalog) = doc.get_dictionary_mut(catalog_id) {
        catalog.set("Names", Object::Reference(names_id));
    }

    Ok(())
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::impress::*;
    use roe_model::common::{Transform, StyleRef};

    fn make_presentation() -> Presentation {
        Presentation {
            slide_size: (960.0, 540.0),
            styles: roe_model::style::StyleRegistry::default(),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![Shape {
                    id: 1,
                    geometry: ShapeGeometry::Rect,
                    position: Transform::from_rect(100.0, 80.0, 760.0, 120.0),
                    style: ShapeStyle::default(),
                    placeholder: None,
                    content: ShapeContent::Text(vec![
                        roe_model::writer::Paragraph {
                            style: StyleRef("Title".into()),
                            runs: vec![roe_model::writer::Run { text: "ROE PDF Test".into(), char_style: None, ..Default::default() }],
                            list_info: None,
                            ..Default::default()
                        }
                    ]),
                    text_body: Default::default(),
                    st_cxn: None,
                    end_cxn: None,
                }],
                background: Default::default(),
                layout_shapes: vec![],
                master_idx: None,
                animation_tree: AnimationNode::TimingRoot { children: vec![] },
                notes: None,
                transition: None,
            }],
            master_slides: vec![],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        }
    }

    #[test]
    fn test_pdf_export_not_empty() {
        let model = make_presentation();
        let odf_bytes = b"<?xml version=\"1.0\"?><office:document/>".to_vec();
        let pdf = PdfExporter::export_presentation(&model, odf_bytes).unwrap();
        assert!(!pdf.is_empty());
        // PDF 魔数
        assert!(pdf.starts_with(b"%PDF-"));
    }

    #[test]
    fn test_save_pdf_to_tmp() {
        let model = make_presentation();
        let odf_bytes = b"<?xml version=\"1.0\"?><office:document/>".to_vec();
        let pdf = PdfExporter::export_presentation(&model, odf_bytes).unwrap();
        std::fs::write("/tmp/roe_test.pdf", &pdf).unwrap();
        println!("saved to /tmp/roe_test.pdf ({} bytes)", pdf.len());
    }
}

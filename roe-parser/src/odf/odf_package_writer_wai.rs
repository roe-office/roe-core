//! ODF Package 写出器（Writer 侧）：WriterDocument → 标准 .odt（zip）或 KAI .wai（zstd）
//!
//! 架构与 odf_package_writer.rs（Calc 侧 OdfPackageWriter）完全对称，唯一
//! 区别是内存模型换成 WriterDocument、Flat XML 生成换成 FodtWriter、
//! mimetype 换成 opendocument.text。
//!
//! 设计要点（与 Calc 侧一致，不重复展开，详见 odf_package_writer.rs 文件头）：
//! - content.xml 和 styles.xml 内容对 zip/zstd 两条路径完全一致，只有最后
//!   打包方式不同
//! - styles.xml 本轮同样是标准空壳：FodtWriter::write 产出的 Flat XML 里
//!   已经内嵌了完整的 office:automatic-styles（含 named styles，见
//!   fodt_writer.rs 文件头"paragraph_styles 是真实样式定义"），这些内容
//!   保留在 content.xml 里，不需要额外从 styles.xml 拆出来再注入回去
//!   （OdfPackageParser 读 styles.xml 时是"抠节点注入进 content.xml"，
//!   空壳注入空内容等于 no-op，不影响已内嵌在 content.xml 里的样式）
//! - 本轮阶段一验收标准很低：生成的包能被 LibreOffice 或 ROE 自己的
//!   parser 正常打开即可，不要求 round-trip 语义完全一致（那是阶段三的
//!   任务）
//! - WriterDocument 没有类似 Calc Workbook.floating_images 的独立图片
//!   集合字段——图片内嵌在 body 树里（BodyElement::Image(InlineImage)），
//!   FodtWriter::write 内部已处理，本文件不需要额外遍历/特殊处理图片

use roe_model::writer::WriterDocument;
use crate::ParseError;
use crate::odf::FodtWriter;

pub struct OdfPackageWriterWai;

impl OdfPackageWriterWai {
    /// 写出为标准 ODF Package（.odt），zip 格式，可被 LibreOffice 等
    /// 外部软件直接打开。
    pub fn write_odt(doc: &WriterDocument) -> Result<Vec<u8>, ParseError> {
        let (content_xml, styles_xml) = build_parts(doc);
        pack_zip(&content_xml, &styles_xml, "application/vnd.oasis.opendocument.text")
    }

    /// 写出为 KAI Wai（.wai），内容与 write_odt 完全一致，仅打包方式换成
    /// zstd（不叠加 zip，与 Calc 侧 write_sai 同一约定）。
    pub fn write_wai(doc: &WriterDocument) -> Result<Vec<u8>, ParseError> {
        let (content_xml, styles_xml) = build_parts(doc);
        pack_zstd(&content_xml, &styles_xml, "application/vnd.oasis.opendocument.text")
    }
}

/// FodtWriter 产出单一 Flat XML（office:document 根节点），拆分成 Package
/// 用的 content.xml（office:document-content 根节点）+ styles.xml
/// （office:document-styles 根节点，本轮为空壳）。做法与 Calc 侧
/// odf_package_writer.rs::build_parts 对称。
fn build_parts(doc: &WriterDocument) -> (String, String) {
    let flat_xml = FodtWriter::write(doc);

    let content_xml = flat_xml
        .replacen("<office:document\n", "<office:document-content\n", 1)
        .replacen("</office:document>", "</office:document-content>", 1);

    let styles_xml = build_empty_styles_xml();

    (content_xml, styles_xml)
}

fn build_empty_styles_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-styles
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:version="1.3">
  <office:styles/>
  <office:automatic-styles/>
  <office:master-styles/>
</office:document-styles>
"#.to_string()
}

fn build_manifest_xml() -> String {
    let mut out = String::new();
    out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    out.push('\n');
    out.push_str(r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">"#);
    out.push('\n');
    out.push_str(r#"  <manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="application/vnd.oasis.opendocument.text"/>"#);
    out.push('\n');
    out.push_str(r#"  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>"#);
    out.push('\n');
    out.push_str(r#"  <manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>"#);
    out.push('\n');
    out.push_str("</manifest:manifest>\n");
    out
}

fn pack_zip(content_xml: &str, styles_xml: &str, mimetype: &str) -> Result<Vec<u8>, ParseError> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let buf: Vec<u8> = Vec::new();
    let cursor = std::io::Cursor::new(buf);
    let mut zw = zip::ZipWriter::new(cursor);

    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zw.start_file("mimetype", stored)
        .map_err(|e| ParseError::Zip(e.to_string()))?;
    zw.write_all(mimetype.as_bytes()).map_err(ParseError::Io)?;

    let deflated = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zw.start_file("META-INF/manifest.xml", deflated)
        .map_err(|e| ParseError::Zip(e.to_string()))?;
    zw.write_all(build_manifest_xml().as_bytes()).map_err(ParseError::Io)?;

    zw.start_file("content.xml", deflated)
        .map_err(|e| ParseError::Zip(e.to_string()))?;
    zw.write_all(content_xml.as_bytes()).map_err(ParseError::Io)?;

    zw.start_file("styles.xml", deflated)
        .map_err(|e| ParseError::Zip(e.to_string()))?;
    zw.write_all(styles_xml.as_bytes()).map_err(ParseError::Io)?;

    let cursor = zw.finish().map_err(|e| ParseError::Zip(e.to_string()))?;
    Ok(cursor.into_inner())
}

/// KAI 路径：v188 会话定案，扩展成 4-part 拼接（mimetype + manifest.xml +
/// content.xml + styles.xml），对齐 pack_zip 覆盖的 part 清单，不引入
/// zip 容器，只整体走一次 zstd 压缩。详见 Calc 侧 odf_package_writer.rs
/// 同名函数的完整说明（不重复展开）。
/// 帧格式：4个 u32(LE) 长度前缀（顺序 mimetype/manifest/content/styles）
/// + 依次拼接的 4 段字节内容。
fn pack_zstd(content_xml: &str, styles_xml: &str, mimetype: &str) -> Result<Vec<u8>, ParseError> {
    let manifest_xml = build_manifest_xml();

    let mime_bytes = mimetype.as_bytes();
    let manifest_bytes = manifest_xml.as_bytes();
    let content_bytes = content_xml.as_bytes();
    let styles_bytes = styles_xml.as_bytes();

    let mut combined = Vec::with_capacity(
        16 + mime_bytes.len() + manifest_bytes.len() + content_bytes.len() + styles_bytes.len(),
    );
    combined.extend_from_slice(&(mime_bytes.len() as u32).to_le_bytes());
    combined.extend_from_slice(&(manifest_bytes.len() as u32).to_le_bytes());
    combined.extend_from_slice(&(content_bytes.len() as u32).to_le_bytes());
    combined.extend_from_slice(&(styles_bytes.len() as u32).to_le_bytes());
    combined.extend_from_slice(mime_bytes);
    combined.extend_from_slice(manifest_bytes);
    combined.extend_from_slice(content_bytes);
    combined.extend_from_slice(styles_bytes);

    crate::kai::KaiCodec::compress(&combined)
}

/// 与 pack_zstd 对称的解包：从 KAI Wai 字节还原 (mime, manifest_xml,
/// content_xml, styles_xml) 四段。供 roundtrip 测试和 lib.rs::
/// open_kai_bytes 使用。
pub fn unpack_zstd_wai(data: &[u8]) -> Result<(String, String, String, String), ParseError> {
    let combined = crate::kai::KaiCodec::decompress(data)?;
    if combined.len() < 16 {
        return Err(ParseError::Malformed("KAI wai package too short".to_string()));
    }
    let mime_len = u32::from_le_bytes([combined[0], combined[1], combined[2], combined[3]]) as usize;
    let manifest_len = u32::from_le_bytes([combined[4], combined[5], combined[6], combined[7]]) as usize;
    let content_len = u32::from_le_bytes([combined[8], combined[9], combined[10], combined[11]]) as usize;
    let styles_len = u32::from_le_bytes([combined[12], combined[13], combined[14], combined[15]]) as usize;

    let expected_total = 16usize
        .checked_add(mime_len)
        .and_then(|v| v.checked_add(manifest_len))
        .and_then(|v| v.checked_add(content_len))
        .and_then(|v| v.checked_add(styles_len))
        .ok_or_else(|| ParseError::Malformed("KAI wai package length overflow".to_string()))?;
    if combined.len() != expected_total {
        return Err(ParseError::Malformed("KAI wai package length mismatch".to_string()));
    }

    let mut offset = 16usize;
    let mime = String::from_utf8(combined[offset..offset + mime_len].to_vec())
        .map_err(|e| ParseError::Malformed(format!("mimetype not valid UTF-8: {e}")))?;
    offset += mime_len;
    let manifest_xml = String::from_utf8(combined[offset..offset + manifest_len].to_vec())
        .map_err(|e| ParseError::Malformed(format!("manifest.xml not valid UTF-8: {e}")))?;
    offset += manifest_len;
    let content_xml = String::from_utf8(combined[offset..offset + content_len].to_vec())
        .map_err(|e| ParseError::Malformed(format!("content.xml not valid UTF-8: {e}")))?;
    offset += content_len;
    let styles_xml = String::from_utf8(combined[offset..offset + styles_len].to_vec())
        .map_err(|e| ParseError::Malformed(format!("styles.xml not valid UTF-8: {e}")))?;

    Ok((mime, manifest_xml, content_xml, styles_xml))
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::writer::{BodyElement, Paragraph, Run};
    use roe_model::common::StyleRef;

    fn make_doc_with_text(text: &str) -> WriterDocument {
        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: text.to_string(), ..Default::default() }],
            ..Default::default()
        }));
        doc
    }

    #[test]
    fn test_write_odt_is_valid_zip_and_roundtrips() {
        let doc = make_doc_with_text("ODF Package Writer 测试");
        let bytes = OdfPackageWriterWai::write_odt(&doc).expect("write_odt failed");
        assert!(!bytes.is_empty());

        let parsed = crate::odf::OdfPackageParser::parse(&bytes).expect("parse back failed");
        match parsed {
            crate::RoeDocument::Writer(doc2) => {
                assert!(!doc2.body.is_empty(), "body should not be empty after roundtrip");
            }
            other => panic!("expected Writer, got {:?}", other),
        }
    }

    #[test]
    fn test_write_odt_mimetype_is_first_entry_and_stored() {
        let doc = make_doc_with_text("mimetype测试");
        let bytes = OdfPackageWriterWai::write_odt(&doc).expect("write_odt failed");

        let cursor = std::io::Cursor::new(&bytes[..]);
        let mut zip = zip::ZipArchive::new(cursor).expect("not a valid zip");
        let first_name = zip.name_for_index(0).expect("no entries").to_string();
        assert_eq!(first_name, "mimetype", "mimetype must be the first zip entry per ODF spec");

        let mut entry = zip.by_name("mimetype").unwrap();
        assert_eq!(entry.compression(), zip::CompressionMethod::Stored, "mimetype must be uncompressed");

        use std::io::Read;
        let mut mime_content = String::new();
        entry.read_to_string(&mut mime_content).unwrap();
        assert_eq!(mime_content, "application/vnd.oasis.opendocument.text");
    }

    #[test]
    fn test_write_wai_roundtrips() {
        let doc = make_doc_with_text("KAI wai测试");
        let bytes = OdfPackageWriterWai::write_wai(&doc).expect("write_wai failed");
        assert!(!bytes.is_empty());

        let (mime, manifest_xml, content_xml, styles_xml) = unpack_zstd_wai(&bytes).expect("unpack failed");
        assert_eq!(mime, "application/vnd.oasis.opendocument.text");
        assert!(manifest_xml.contains("manifest:manifest"));
        assert!(content_xml.contains("KAI wai测试"));
        assert!(styles_xml.contains("office:document-styles"));
    }
}

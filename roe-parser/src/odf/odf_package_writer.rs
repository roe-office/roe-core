//! ODF Package 写出器：Workbook → 标准 .ods（zip）或 KAI .sai（zstd）
//!
//! 与 OdfPackageParser（package.rs）对称，但方向相反：
//! 内存模型 → FodsWriter 生成的 Flat XML → 拆分 content.xml/styles.xml
//! → 打包。
//!
//! 设计要点：
//! - content.xml 和 styles.xml 内容完全一致（无论走 zip 还是走 zstd）；
//!   唯一区别是最后一步打包方式。KAI 系列（.sai/.wai/.pai）和标准 ODF
//!   （.ods/.odt/.odp）除了打包方式，其余（XML 生成/拆分/manifest/assets）
//!   完全复用，不重复实现（用户明确要求：不在 zip 外面再叠一层 zstd，
//!   两条路径各走各的）。
//! - v188 会话：pack_zstd/unpack_zstd_sai 从"content.xml+styles.xml 两段
//!   拼接"扩展为"mimetype+manifest.xml+content.xml+styles.xml 四段拼接"，
//!   与 pack_zip 覆盖的 part 清单完全对齐，不再是范围不完整的简化格式。
//!   读侧 open_kai_bytes（roe-parser/src/lib.rs）解出四段后复用
//!   package.rs::parse_parts，与 OdfPackageParser::parse（zip 路径）走
//!   同一套"root节点转换+styles注入+mimetype分发"逻辑，不维护两份。
//! - styles.xml 本轮是标准空壳：Calc 侧 workbook.styles.cell_styles 目前
//!   是从未被解析器填充、也从未被任何写出器消费的死字段（2026-09-02
//!   核实），没有真实命名样式内容可拆，故 styles.xml 只包含合规的空
//!   office:styles/office:automatic-styles/office:master-styles 节点，
//!   为未来 Calc 命名样式功能预留位置。
//! - floating_images：写出为 draw:frame > draw:image > office:binary-data
//!   base64 内嵌模式，与现有 fods.rs 解析器（parse_frame_image/
//!   parse_shapes_charts）保持对齐。这不是 ODF Package 规范里更常见的
//!   xlink:href 外部文件引用模式——那需要同步修改解析器才能 roundtrip，
//!   是下一轮"真正 assets 落地"任务的范围，本轮不做。
//! - floating_charts：本轮不处理，留待后续任务（ODF chart 写出需要完整
//!   内嵌 draw:object 子文档，复杂度显著高于图片/形状）。

use roe_model::calc::Workbook;
use crate::ParseError;
use crate::odf::FodsWriter;

pub struct OdfPackageWriter;

impl OdfPackageWriter {
    /// 写出为标准 ODF Package（.ods），zip 格式，可被 LibreOffice 等
    /// 外部软件直接打开。
    pub fn write_ods(workbook: &Workbook) -> Result<Vec<u8>, ParseError> {
        let (content_xml, styles_xml) = build_parts(workbook);
        pack_zip(&content_xml, &styles_xml, "application/vnd.oasis.opendocument.spreadsheet")
    }

    /// 写出为 KAI Sai（.sai），内容与 write_ods 完全一致，仅打包方式换成
    /// zstd（不叠加 zip，避免"两边不讨好"——见文件头注释）。
    pub fn write_sai(workbook: &Workbook) -> Result<Vec<u8>, ParseError> {
        let (content_xml, styles_xml) = build_parts(workbook);
        pack_zstd(&content_xml, &styles_xml, "application/vnd.oasis.opendocument.spreadsheet")
    }
}

/// FodsWriter 产出的是单一 Flat XML（office:document 根节点，body 和
/// automatic-styles 都在一起）。拆分成 Package 用的 content.xml
/// （office:document-content 根节点）+ styles.xml（office:document-styles
/// 根节点，本轮为空壳）。
fn build_parts(workbook: &Workbook) -> (String, String) {
    let flat_xml = FodsWriter::write(workbook);

    // Flat XML 根节点是 office:document，Package content.xml 根节点是
    // office:document-content；body 结构（office:automatic-styles +
    // office:body）两者相同，直接替换根节点名即可（与 package.rs 解析
    // 方向的做法对称：那边是 document-content -> document，这里反过来）。
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
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
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
    out.push_str(r#"  <manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>"#);
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

    // mimetype：ODF 规范要求必须是 zip 的第一个 entry，且无压缩（Stored）。
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

/// KAI 路径：v188 会话定案，不再是"content.xml+styles.xml 两段拼接"的
/// 简化格式（该格式漏掉了 mimetype/manifest.xml 两个 ODF 包必要元信息，
/// 且与 pack_zip 覆盖的 part 清单不对称，详见 roe-ext_progress_v188.md
/// §三）。现在拼接 pack_zip 打包的全部 4 个 part（mimetype +
/// META-INF/manifest.xml + content.xml + styles.xml），不引入 zip 容器，
/// 只是拼接后整体走一次 zstd 压缩——这是"zstd 替代 zip 作为打包容器"的
/// 正确实现，而非"zstd 叠加在 zip 之上"。
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

/// 与 pack_zstd 对称的解包：从 KAI Sai 字节还原 (mime, manifest_xml,
/// content_xml, styles_xml) 四段。供 roundtrip 测试和 lib.rs::
/// open_kai_bytes 使用（后者拿到这四段后转交 package::parse_parts 复用
/// zip 路径同一套转换逻辑）。
pub fn unpack_zstd_sai(data: &[u8]) -> Result<(String, String, String, String), ParseError> {
    let combined = crate::kai::KaiCodec::decompress(data)?;
    if combined.len() < 16 {
        return Err(ParseError::Malformed("KAI sai package too short".to_string()));
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
        .ok_or_else(|| ParseError::Malformed("KAI sai package length overflow".to_string()))?;
    if combined.len() != expected_total {
        return Err(ParseError::Malformed("KAI sai package length mismatch".to_string()));
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
    use roe_model::calc::{CellBlock, CellValue, Sheet, SparseColumn, SparseGrid};
    use std::collections::HashMap;

    fn make_wb_with_text(text: &str) -> Workbook {
        let mut grid = SparseGrid::default();
        grid.cols.insert(0, SparseColumn {
            blocks: vec![CellBlock {
                start_row: 0,
                values: vec![CellValue::Text(text.to_string())].into(),
            }].into(),
        });
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Sheet1".into(),
                cells: grid,
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats: HashMap::new(),
                cond_formats: vec![],
                data_validations: vec![],
                auto_filter: None,
                merged_cells: vec![],
                pivot: None,
                ..Default::default()
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn test_write_ods_is_valid_zip_and_roundtrips() {
        let wb = make_wb_with_text("ODF Package 测试");
        let bytes = OdfPackageWriter::write_ods(&wb).expect("write_ods failed");
        assert!(!bytes.is_empty());

        // 必须是合法 zip，且能被现有 OdfPackageParser 读回
        let doc = crate::odf::OdfPackageParser::parse(&bytes).expect("parse back failed");
        match doc {
            crate::RoeDocument::Calc(wb2) => {
                assert_eq!(wb2.sheets.len(), 1);
            }
            other => panic!("expected Calc, got {:?}", other),
        }
    }

    #[test]
    fn test_write_ods_mimetype_is_first_entry_and_stored() {
        let wb = make_wb_with_text("mimetype测试");
        let bytes = OdfPackageWriter::write_ods(&wb).expect("write_ods failed");

        let cursor = std::io::Cursor::new(&bytes[..]);
        let mut zip = zip::ZipArchive::new(cursor).expect("not a valid zip");
        let first_name = zip.name_for_index(0).expect("no entries").to_string();
        assert_eq!(first_name, "mimetype", "mimetype must be the first zip entry per ODF spec");

        let mut entry = zip.by_name("mimetype").unwrap();
        assert_eq!(entry.compression(), zip::CompressionMethod::Stored, "mimetype must be uncompressed");

        use std::io::Read;
        let mut mime_content = String::new();
        entry.read_to_string(&mut mime_content).unwrap();
        assert_eq!(mime_content, "application/vnd.oasis.opendocument.spreadsheet");
    }

    #[test]
    fn test_write_sai_roundtrips() {
        let wb = make_wb_with_text("KAI sai测试");
        let bytes = OdfPackageWriter::write_sai(&wb).expect("write_sai failed");
        assert!(!bytes.is_empty());

        let (mime, manifest_xml, content_xml, styles_xml) = unpack_zstd_sai(&bytes).expect("unpack failed");
        assert_eq!(mime, "application/vnd.oasis.opendocument.spreadsheet");
        assert!(manifest_xml.contains("manifest:manifest"));
        assert!(content_xml.contains("KAI sai测试"));
        assert!(styles_xml.contains("office:document-styles"));
    }

    #[test]
    fn test_floating_image_written_and_read_back() {
        let mut wb = make_wb_with_text("图片测试");
        wb.sheets[0].floating_images.push(roe_model::calc::FloatingImage {
            id: 0,
            data_base64: "aGVsbG8=".to_string(), // "hello"
            mime: "image/png".to_string(),
            width_pt: 100.0,
            height_pt: 50.0,
            x_pt: 10.0,
            y_pt: 20.0,
            rot: 0.0,
            hidden: false,
        });

        let bytes = OdfPackageWriter::write_ods(&wb).expect("write_ods failed");
        let doc = crate::odf::OdfPackageParser::parse(&bytes).expect("parse back failed");
        match doc {
            crate::RoeDocument::Calc(wb2) => {
                assert_eq!(wb2.sheets[0].floating_images.len(), 1, "floating image should roundtrip");
                assert_eq!(wb2.sheets[0].floating_images[0].data_base64, "aGVsbG8=");
            }
            other => panic!("expected Calc, got {:?}", other),
        }
    }

    #[test]
    fn test_hidden_floating_image_not_written() {
        let mut wb = make_wb_with_text("隐藏图片测试");
        wb.sheets[0].floating_images.push(roe_model::calc::FloatingImage {
            id: 0,
            data_base64: "aGVsbG8=".to_string(),
            mime: "image/png".to_string(),
            width_pt: 100.0,
            height_pt: 50.0,
            x_pt: 0.0,
            y_pt: 0.0,
            rot: 0.0,
            hidden: true,
        });

        let bytes = OdfPackageWriter::write_ods(&wb).expect("write_ods failed");
        let doc = crate::odf::OdfPackageParser::parse(&bytes).expect("parse back failed");
        match doc {
            crate::RoeDocument::Calc(wb2) => {
                assert_eq!(wb2.sheets[0].floating_images.len(), 0, "hidden image should not be written");
            }
            other => panic!("expected Calc, got {:?}", other),
        }
    }
}


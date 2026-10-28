//! ODF Package 支持：.odt / .ods / .odp
//!
//! ODF Package = ZIP，content.xml 内容与 FODT/FODS/FODP body 部分相同。
//! 策略：解 ZIP → 提取 content.xml → 喂给现有 Flat 解析器。
//!
//! mimetype 文件（无压缩，第一个 entry）决定文档类型：
//!   application/vnd.oasis.opendocument.text         → Writer
//!   application/vnd.oasis.opendocument.spreadsheet  → Calc
//!   application/vnd.oasis.opendocument.presentation → Impress

use std::io::Read;
use zip::ZipArchive;

use crate::odf::{FodpParser, FodsParser, FodtParser};
use crate::{ParseError, RoeDocument};

pub struct OdfPackageParser;

impl OdfPackageParser {
    pub fn parse(data: &[u8]) -> Result<RoeDocument, ParseError> {
        let cursor = std::io::Cursor::new(data);
        let mut zip = ZipArchive::new(cursor)
            .map_err(|e| ParseError::Zip(e.to_string()))?;

        // 1. 读 mimetype（决定文档类型）
        let mime = read_entry_opt(&mut zip, "mimetype")?
            .unwrap_or_default();
        let mime = mime.trim().to_string();

        // 2. 提取 content.xml
        let content = read_entry(&mut zip, "content.xml")?;

        // 3. 提取 styles.xml（可选，ODF Package 把图形/段落样式放在这里）
        let styles_xml = read_entry_opt(&mut zip, "styles.xml")?;

        parse_parts(&mime, &content, styles_xml.as_deref())
    }
}

/// 核心转换逻辑：给定 mimetype + content.xml + 可选 styles.xml（均已从各自
/// 打包格式中提取为字符串），转换为统一内存模型。
///
/// 提取自原 `OdfPackageParser::parse`（zip 路径专用），v188 会话为了让
/// KAI 路径（zstd 4-part 拼接，见 odf_package_writer*.rs::pack_zstd /
/// lib.rs::open_kai_bytes）复用同一套"root节点转换+styles注入+mimetype
/// 分发"逻辑而抽出。两条路径（zip 经 OdfPackageParser::parse，KAI 经
/// open_kai_bytes）唯一的区别是"如何拿到 (mime, content_xml, styles_xml)
/// 这三个字符串"，拿到之后走的转换逻辑必须完全一致，不能维护两份。
pub(crate) fn parse_parts(
    mime: &str,
    content: &str,
    styles_xml: Option<&str>,
) -> Result<RoeDocument, ParseError> {
    // document-content → document（统一根元素，复用 Flat 解析器）
    // ODF Package 的 content.xml 根元素是 office:document-content，
    // Flat 格式根元素是 office:document；两者 body/master-styles 结构相同。
    // 最简兼容：把根元素名替换掉再喂给现有 parser。
    let mut normalized = content
        .replace("office:document-content", "office:document");

    // 把 styles.xml 里的样式注入到 content.xml 的 automatic-styles 里
    // styles.xml 包含 office:styles（命名样式）和 office:automatic-styles（自动样式），
    // 两者都可能含图形样式，一并注入。
    if let Some(styles) = styles_xml {
        let injected = extract_style_nodes(styles);
        if !injected.is_empty() {
            // 找 office:automatic-styles 结束标签，在它前面插入
            if let Some(pos) = normalized.find("</office:automatic-styles>") {
                normalized.insert_str(pos, &injected);
            } else if let Some(pos) = normalized.find("<office:body") {
                // 没有 automatic-styles 块，插入一个新的
                let block = format!("<office:automatic-styles>{}</office:automatic-styles>", injected);
                normalized.insert_str(pos, &block);
            }
        }
    }

    if mime.contains("spreadsheet") {
        Ok(RoeDocument::Calc(FodsParser::parse(&normalized)?))
    } else if mime.contains("presentation") {
        Ok(RoeDocument::Impress(FodpParser::parse(&normalized)?))
    } else if mime.contains("text") {
        Ok(RoeDocument::Writer(FodtParser::parse(&normalized)?))
    } else {
        if normalized.contains("opendocument.spreadsheet") {
            Ok(RoeDocument::Calc(FodsParser::parse(&normalized)?))
        } else if normalized.contains("opendocument.presentation") {
            Ok(RoeDocument::Impress(FodpParser::parse(&normalized)?))
        } else {
            Ok(RoeDocument::Writer(FodtParser::parse(&normalized)?))
        }
    }
}

// ─── styles.xml 注入 ──────────────────────────────────────────────────────────

/// 从 styles.xml 提取所有 style:style / style:default-style / draw:gradient 等节点的原始 XML 片段，
/// 用于注入到 content.xml 的 office:automatic-styles 块中。
/// 策略：提取 office:styles 和 office:automatic-styles 的直接子节点文本。
fn extract_style_nodes(styles_xml: &str) -> String {
    let mut result = String::new();
    // 提取 office:automatic-styles 内容
    if let Some(start) = styles_xml.find("<office:automatic-styles>") {
        if let Some(end) = styles_xml.find("</office:automatic-styles>") {
            let inner = &styles_xml[start + "<office:automatic-styles>".len()..end];
            result.push_str(inner);
        }
    }
    // 提取 office:styles 内容（命名样式，含 draw:gradient/fill-image 等）
    if let Some(start) = styles_xml.find("<office:styles>") {
        if let Some(end) = styles_xml.find("</office:styles>") {
            let inner = &styles_xml[start + "<office:styles>".len()..end];
            result.push_str(inner);
        }
    }
    result
}

// ─── ZIP 工具 ─────────────────────────────────────────────────────────────────

fn read_entry(
    zip: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
) -> Result<String, ParseError> {
    read_entry_opt(zip, name)?
        .ok_or_else(|| ParseError::Malformed(format!("missing entry: {name}")))
}

fn read_entry_opt(
    zip: &mut ZipArchive<std::io::Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<String>, ParseError> {
    match zip.by_name(name) {
        Ok(mut entry) => {
            let mut buf = String::new();
            entry.read_to_string(&mut buf).map_err(ParseError::Io)?;
            Ok(Some(buf))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(ParseError::Zip(e.to_string())),
    }
}

// ─── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_odf_package(mime: &str, content_xml: &str) -> Vec<u8> {
        use std::io::Write;
        let buf = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zw = zip::ZipWriter::new(cursor);

        // mimetype：必须 Stored（无压缩）
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let deflated = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        zw.start_file("mimetype", stored).unwrap();
        zw.write_all(mime.as_bytes()).unwrap();

        zw.start_file("content.xml", deflated).unwrap();
        zw.write_all(content_xml.as_bytes()).unwrap();

        zw.finish().unwrap().into_inner()
    }

    const MINI_FODS_CONTENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
  office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
 <office:body>
  <office:spreadsheet>
   <table:table table:name="Sheet1">
    <table:table-row>
     <table:table-cell office:value-type="float" office:value="42">
      <text:p>42</text:p>
     </table:table-cell>
    </table:table-row>
   </table:table>
  </office:spreadsheet>
 </office:body>
</office:document-content>"#;

    const MINI_FODP_CONTENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"
  office:mimetype="application/vnd.oasis.opendocument.presentation">
 <office:body>
  <office:presentation>
   <draw:page draw:name="Slide1"/>
   <draw:page draw:name="Slide2"/>
  </office:presentation>
 </office:body>
</office:document-content>"#;

    const MINI_FODT_CONTENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
  xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
  xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
  office:mimetype="application/vnd.oasis.opendocument.text">
 <office:body>
  <office:text>
   <text:p>Hello ODF Package</text:p>
  </office:text>
 </office:body>
</office:document-content>"#;

    #[test]
    fn test_ods_package() {
        let data = make_odf_package(
            "application/vnd.oasis.opendocument.spreadsheet",
            MINI_FODS_CONTENT,
        );
        let doc = OdfPackageParser::parse(&data).unwrap();
        match doc {
            RoeDocument::Calc(wb) => assert!(!wb.sheets.is_empty()),
            other => panic!("expected Calc, got {:?}", other),
        }
    }

    #[test]
    fn test_odp_package() {
        let data = make_odf_package(
            "application/vnd.oasis.opendocument.presentation",
            MINI_FODP_CONTENT,
        );
        let doc = OdfPackageParser::parse(&data).unwrap();
        match doc {
            RoeDocument::Impress(prs) => assert_eq!(prs.slides.len(), 2),
            other => panic!("expected Impress, got {:?}", other),
        }
    }

    #[test]
    fn test_odt_package() {
        let data = make_odf_package(
            "application/vnd.oasis.opendocument.text",
            MINI_FODT_CONTENT,
        );
        let doc = OdfPackageParser::parse(&data).unwrap();
        match doc {
            RoeDocument::Writer(_) => {}
            other => panic!("expected Writer, got {:?}", other),
        }
    }

    #[test]
    fn test_unknown_mime_fallback() {
        // mimetype 为空，靠 content.xml 内容猜测
        let data = make_odf_package("", MINI_FODP_CONTENT);
        let doc = OdfPackageParser::parse(&data).unwrap();
        match doc {
            RoeDocument::Impress(_) => {}
            other => panic!("expected Impress fallback, got {:?}", other),
        }
    }
}

//! roe-parser：格式 import / export
//! ODF Flat (FODT/FODS/FODP) ←→ roe-model
//! OOXML (docx/xlsx/pptx)    ←→ roe-model
//! KAI (.wai/.sai/.pai/.kai) = zstd(ODF Flat)

pub mod csv;
pub mod odf;
pub mod ooxml;
pub mod kai;
pub mod pivot_expand;
pub mod default_template;

pub use kai::{KaiCodec, KaiSubtype};

#[derive(thiserror::Error, Debug)]
pub enum ParseError {
    #[error("XML parse error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("Malformed document: {0}")]
    Malformed(String),
    #[error("Zip error: {0}")]
    Zip(String),
}

use roe_model::{calc::Workbook, impress::Presentation, writer::WriterDocument};

/// 统一文档模型：KAI 文件解压解析后得到的内存模型之一
#[derive(Debug, Clone)]
pub enum RoeDocument {
    Writer(WriterDocument),
    Calc(Workbook),
    Impress(Presentation),
}

impl RoeDocument {
    /// 打开一个 KAI 文件（.wai/.sai/.pai/.kai）：
    /// 按扩展名判断子类型 → zstd 解压 → 对应 ODF Flat 解析器 → 统一内存模型
    /// 新建空白 Writer 文档（阶段C：roe_new_document 支撑函数）。
    /// WriterDocument::default() 已足够合法（Default derive），无需额外初始化。
    pub fn new_writer() -> RoeDocument {
        RoeDocument::Writer(WriterDocument::default())
    }

    /// 新建空白 Calc 文档，自动补一个默认 Sheet（"Sheet1"）。
    /// Workbook::default() 的 sheets 是空 Vec，若不补默认 sheet，
    /// 前端会看到没有任何 sheet 的非法空白表格。
    pub fn new_calc() -> RoeDocument {
        let sheet = roe_model::calc::Sheet {
            name: "Sheet1".to_string(),
            ..Default::default()
        };
        let wb = Workbook {
            sheets: vec![sheet],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        RoeDocument::Calc(wb)
    }

    /// 新建空白 Impress 文档（阶段C：roe_new_document 支撑函数）。
    /// Presentation 无 Default derive（slide_size 若走全零 Default 不合理），
    /// 手写字段：slide_size 为 16:9（13.333"x7.5" = 960pt x 540pt，
    /// EMU 12192000x6858000）。注意这与 pptx.rs 解析器自身的 fallback
    /// （10"x7.5" = 720pt x 540pt，4:3）不同：fallback 只用于打开缺少
    /// sldSz 的文件，新建文稿用 16:9。
    /// 注：本函数补单测时发现 pptx.rs 原 fallback EMU 常量写错
    /// （25400000/19050000，应为 9144000/6858000，即 10"/7.5" 按
    /// 914400 EMU/inch 换算），已一并修正 pptx.rs 三处同款错误
    /// （模块级初始值 + sldSz 标签两处 unwrap_or 兜底），
    /// 此处同步使用修正后的正确值。
    /// 预置一张空白 slide（否则前端看到零幻灯片的非法演示文稿）。
    pub fn new_impress() -> RoeDocument {
        let blank_slide = roe_model::impress::Slide { layout_idx: Some(1),
            shapes: crate::default_template::slide_shapes_from_layout(&crate::default_template::default_master().layouts[1]),
            master_idx: Some(0),
            animation_tree: roe_model::impress::AnimationNode::default(),
            transition: None,
            notes: None,
            background: None,
            layout_shapes: vec![],
        };
        let pres = Presentation {
            slide_size: (12192000_f32 / 12700.0, 6858000_f32 / 12700.0), // 16:9 (960x540pt)
            slides: vec![blank_slide],
            master_slides: vec![crate::default_template::default_master()],
            styles: roe_model::style::StyleRegistry::default(),
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        };
        RoeDocument::Impress(pres)
    }

    pub fn open_kai_file<P: AsRef<std::path::Path>>(path: P) -> Result<RoeDocument, ParseError> {
        let path = path.as_ref();
        let subtype = path
            .to_str()
            .and_then(KaiCodec::detect_subtype)
            .ok_or_else(|| ParseError::UnsupportedFormat(path.display().to_string()))?;

        let raw = std::fs::read(path)?;
        Self::open_kai_bytes(&raw, subtype)
    }

    /// 解析内存中的 KAI 字节流（子类型已知，由调用方指定）：
    /// zstd 解压 → 对应 ODF Flat 解析器 → 统一内存模型
    ///
    /// 供 roe-api::roe_open_document_from_bytes 使用：FFI 边界传入
    /// `(data, len, format_hint)`，子类型由 format_hint 直接映射得到，
    /// 不需要再走扩展名判断。
    pub fn open_kai_bytes(data: &[u8], subtype: KaiSubtype) -> Result<RoeDocument, ParseError> {
        // v188 会话：KAI 格式从"单一 Flat XML 直压"改为"4-part 拼接
        // （mimetype+manifest.xml+content.xml+styles.xml）再压"，对齐
        // pack_zip 覆盖的 part 清单，与 write_wai/write_sai/write_pai
        // 是同一套格式（见 odf/odf_package_writer*.rs::pack_zstd 说明）。
        // 拿到 4 段后复用 zip 路径同一套转换逻辑（odf::package::parse_parts），
        // 不重新实现一遍 root 节点转换/styles 注入。
        match subtype {
            KaiSubtype::Wai => {
                let (mime, _manifest, content_xml, styles_xml) =
                    odf::odf_package_writer_wai::unpack_zstd_wai(data)?;
                odf::package::parse_parts(&mime, &content_xml, Some(&styles_xml))
            }
            KaiSubtype::Sai => {
                let (mime, _manifest, content_xml, styles_xml) =
                    odf::odf_package_writer::unpack_zstd_sai(data)?;
                odf::package::parse_parts(&mime, &content_xml, Some(&styles_xml))
            }
            KaiSubtype::Pai => {
                let (mime, _manifest, content_xml, styles_xml) =
                    odf::odf_package_writer_pai::unpack_zstd_pai(data)?;
                odf::package::parse_parts(&mime, &content_xml, Some(&styles_xml))
            }
            KaiSubtype::Kai => Err(ParseError::UnsupportedFormat(
                "FODF (.kai) parser not yet implemented".to_string(),
            )),
        }
    }
    /// 直接解析 FODT（ODF Flat Text XML）字节流
    pub fn open_fodt_bytes(data: &[u8]) -> Result<RoeDocument, ParseError> {
        let xml = std::str::from_utf8(data)
            .map_err(|e| ParseError::Malformed(format!("invalid utf-8: {e}")))?;
        Ok(RoeDocument::Writer(odf::FodtParser::parse(xml)?))
    }

    /// 直接解析 FODS（ODF Flat Spreadsheet XML）字节流
    pub fn open_fods_bytes(data: &[u8]) -> Result<RoeDocument, ParseError> {
        let xml = std::str::from_utf8(data)
            .map_err(|e| ParseError::Malformed(format!("invalid utf-8: {e}")))?;
        Ok(RoeDocument::Calc(odf::FodsParser::parse(xml)?))
    }

    /// 直接解析 FODP（ODF Flat Presentation XML）字节流
    pub fn open_fodp_bytes(data: &[u8]) -> Result<RoeDocument, ParseError> {
        let xml = std::str::from_utf8(data)
            .map_err(|e| ParseError::Malformed(format!("invalid utf-8: {e}")))?;
        Ok(RoeDocument::Impress(odf::FodpParser::parse(xml)?))
    }

    /// 解析内存中的 XLSX 字节流 → RoeDocument::Calc
    pub fn open_xlsx_bytes(data: &[u8]) -> Result<RoeDocument, ParseError> {
        Ok(RoeDocument::Calc(crate::ooxml::xlsx::XlsxParser::parse(data)?))
    }

    /// 打开一个 XLSX 文件
    pub fn open_xlsx_file<P: AsRef<std::path::Path>>(path: P) -> Result<RoeDocument, ParseError> {
        let data = std::fs::read(path)?;
        Self::open_xlsx_bytes(&data)
    }

    /// 解析内存中的 DOCX 字节流 → RoeDocument::Writer
    pub fn open_docx_bytes(data: &[u8]) -> Result<RoeDocument, ParseError> {
        Ok(RoeDocument::Writer(crate::ooxml::docx::DocxParser::parse(data)?))
    }

    /// 打开一个 DOCX 文件
    pub fn open_docx_file<P: AsRef<std::path::Path>>(path: P) -> Result<RoeDocument, ParseError> {
        let data = std::fs::read(path)?;
        Self::open_docx_bytes(&data)
    }

    /// 解析内存中的 PPTX 字节流 → RoeDocument::Impress
    pub fn open_pptx_bytes(data: &[u8]) -> Result<RoeDocument, ParseError> {
        Ok(RoeDocument::Impress(crate::ooxml::pptx::PptxParser::parse(data)?))
    }

    /// 打开一个 PPTX 文件
    pub fn open_pptx_file<P: AsRef<std::path::Path>>(path: P) -> Result<RoeDocument, ParseError> {
        let data = std::fs::read(path)?;
        Self::open_pptx_bytes(&data)
    }

    /// 解析内存中的 ODF Package 字节流（.odt/.ods/.odp）→ RoeDocument
    pub fn open_odf_bytes(data: &[u8]) -> Result<RoeDocument, ParseError> {
        crate::odf::OdfPackageParser::parse(data)
    }

    /// 打开一个 ODF Package 文件（.odt/.ods/.odp）
    pub fn open_odf_file<P: AsRef<std::path::Path>>(path: P) -> Result<RoeDocument, ParseError> {
        let data = std::fs::read(path)?;
        Self::open_odf_bytes(&data)
    }

    /// 解析内存中的 CSV 字节流 → RoeDocument::Calc
    pub fn open_csv_bytes(data: &[u8], sheet_name: Option<&str>) -> Result<RoeDocument, ParseError> {
        Ok(RoeDocument::Calc(crate::csv::CsvParser::parse(data, sheet_name)?))
    }

    /// 打开一个 CSV 文件（自动检测编码和分隔符）
    pub fn open_csv_file<P: AsRef<std::path::Path>>(path: P, sheet_name: Option<&str>) -> Result<RoeDocument, ParseError> {
        let data = std::fs::read(path)?;
        Self::open_csv_bytes(&data, sheet_name)
    }
}

#[cfg(test)]
mod kai_open_tests {
    use super::*;

    // v188起：KAI 字节格式从"单一 Flat XML 直压"改为"4-part 拼接
    // （mimetype+manifest.xml+content.xml+styles.xml）再压"（详见
    // odf/odf_package_writer_pai.rs::pack_zstd），因此这个测试不能再
    // 手工拼一份旧格式的 KAI 字节，必须走真实的 OdfPackageWriterPai::
    // write_pai 生成，才能验证 open_kai_bytes/open_kai_file 与生产写出
    // 路径实际兼容（而不是自己造一份格式跟自己对齐，测不出真实问题）。
    fn make_pres_with_two_slides() -> roe_model::impress::Presentation {
        use roe_model::impress::{AnimationNode, Presentation, Slide};

        let slide = Slide { layout_idx: None,
            shapes: vec![],
            master_idx: None,
            animation_tree: AnimationNode::default(),
            transition: None,
            background: None,
            layout_shapes: vec![],
            notes: None,
        };
        Presentation {
            styles: Default::default(),
            slide_size: (960.0, 540.0),
            slides: vec![slide.clone(), slide],
            master_slides: vec![],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        }
    }

    #[test]
    fn test_open_pai_roundtrip() {
        let pres = make_pres_with_two_slides();
        let compressed = crate::odf::OdfPackageWriterPai::write_pai(&pres).unwrap();

        let mut path = std::env::temp_dir();
        path.push(format!("roe_test_{}.pai", std::process::id()));
        std::fs::write(&path, &compressed).unwrap();

        let doc = RoeDocument::open_kai_file(&path).unwrap();
        std::fs::remove_file(&path).ok();

        match doc {
            RoeDocument::Impress(pres) => assert_eq!(pres.slides.len(), 2),
            other => panic!("expected Impress, got {:?}", other),
        }
    }

    #[test]
    fn test_detect_subtype_from_extension() {
        assert_eq!(KaiCodec::detect_subtype("foo.pai"), Some(KaiSubtype::Pai));
        assert_eq!(KaiCodec::detect_subtype("foo.wai"), Some(KaiSubtype::Wai));
        assert_eq!(KaiCodec::detect_subtype("foo.sai"), Some(KaiSubtype::Sai));
        assert_eq!(KaiCodec::detect_subtype("foo.txt"), None);
    }

    // ─── v188 §3.5 步骤5：write_wai/write_sai/write_pai → open_kai_bytes
    // 完整闭环 round-trip 测试 ────────────────────────────────────────
    // 验证目标：用真实的 Package 层 writer 写出 KAI 字节（4-part 拼接+
    // zstd），能被改造后的 open_kai_bytes 正确读回，语义（不是仅"能
    // parse 成功"）与写出前一致。覆盖 Wai/Sai/Pai 三种类型。

    #[test]
    fn test_wai_write_then_open_kai_bytes_roundtrip() {
        use roe_model::writer::{BodyElement, Paragraph, Run, WriterDocument};
        use roe_model::common::StyleRef;

        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Paragraph(Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: "KAI wai端到端测试文字".to_string(), ..Default::default() }],
            ..Default::default()
        }));

        let bytes = crate::odf::OdfPackageWriterWai::write_wai(&doc).expect("write_wai failed");
        let opened = RoeDocument::open_kai_bytes(&bytes, KaiSubtype::Wai).expect("open_kai_bytes failed");

        match opened {
            RoeDocument::Writer(doc2) => {
                let has_text = doc2.body.iter().any(|el| match el {
                    BodyElement::Paragraph(p) => p.runs.iter().any(|r| r.text.contains("KAI wai端到端测试文字")),
                    _ => false,
                });
                assert!(has_text, "读回的文档应包含原始段落文字");
            }
            other => panic!("expected Writer, got {:?}", other),
        }
    }

    #[test]
    fn test_sai_write_then_open_kai_bytes_roundtrip() {
        use roe_model::calc::{CellBlock, CellValue, Sheet, SparseColumn, SparseGrid, Workbook};
        use std::collections::HashMap;

        let mut grid = SparseGrid::default();
        grid.cols.insert(0, SparseColumn {
            blocks: vec![CellBlock {
                start_row: 0,
                values: vec![CellValue::Text("KAI sai端到端测试".to_string())].into(),
            }].into(),
        });
        let wb = Workbook {
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
        };

        let bytes = crate::odf::OdfPackageWriter::write_sai(&wb).expect("write_sai failed");
        let opened = RoeDocument::open_kai_bytes(&bytes, KaiSubtype::Sai).expect("open_kai_bytes failed");

        match opened {
            RoeDocument::Calc(wb2) => {
                assert_eq!(wb2.sheets.len(), 1);
                assert_eq!(wb2.sheets[0].name, "Sheet1");
            }
            other => panic!("expected Calc, got {:?}", other),
        }
    }

    #[test]
    fn test_pai_write_then_open_kai_bytes_roundtrip() {
        let pres = make_pres_with_two_slides();
        let bytes = crate::odf::OdfPackageWriterPai::write_pai(&pres).expect("write_pai failed");
        let opened = RoeDocument::open_kai_bytes(&bytes, KaiSubtype::Pai).expect("open_kai_bytes failed");

        match opened {
            RoeDocument::Impress(pres2) => {
                assert_eq!(pres2.slides.len(), 2);
            }
            other => panic!("expected Impress, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod new_document_tests {
    use super::*;

    #[test]
    fn test_new_writer_returns_writer_variant() {
        let doc = RoeDocument::new_writer();
        match doc {
            RoeDocument::Writer(_) => {}
            other => panic!("expected Writer, got {:?}", other),
        }
    }

    #[test]
    fn test_new_calc_has_one_default_sheet() {
        let doc = RoeDocument::new_calc();
        match doc {
            RoeDocument::Calc(wb) => {
                assert_eq!(wb.sheets.len(), 1);
                assert_eq!(wb.sheets[0].name, "Sheet1");
            }
            other => panic!("expected Calc, got {:?}", other),
        }
    }

    #[test]
    fn test_new_impress_has_one_slide_and_default_size() {
        let doc = RoeDocument::new_impress();
        match doc {
            RoeDocument::Impress(pres) => {
                assert_eq!(pres.slides.len(), 1);
                let (w, h) = pres.slide_size;
                assert!((w - 960.0_f32).abs() < 0.01, "unexpected width: {}", w);
                assert!((h - 540.0_f32).abs() < 0.01, "unexpected height: {}", h);
            }
            other => panic!("expected Impress, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod comment_reply_chain_e2e_tests {
    use super::*;

    /// 端到端验证：从真实docx字节流 open_docx_bytes 解析，一路到
    /// WriterDocument::comment_threads()，确认"XML解析parent_id回填"
    /// 和"comment_threads()分组"这两段各自单测都通过的代码，接起来
    /// 之后真的连通，不是"两段分别对、组合起来却断了"。
    /// 样本：CommentReply.docx，一对一回复场景
    /// （id=1 "Parent" <- id=0 "Child"）。
    #[test]
    fn open_docx_bytes_then_comment_threads_resolves_one_to_one_reply() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test_samples/docx/sw_ooxmlexport_data__CommentReply.docx"
        );
        let data = std::fs::read(path).expect("CommentReply.docx sample should exist");
        let doc = RoeDocument::open_docx_bytes(&data).expect("should parse CommentReply.docx");

        let wdoc = match doc {
            RoeDocument::Writer(d) => d,
            other => panic!("expected Writer, got {:?}", other),
        };

        let threads = wdoc.comment_threads();
        assert_eq!(threads.len(), 1, "应该只有一条讨论串（Child归入Parent下）");
        assert_eq!(threads[0].root.id, 1, "root应该是Parent(id=1)");
        assert_eq!(threads[0].replies.len(), 1);
        assert_eq!(threads[0].replies[0].id, 0, "reply应该是Child(id=0)");
    }

    /// 样本：comment-annotationref.docx，一对多回复场景
    /// （id=1 原批注 <- id=2 "r1" 和 id=3 "r2"，都是同一层回复，不是链式）。
    #[test]
    fn open_docx_bytes_then_comment_threads_resolves_one_to_many_replies() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test_samples/docx/sw_ooxmlexport_data__comment-annotationref.docx"
        );
        let data = std::fs::read(path).expect("comment-annotationref.docx sample should exist");
        let doc = RoeDocument::open_docx_bytes(&data).expect("should parse comment-annotationref.docx");

        let wdoc = match doc {
            RoeDocument::Writer(d) => d,
            other => panic!("expected Writer, got {:?}", other),
        };

        let threads = wdoc.comment_threads();
        let original_thread = threads.iter().find(|t| t.root.id == 1)
            .expect("原批注(id=1)应该作为某个thread的root存在");
        assert_eq!(original_thread.replies.len(), 2, "id=1应该有2条回复");
        let reply_ids: Vec<u32> = original_thread.replies.iter().map(|c| c.id).collect();
        assert!(reply_ids.contains(&2), "reply应包含id=2(r1)");
        assert!(reply_ids.contains(&3), "reply应包含id=3(r2)");
    }
}

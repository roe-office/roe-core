//! roe_open_document_from_bytes / roe_close_document / roe_save_document_to_bytes / roe_get_document_size

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use roe_parser::{KaiCodec, KaiSubtype, RoeDocument, ParseError};
use roe_writer::layout::{arena_to_layout_pages, ParagraphLayoutEngine};
use roe_writer::FontRegistry;

use crate::error::{set_last_error, RoeErrorCode};
use crate::handle::{self, DocumentEntry};

/// format_hint 取值。
/// KAI 格式：Wai/Sai/Pai
/// OOXML：Docx/Xlsx/Pptx
/// ODF Package：Odt/Ods/Odp
/// Auto：按内容自动检测（先尝试 zstd 解压，失败则尝试 ZIP，失败则尝试 XML）
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoeFormatHint {
    Wai  = 1,  // KAI: zstd(FODT)
    Sai  = 2,  // KAI: zstd(FODS)
    Pai  = 3,  // KAI: zstd(FODP)
    Docx = 4,  // OOXML Word
    Xlsx = 5,  // OOXML Calc
    Pptx = 6,  // OOXML Impress
    Odt  = 7,  // ODF Package Word
    Ods  = 8,  // ODF Package Calc
    Odp  = 9,  // ODF Package Impress
    Csv  = 10, // CSV
    Auto = 99, // 自动检测
}

/// 打开文档，返回句柄（0 表示失败）。
/// format_hint 取值见 RoeFormatHint，传 99 自动检测。
#[no_mangle]
pub extern "C" fn roe_open_document_from_bytes(
    data: *const u8,
    len: usize,
    format_hint: i32,
) -> u64 {
    if data.is_null() {
        set_last_error("roe_open_document_from_bytes: data is null");
        return 0;
    }

    let bytes = unsafe { slice::from_raw_parts(data, len) };
    open_document_bytes_impl(bytes, format_hint)
}

/// 从字节打开文档——内部 Rust 版本,供 roe-agent 等同进程消费者直接调用
/// (不跨 FFI)。语义与 roe_open_document_from_bytes 完全一致;
/// 失败返回 0,原因见 set_last_error。format_hint 同 RoeFormatHint(4=docx 5=xlsx 6=pptx 10=csv 99=自动)。
pub fn open_document_bytes_impl(bytes: &[u8], format_hint: i32) -> u64 {
    let doc = match format_hint {
        1  => RoeDocument::open_kai_bytes(bytes, KaiSubtype::Wai),
        2  => RoeDocument::open_kai_bytes(bytes, KaiSubtype::Sai),
        3  => RoeDocument::open_kai_bytes(bytes, KaiSubtype::Pai),
        4  => RoeDocument::open_docx_bytes(bytes),
        5  => RoeDocument::open_xlsx_bytes(bytes),
        6  => RoeDocument::open_pptx_bytes(bytes),
        7  => RoeDocument::open_odf_bytes(bytes),
        8  => RoeDocument::open_odf_bytes(bytes),
        9  => RoeDocument::open_odf_bytes(bytes),
        10 => RoeDocument::open_csv_bytes(bytes, None),
        99 => auto_detect(bytes),
        other => {
            set_last_error(format!(
                "roe_open_document_from_bytes: unsupported format_hint {other}"
            ));
            return 0;
        }
    };

    match doc {
        Ok(mut doc) => {
            if let RoeDocument::Calc(wb) = &mut doc {
                for sh in wb.sheets.iter_mut() { roe_model::object_ids::ensure_object_ids(sh); }
            }
            let calc_engine = init_calc_engine_for_doc(&mut doc);
            handle::insert(DocumentEntry {
                doc,
                snapshot: None,
            is_shadow: false,
                events: VecDeque::new(),
                animation: HashMap::new(),
                writer_layout_cache: None,
                writer_font_registry: None,
                undo_stack: Vec::new(),
                redo_stack: Vec::new(),
                writer_options: handle::WriterOptions::default(),
                impress_font_registry: None,
                writer_parley_font_context: None,
                calc_engine,
                pending_comment_hits: Vec::new(),
            })
        }
        Err(e) => {
            set_last_error(format!("roe_open_document_from_bytes: {e}"));
            0
        }
    }
}

/// v118新增：打开文档时，如果是 Calc 文档，扫描所有 sheet 里的公式单元格坐标，
/// 立即触发一次全量重算并把结果写回 workbook（写回 CellValue::Formula.cached）。
///
/// 设计原则（与 v112 透视表 cache 教训一致）：绝不信任文件里可能存在的静态缓存值，
/// 一律用 roe-calc 自己重算一次，参考 OnlyOffice 的做法。此函数只在"首次打开"时
/// 跑一次全量重算，后续用户编辑触发的增量重算走 roe_execute_script 里已有的
/// calc_engine.get_or_insert_with 逻辑，不受影响。
///
/// 非 Calc 文档（Writer/Impress）直接返回 None，不做任何操作。
pub fn init_calc_engine_for_doc(doc: &mut RoeDocument) -> Option<roe_calc::CalcEngine> {
    let wb = match doc {
        RoeDocument::Calc(wb) => wb,
        _ => return None,
    };

    let mut dirty_cells: Vec<(usize, u32, u32)> = Vec::new();
    for (sheet_idx, sheet) in wb.sheets.iter().enumerate() {
        for (&col, column) in sheet.cells.cols.iter() {
            for block in &column.blocks {
                for (offset, value) in block.values.iter().enumerate() {
                    if matches!(value, roe_model::calc::CellValue::Formula { .. }) {
                        let row = block.start_row + offset as u32;
                        dirty_cells.push((sheet_idx, row, col));
                    }
                }
            }
        }
    }

    if dirty_cells.is_empty() {
        return Some(roe_calc::CalcEngine::new());
    }

    let mut engine = roe_calc::CalcEngine::new();
    let _applied = engine.recalc_and_apply(wb, &dirty_cells);
    // 命名公式(named_formulas)展开走 eval_formula 直读 workbook 当前值,
    // 不向 DAG 登记依赖;首轮若引用者先于被引用格求值会读到空缓存
    // (named-ranges-global.xlsx 的 Global5->global6->Sheet2!B1)。
    // 有命名公式时补一轮重算,等价于 LOK 的 DoHardRecalc。
    if !wb.named_formulas.is_empty() {
        let _ = engine.recalc_and_apply(wb, &dirty_cells);
    }
    Some(engine)
}

/// 自动检测格式并打开。
/// 策略：先尝试 zstd 解压（KAI），再尝试 ZIP 解压（OOXML/ODF Package），
/// 再尝试 UTF-8 XML（ODF Flat），最后尝试 CSV。
fn auto_detect(bytes: &[u8]) -> Result<RoeDocument, ParseError> {
    // 1. 尝试 KAI（zstd magic: 0xFD2FB528）
    // v188起：KAI 是 4-part 拼接（mimetype+manifest+content+styles）再 zstd，
    // 不再是单一 XML 直压。mimetype 是 4 段中的第一段，用
    // unpack_zstd_sai（三个 unpack_zstd_* 帧格式完全一致，用哪个解都行，
    // 这里只需要 mime 字段本身，不关心是否真的是 Sai）解出来即可判断子类型，
    // 再交给 open_kai_bytes 用正确的子类型重新走一遍完整解析。
    if bytes.len() >= 4 && &bytes[..4] == b"\xfd\x2f\xb5\x28" {
        if let Ok((mime, _manifest, _content, _styles)) =
            roe_parser::odf::odf_package_writer::unpack_zstd_sai(bytes)
        {
            return if mime.contains("spreadsheet") {
                RoeDocument::open_kai_bytes(bytes, KaiSubtype::Sai)
            } else if mime.contains("presentation") {
                RoeDocument::open_kai_bytes(bytes, KaiSubtype::Pai)
            } else {
                RoeDocument::open_kai_bytes(bytes, KaiSubtype::Wai)
            };
        }
    }

    // 2. 尝试 ZIP（OOXML / ODF Package）
    if bytes.len() >= 2 && &bytes[..2] == b"PK" {
        // 先尝试 OOXML（检查 [Content_Types].xml 中的 relationship type）
        if let Ok(doc) = try_zip_ooxml(bytes) {
            return Ok(doc);
        }
        // 再尝试 ODF Package
        return RoeDocument::open_odf_bytes(bytes);
    }

    // 3. 尝试 ODF Flat XML（直接 UTF-8 XML）
    if let Ok(xml) = std::str::from_utf8(bytes) {
        let xml = xml.trim_start();
        if xml.starts_with("<?xml") || xml.starts_with("<office:document") {
            // 根据 mimetype 判断具体格式
            if xml.contains("opendocument.text") {
                return RoeDocument::open_fodt_bytes(bytes);
            } else if xml.contains("opendocument.spreadsheet") {
                return RoeDocument::open_fods_bytes(bytes);
            } else if xml.contains("opendocument.presentation") {
                return RoeDocument::open_fodp_bytes(bytes);
            }
            return RoeDocument::open_odf_bytes(bytes);
        }
    }

    // 4. 最后尝试 CSV
    RoeDocument::open_csv_bytes(bytes, None)
}

/// 尝试依次以 docx/xlsx/pptx 解析 ZIP 格式字节流。
fn try_zip_ooxml(bytes: &[u8]) -> Result<RoeDocument, ParseError> {
    if let Ok(doc) = RoeDocument::open_docx_bytes(bytes) { return Ok(doc); }
    if let Ok(doc) = RoeDocument::open_xlsx_bytes(bytes) { return Ok(doc); }
    if let Ok(doc) = RoeDocument::open_pptx_bytes(bytes) { return Ok(doc); }
    Err(ParseError::UnsupportedFormat("unknown ZIP-based format".to_string()))
}

/// 关闭文档，释放句柄。成功返回 0，句柄不存在返回 -1。
/// 关闭文档并释放句柄——内部 Rust 版本。句柄存在返回 true。
/// 会话级 DataFrame 数据集随 handle::remove 一并释放(见 handle::DATASET_STORES)。
pub fn close_document_impl(handle: u64) -> bool {
    handle::remove(handle).is_some()
}

#[no_mangle]
pub extern "C" fn roe_close_document(handle: u64) -> i32 {
    if close_document_impl(handle) {
        RoeErrorCode::Ok as i32
    } else {
        RoeErrorCode::InvalidHandle as i32
    }
}

/// 将文档序列化为 KAI 字节并通过回调返回给调用方。
///
/// 成功时调用 `callback(out_data, out_len, userdata)` 一次，返回 0。
/// 失败返回 -1，错误信息见 roe_last_error_message。
///
/// 支持写出格式：Wai/Sai/Pai（KAI），三者均为真实导出（v188 会话前的
/// "Wai/Pai 仍是占位空文档"描述已过时并在 v188 会话改正——这是本项目
/// 第三次发现"函数级注释滞后于代码"的案例，详见 roe-save-design.md
/// §4.3）。三者内部分别调用 OdfPackageWriterWai::write_wai /
/// OdfPackageWriter::write_sai / OdfPackageWriterPai::write_pai，产出
/// 内容与 roe_save_document_to_odf_bytes（ODF 原生 zip 路径）完全一致，
/// 仅打包方式不同（KAI 用 4-part 拼接 + zstd，ODF 原生用 zip）。
#[no_mangle]
pub extern "C" fn roe_save_document_to_bytes(
    handle: u64,
    format_hint: i32,
    callback: extern "C" fn(*const u8, usize, *mut std::ffi::c_void),
    userdata: *mut std::ffi::c_void,
) -> i32 {
    // 确定目标 KAI 子类型
    let subtype = match format_hint {
        1 => KaiSubtype::Wai,
        2 => KaiSubtype::Sai,
        3 => KaiSubtype::Pai,
        other => {
            set_last_error(format!("roe_save_document_to_bytes: unsupported format_hint {other}; only KAI (1/2/3) supported"));
            return RoeErrorCode::Unsupported as i32;
        }
    };

    let result = handle::with_entry(handle, |entry| {
        serialize_to_kai(&entry.doc, subtype)
    });

    match result {
        None => {
            set_last_error("roe_save_document_to_bytes: invalid handle");
            RoeErrorCode::InvalidHandle as i32
        }
        Some(Err(e)) => {
            set_last_error(format!("roe_save_document_to_bytes: {e}"));
            RoeErrorCode::ParseError as i32
        }
        Some(Ok(bytes)) => {
            callback(bytes.as_ptr(), bytes.len(), userdata);
            RoeErrorCode::Ok as i32
        }
    }
}

/// 将内存模型序列化为 KAI 字节（4-part 拼接 + zstd，见
/// roe-parser/src/odf/odf_package_writer*.rs::pack_zstd）。
/// v188 会话改为直接调用 Package 层 write_wai/write_sai/write_pai——
/// 这三个函数内部已经产出正确格式的 zstd 压缩字节，不需要（也不应该）
/// 再额外调用 KaiCodec::compress 包一层，否则等于压缩两次。
fn serialize_to_kai(doc: &RoeDocument, subtype: KaiSubtype) -> Result<Vec<u8>, ParseError> {
    match (doc, subtype) {
        (RoeDocument::Writer(doc), KaiSubtype::Wai) => roe_parser::odf::OdfPackageWriterWai::write_wai(doc),
        (RoeDocument::Calc(wb),     KaiSubtype::Sai) => roe_parser::odf::OdfPackageWriter::write_sai(wb),
        (RoeDocument::Impress(pres),KaiSubtype::Pai) => roe_parser::odf::OdfPackageWriterPai::write_pai(pres),
        _ => Err(ParseError::UnsupportedFormat(
            "document type does not match format_hint".to_string()
        )),
    }
}

/// 将文档序列化为 OOXML 字节并通过回调返回给调用方（不走 zstd 压缩，
/// zip 本身已是压缩格式）。
///
/// 成功时调用 `callback(out_data, out_len, userdata)` 一次，返回 0。
/// 失败返回 -1，错误信息见 roe_last_error_message。
///
/// 目前支持写出格式：Docx（4）、Xlsx（5）、Pptx（6）。三种 OOXML 写出格式
/// 已全部实现并接入。
/// Xlsx 写出范围对齐 xlsx.rs parser 现状：单元格值（Number/Text/Formula/Bool/
/// Error），不含 col_widths/cell_formats/merged_cells（parser 暂不读取这些字段，
/// 写出后无法 roundtrip 验证，故本版本不写）。
/// Docx 写出范围对齐 docx.rs parser 现状：段落 + 段落样式名（w:pStyle）+ run
/// 文字（w:t），不含字符格式/表格/图片/页眉页脚/list_info（详见
/// roe-parser/src/ooxml/docx_writer.rs 文件头注释）。
/// Pptx 写出范围对齐 pptx.rs parser 现状：slide_size + shape（位置/geometry/
/// fill/stroke宽度/文字或图片），不含动画/转场/备注/母版真实内容（详见
/// roe-parser/src/ooxml/pptx_writer.rs 文件头注释）。
#[no_mangle]
pub extern "C" fn roe_save_document_to_ooxml_bytes(
    handle: u64,
    format_hint: i32,
    callback: extern "C" fn(*const u8, usize, *mut std::ffi::c_void),
    userdata: *mut std::ffi::c_void,
) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        serialize_to_ooxml(&entry.doc, format_hint)
    });

    match result {
        None => {
            set_last_error("roe_save_document_to_ooxml_bytes: invalid handle");
            RoeErrorCode::InvalidHandle as i32
        }
        Some(Err(e)) => {
            set_last_error(format!("roe_save_document_to_ooxml_bytes: {e}"));
            RoeErrorCode::ParseError as i32
        }
        Some(Ok(bytes)) => {
            callback(bytes.as_ptr(), bytes.len(), userdata);
            RoeErrorCode::Ok as i32
        }
    }
}

/// 把文档序列化为 OOXML 字节(docx=4 / xlsx=5 / pptx=6)——内部 Rust 版本。
/// 与 roe_save_document_to_ooxml_bytes 走同一个 serialize_to_ooxml,产物与前台保存一致。
pub fn save_document_ooxml_impl(handle: u64, format_hint: i32) -> Result<Vec<u8>, String> {
    match handle::with_entry(handle, |entry| serialize_to_ooxml(&entry.doc, format_hint)) {
        None => Err(format!("invalid document handle: {handle}")),
        Some(Err(e)) => Err(e.to_string()),
        Some(Ok(bytes)) => Ok(bytes),
    }
}

/// 将内存模型序列化为 OOXML 字节流（zip 包），不经过 KAI/zstd。
/// 目前仅 Xlsx（format_hint=5，与 RoeFormatHint::Xlsx 对应）已实现。
fn serialize_to_ooxml(doc: &RoeDocument, format_hint: i32) -> Result<Vec<u8>, ParseError> {
    match (doc, format_hint) {
        (RoeDocument::Writer(wd), 4) => roe_parser::ooxml::docx_writer::DocxWriter::write(wd),
        (RoeDocument::Calc(wb), 5) => roe_parser::ooxml::xlsx_writer::XlsxWriter::write(wb),
        (RoeDocument::Impress(prs), 6) => roe_parser::ooxml::pptx_writer::PptxWriter::write(prs),
        _ => Err(ParseError::UnsupportedFormat(
            "document type does not match format_hint, or format_hint not in OOXML range (4/5/6)".to_string()
        )),
    }
}

/// 将文档序列化为纯 ODF Package 字节并通过回调返回给调用方（zip 格式，
/// 不经过 KAI/zstd 压缩）。与 roe_save_document_to_ooxml_bytes 对称，是
/// ODF 标准（.odt/.ods/.odp）的原生导出入口——注意这与
/// roe_save_document_to_bytes（KAI，zstd 压缩的 Wai/Sai/Pai）是两条独立
/// 路径，不要混淆：ODF 原生路径产出可被 LibreOffice 等外部软件直接打开
/// 的标准 zip 包，KAI 路径产出的是 ROE 自己的 zstd 压缩变体。
///
/// 成功时调用 `callback(out_data, out_len, userdata)` 一次，返回 0。
/// 失败返回 -1，错误信息见 roe_last_error_message。
///
/// 目前支持写出格式：Odt（7）、Ods（8）、Odp（9）。三种 ODF 写出均已实现
/// （roe-parser/src/odf/odf_package_writer*.rs），阶段一/二/三（round-trip
/// 测试，见 roe-save-design.md §5.2）均已完成。KAI 路径（Wai/Sai/Pai，见
/// roe_save_document_to_bytes）与本函数产出内容完全一致，仅打包容器不同
/// （zip vs zstd 4-part 拼接），两条路径已于 v188 会话统一收尾。写出范围
/// 与各自 Flat
/// Writer（FodtWriter/FodsWriter/FodpWriter）现有能力完全一致，styles.xml
/// 本轮为标准空壳（命名样式内嵌在 content.xml 里，详见
/// odf_package_writer_wai.rs/odf_package_writer_pai.rs 文件头注释）。
#[no_mangle]
pub extern "C" fn roe_save_document_to_odf_bytes(
    handle: u64,
    format_hint: i32,
    callback: extern "C" fn(*const u8, usize, *mut std::ffi::c_void),
    userdata: *mut std::ffi::c_void,
) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        serialize_to_odf(&entry.doc, format_hint)
    });

    match result {
        None => {
            set_last_error("roe_save_document_to_odf_bytes: invalid handle");
            RoeErrorCode::InvalidHandle as i32
        }
        Some(Err(e)) => {
            set_last_error(format!("roe_save_document_to_odf_bytes: {e}"));
            RoeErrorCode::ParseError as i32
        }
        Some(Ok(bytes)) => {
            callback(bytes.as_ptr(), bytes.len(), userdata);
            RoeErrorCode::Ok as i32
        }
    }
}

/// 将内存模型序列化为纯 ODF Package 字节流（zip 包），不经过 KAI/zstd。
/// format_hint 对应 RoeFormatHint::Odt(7)/Ods(8)/Odp(9)。
fn serialize_to_odf(doc: &RoeDocument, format_hint: i32) -> Result<Vec<u8>, ParseError> {
    match (doc, format_hint) {
        (RoeDocument::Writer(wd), 7) => roe_parser::odf::OdfPackageWriterWai::write_odt(wd),
        (RoeDocument::Calc(wb), 8) => roe_parser::odf::OdfPackageWriter::write_ods(wb),
        (RoeDocument::Impress(prs), 9) => roe_parser::odf::OdfPackageWriterPai::write_odp(prs),
        _ => Err(ParseError::UnsupportedFormat(
            "document type does not match format_hint, or format_hint not in ODF range (7/8/9)".to_string()
        )),
    }
}

/// 查询文档页面尺寸，返回 JSON 字符串 `{"width_pt":W,"height_pt":H}`。
/// 失败返回 null 指针，错误信息见 roe_last_error_message。
/// 调用方必须用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_get_document_size(handle: u64) -> *mut c_char {
    let result = handle::with_entry(handle, |entry| {
        let (w, h) = document_size(&entry.doc);
        format!("{{\"width_pt\":{:.2},\"height_pt\":{:.2}}}", w, h)
    });

    match result {
        None => {
            set_last_error("roe_get_document_size: invalid handle");
            std::ptr::null_mut()
        }
        Some(s) => match CString::new(s) {
            Ok(cs) => cs.into_raw(),
            Err(e) => {
                set_last_error(format!("roe_get_document_size: {e}"));
                std::ptr::null_mut()
            }
        }
    }
}

/// 从内存模型取页面尺寸（pt）。
fn document_size(doc: &RoeDocument) -> (f32, f32) {
    match doc {
        RoeDocument::Writer(w)  => w.page_size,
        RoeDocument::Impress(p) => p.slide_size,
        RoeDocument::Calc(_)    => (841.89, 595.28), // A4 横向（电子表格无固定页面）
    }
}

/// 查询文档页数。
/// - Writer：返回 layout 后的实际页数
/// - Impress：返回幻灯片数量
/// - Calc：返回 -1（概念不适用）
#[no_mangle]
pub extern "C" fn roe_get_page_count(handle: u64) -> i32 {
    let result = handle::with_entry(handle, |entry| {
        match &entry.doc {
            RoeDocument::Writer(doc) => {
                let registry = FontRegistry::new_with_system_fonts();
                let mut engine = ParagraphLayoutEngine::new(registry);
                let layout_result = engine.layout(doc);
                let pages = arena_to_layout_pages(&layout_result.arena, doc);
                pages.len() as i32
            }
            RoeDocument::Impress(p) => p.slides.len() as i32,
            RoeDocument::Calc(_) => -1,
        }
    });

    match result {
        Some(n) => n,
        None => {
            set_last_error("roe_get_page_count: invalid handle");
            RoeErrorCode::InvalidHandle as i32
        }
    }
}

/// doc_type 取值：
/// - 1 = Writer（空白文档，无内容）
/// - 2 = Calc（自动补一个默认 Sheet "Sheet1"，避免前端看到零 sheet 的非法空白表格）
/// - 3 = Impress（一张空白幻灯片，尺寸 10"x7.5" = 720pt x 540pt，与 pptx 默认标准一致）
///
/// 其他取值返回 0 并设置 last_error。
///
/// 说明：本函数只处理"创建一个新的文档 handle"这件事，属于文档生命周期管理，
/// 不涉及文档内容的语义化编辑，因此只提供 C-ABI，不提供 Rhai 镜像函数——
/// 这与 roe_open_document_from_bytes / roe_close_document / roe_save_document_to_bytes
/// 现状一致（它们同样只有 C-ABI，没有 Rhai 版本）。
///
/// Agent 侧同样直接调用本函数拿到 handle，再用该 handle 调用 roe_execute_script
/// 执行 Rhai 脚本填充文档内容（插入段落/表格/形状等）——C-ABI 不是"前端专用"，
/// 拿到 handle 之后 C-ABI 和 Rhai 两条线对前端和 Agent 都同样开放，参见
/// roe-interface-gap-analysis.md 第8.1节"两条线都是前端和Agent共享"。
/// 新建文档——内部 Rust 版本，供 roe-agent 等同进程 Rust 消费者直接调用
/// （架构决策见 roe-agent/src/lib.rs：不跨 FFI 边界调用 C-ABI）。
/// 语义与 roe_new_document 完全一致（本就是同一段逻辑抽出来的）。
/// 返回 0 表示 doc_type 不合法。
pub fn new_document_impl(doc_type: i32) -> u64 {
    let doc = match doc_type {
        1 => RoeDocument::new_writer(),
        2 => RoeDocument::new_calc(),
        3 => RoeDocument::new_impress(),
        other => {
            set_last_error(format!(
                "roe_new_document: unsupported doc_type {other} (expected 1=Writer, 2=Calc, 3=Impress)"
            ));
            return 0;
        }
    };

    let mut doc = doc;
    let calc_engine = init_calc_engine_for_doc(&mut doc);

    handle::insert(DocumentEntry {
        doc,
        snapshot: None,
            is_shadow: false,
        events: VecDeque::new(),
        animation: HashMap::new(),
        writer_layout_cache: None,
        writer_font_registry: None,
        undo_stack: Vec::new(),
        redo_stack: Vec::new(),
        writer_options: handle::WriterOptions::default(),
        impress_font_registry: None,
        writer_parley_font_context: None,
        calc_engine,
        pending_comment_hits: Vec::new(),
    })
}

#[no_mangle]
pub extern "C" fn roe_new_document(doc_type: i32) -> u64 {
    new_document_impl(doc_type)
}

#[cfg(test)]
mod tests_new_document {
    use super::*;
    use std::ffi::{CStr, CString};

    fn json_from(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        let s = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
        crate::script::roe_free_string(ptr);
        s
    }

    #[test]
    fn test_new_document_writer_returns_nonzero_handle() {
        let h = roe_new_document(1);
        assert_ne!(h, 0, "doc_type=1 (Writer) should return a non-zero handle");
        let is_writer = handle::with_entry(h, |entry| matches!(entry.doc, RoeDocument::Writer(_)));
        assert_eq!(is_writer, Some(true));
        handle::remove(h);
    }

    #[test]
    fn test_new_document_calc_returns_nonzero_handle() {
        let h = roe_new_document(2);
        assert_ne!(h, 0, "doc_type=2 (Calc) should return a non-zero handle");
        let is_calc = handle::with_entry(h, |entry| matches!(entry.doc, RoeDocument::Calc(_)));
        assert_eq!(is_calc, Some(true));
        handle::remove(h);
    }

    #[test]
    fn test_new_document_impress_returns_nonzero_handle() {
        let h = roe_new_document(3);
        assert_ne!(h, 0, "doc_type=3 (Impress) should return a non-zero handle");
        let is_impress = handle::with_entry(h, |entry| matches!(entry.doc, RoeDocument::Impress(_)));
        assert_eq!(is_impress, Some(true));
        handle::remove(h);
    }

    // ── roe_save_document_to_odf_bytes 端到端测试（阶段二）──────────────
    // 用裸指针把回调收到的字节搬进 Vec<u8>，是 extern "C" 回调场景下最
    // 简单的做法，避免引入线程局部变量或 channel。

    extern "C" fn collect_bytes_cb(data: *const u8, len: usize, userdata: *mut std::ffi::c_void) {
        let out = unsafe { &mut *(userdata as *mut Vec<u8>) };
        let slice = unsafe { std::slice::from_raw_parts(data, len) };
        out.extend_from_slice(slice);
    }

    #[test]
    fn test_save_document_to_odf_bytes_writer_odt_roundtrips() {
        let h = roe_new_document(1); // Writer
        assert_ne!(h, 0);

        let mut out: Vec<u8> = Vec::new();
        let rc = roe_save_document_to_odf_bytes(h, 7, collect_bytes_cb, &mut out as *mut _ as *mut std::ffi::c_void);
        assert_eq!(rc, 0, "expected RoeErrorCode::Ok (0)");
        assert!(!out.is_empty());

        let parsed = roe_parser::RoeDocument::open_odf_bytes(&out).expect("parse back failed");
        assert!(matches!(parsed, roe_parser::RoeDocument::Writer(_)));

        handle::remove(h);
    }

    #[test]
    fn test_save_document_to_odf_bytes_calc_ods_roundtrips() {
        let h = roe_new_document(2); // Calc
        assert_ne!(h, 0);

        let mut out: Vec<u8> = Vec::new();
        let rc = roe_save_document_to_odf_bytes(h, 8, collect_bytes_cb, &mut out as *mut _ as *mut std::ffi::c_void);
        assert_eq!(rc, 0, "expected RoeErrorCode::Ok (0)");
        assert!(!out.is_empty());

        let parsed = roe_parser::RoeDocument::open_odf_bytes(&out).expect("parse back failed");
        assert!(matches!(parsed, roe_parser::RoeDocument::Calc(_)));

        handle::remove(h);
    }

    #[test]
    fn test_save_document_to_odf_bytes_impress_odp_roundtrips() {
        let h = roe_new_document(3); // Impress
        assert_ne!(h, 0);

        let mut out: Vec<u8> = Vec::new();
        let rc = roe_save_document_to_odf_bytes(h, 9, collect_bytes_cb, &mut out as *mut _ as *mut std::ffi::c_void);
        assert_eq!(rc, 0, "expected RoeErrorCode::Ok (0)");
        assert!(!out.is_empty());

        let parsed = roe_parser::RoeDocument::open_odf_bytes(&out).expect("parse back failed");
        assert!(matches!(parsed, roe_parser::RoeDocument::Impress(_)));

        handle::remove(h);
    }

    #[test]
    fn test_save_document_to_odf_bytes_wrong_format_hint_fails() {
        let h = roe_new_document(1); // Writer, 但传 Calc 的 format_hint(8)
        assert_ne!(h, 0);

        let mut out: Vec<u8> = Vec::new();
        let rc = roe_save_document_to_odf_bytes(h, 8, collect_bytes_cb, &mut out as *mut _ as *mut std::ffi::c_void);
        assert_ne!(rc, 0, "mismatched doc type vs format_hint should fail");
        assert!(out.is_empty());

        handle::remove(h);
    }

    /// 验证新建的 Calc 文档不仅 handle 有效，sheet 也真的可用——
    /// 能立即通过 roe_execute_script 跑 set_cell/get_cell，不是一个
    /// "有 handle 但内部 sheet 是空/非法状态" 的半成品。
    #[test]
    fn test_new_document_calc_sheet_is_immediately_usable() {
        let h = roe_new_document(2);
        assert_ne!(h, 0);

        let set_script = CString::new("set_cell(0, 0, 0, \"hello\");").unwrap();
        let r1 = json_from(crate::script::roe_execute_script(h, set_script.as_ptr()));
        assert!(r1.contains("\"ok\":true"), "set_cell should succeed on freshly created Calc doc, got: {r1}");

        let get_script = CString::new("get_cell(0, 0, 0)").unwrap();
        let r2 = json_from(crate::script::roe_execute_script(h, get_script.as_ptr()));
        assert!(r2.contains("\"ok\":true"), "get_cell should succeed on freshly created Calc doc, got: {r2}");
        assert!(r2.contains("hello"), "get_cell should return the value just written, got: {r2}");

        handle::remove(h);
    }

    #[test]
    fn test_new_document_invalid_doc_type_returns_zero_and_sets_last_error() {
        let h = roe_new_document(999);
        assert_eq!(h, 0, "invalid doc_type should return 0");

        let err_ptr = crate::error::roe_last_error_message();
        assert!(!err_ptr.is_null(), "invalid doc_type should set last_error");
        let err_msg = unsafe { CStr::from_ptr(err_ptr) }.to_str().unwrap().to_string();
        assert!(err_msg.contains("roe_new_document"), "error message should mention roe_new_document, got: {err_msg}");
        assert!(err_msg.contains("999"), "error message should mention the bad doc_type value, got: {err_msg}");
    }

    #[test]
    fn test_new_document_writer_calc_impress_have_distinct_handles() {
        let h1 = roe_new_document(1);
        let h2 = roe_new_document(2);
        let h3 = roe_new_document(3);
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);
        assert_ne!(h1, h3);
        handle::remove(h1);
        handle::remove(h2);
        handle::remove(h3);
    }
}

#[cfg(test)]
mod tests_save_document {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use roe_model::calc::{CellBlock, CellValue, SparseColumn, SparseGrid, Sheet, Workbook};
    use crate::handle::DocumentEntry;

    fn open(doc: RoeDocument) -> u64 {
        handle::insert(DocumentEntry {
            doc,
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: handle::WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn make_calc_with_data() -> RoeDocument {
        let mut grid = SparseGrid::default();
        grid.cols.insert(0, SparseColumn {
            blocks: vec![CellBlock {
                start_row: 0,
                values: vec![CellValue::Text("save_document 测试".into())].into(),
            }].into(),
        });
        RoeDocument::Calc(Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Sheet1".into(),
                cells: grid,
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats: std::collections::HashMap::new(),
                cond_formats: vec![],
                data_validations: vec![],
                auto_filter: None,
                merged_cells: vec![],
                pivot:        None,
                ..Default::default()
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        })
    }

    extern "C" fn collect_callback(data: *const u8, len: usize, userdata: *mut std::ffi::c_void) {
        let slice = unsafe { std::slice::from_raw_parts(data, len) };
        let out: &mut Vec<u8> = unsafe { &mut *(userdata as *mut Vec<u8>) };
        out.extend_from_slice(slice);
    }

    #[test]
    fn test_save_sai_contains_real_data() {
        let h = open(make_calc_with_data());
        let mut collected: Vec<u8> = Vec::new();

        let code = roe_save_document_to_bytes(
            h,
            2, // Sai
            collect_callback,
            &mut collected as *mut Vec<u8> as *mut std::ffi::c_void,
        );
        assert_eq!(code, RoeErrorCode::Ok as i32);
        assert!(!collected.is_empty(), "保存后的字节不应为空");

        // 解压验证内容不是 minimal_fods 占位文档，而是真实写出的数据
        // v188起：KAI 是 4-part 拼接（mimetype+manifest+content+styles），
        // 用 unpack_zstd_sai 解出 content_xml 再断言，而不是直接当单一 XML 处理。
        let (_mime, _manifest, xml, _styles) = roe_parser::odf::odf_package_writer::unpack_zstd_sai(&collected)
            .expect("KAI sai 4-part 解包失败");

        assert!(xml.contains("save_document 测试"), "导出的 XML 应包含实际单元格文本，实际内容：{xml}");
        assert!(!xml.contains("<office:spreadsheet/>"), "不应再是 minimal_fods 的空壁纸文档");

        handle::remove(h);
    }

    #[test]
    fn test_save_unsupported_format_hint() {
        let h = open(make_calc_with_data());
        let mut collected: Vec<u8> = Vec::new();
        let code = roe_save_document_to_bytes(
            h, 4, collect_callback,
            &mut collected as *mut Vec<u8> as *mut std::ffi::c_void,
        );
        assert_eq!(code, RoeErrorCode::Unsupported as i32);
        handle::remove(h);
    }

    fn make_writer_with_text() -> RoeDocument {
        use roe_model::writer::{WriterDocument, BodyElement, Paragraph, Run};
        let mut doc = WriterDocument::default();
        doc.body.push(BodyElement::Paragraph(Paragraph {
            runs: vec![Run {
                text: "P3验证文字".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }));
        RoeDocument::Writer(doc)
    }

    fn make_impress_with_shape() -> RoeDocument {
        use roe_model::impress::{Presentation, Slide, Shape, ShapeGeometry, ShapeStyle, ShapeContent};
        use roe_model::writer::{Paragraph, Run};
        let slide = Slide { layout_idx: None,
            shapes: vec![Shape {
                id: 1,
                geometry: ShapeGeometry::Rect,
                position: roe_model::common::Transform::from_rect(10.0, 10.0, 100.0, 50.0),
                style: ShapeStyle::default(),
                content: ShapeContent::Text(vec![Paragraph {
                    runs: vec![Run {
                        text: "P3演示验证".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }]),
                text_body: Default::default(),
                placeholder: None,
                end_cxn: None,
                st_cxn: None,
            }],
            master_idx: None,
            animation_tree: Default::default(),
            transition: None,
            notes: None,
            background: None,
            layout_shapes: vec![],
        };
        RoeDocument::Impress(Presentation {
            styles: roe_model::style::StyleRegistry::default(),
            slide_size: (960.0, 540.0),
            slides: vec![slide],
            master_slides: vec![], theme_colors: Default::default(), default_text_style: Default::default() })
    }

    #[test]
    fn test_save_wai_contains_real_data() {
        let h = open(make_writer_with_text());
        let mut collected: Vec<u8> = Vec::new();

        let code = roe_save_document_to_bytes(
            h, 1, // Wai
            collect_callback,
            &mut collected as *mut Vec<u8> as *mut std::ffi::c_void,
        );
        assert_eq!(code, RoeErrorCode::Ok as i32);
        assert!(!collected.is_empty());

        let (_mime, _manifest, xml, _styles) = roe_parser::odf::odf_package_writer_wai::unpack_zstd_wai(&collected)
            .expect("KAI wai 4-part 解包失败");

        assert!(xml.contains("P3验证文字"), "导出的 XML 应包含实际段落文字，实际内容：{xml}");
        assert!(!xml.contains("<office:text/>"), "不应再是 minimal_fodt 的空壁纸文档");

        handle::remove(h);
    }

    #[test]
    fn test_save_pai_contains_real_data() {
        let h = open(make_impress_with_shape());
        let mut collected: Vec<u8> = Vec::new();

        let code = roe_save_document_to_bytes(
            h, 3, // Pai
            collect_callback,
            &mut collected as *mut Vec<u8> as *mut std::ffi::c_void,
        );
        assert_eq!(code, RoeErrorCode::Ok as i32);
        assert!(!collected.is_empty());

        let (_mime, _manifest, xml, _styles) = roe_parser::odf::odf_package_writer_pai::unpack_zstd_pai(&collected)
            .expect("KAI pai 4-part 解包失败");

        assert!(xml.contains("P3演示验证"), "导出的 XML 应包含实际形状文字，实际内容：{xml}");
        assert!(!xml.contains("<office:presentation/>"), "不应再是 minimal_fodp 的空壁纸文档");

        handle::remove(h);
    }
}

#[cfg(test)]
mod tests_page_count {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use roe_model::calc::{SparseGrid, Sheet, Workbook};
    use roe_model::impress::*;
    use roe_model::common::{Color as ModelColor, Rect as ModelRect};
    use roe_parser::odf::fodt::FodtParser;
    use crate::handle::DocumentEntry;

    const SAMPLE_FODT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
    office:mimetype="application/vnd.oasis.opendocument.text">
  <office:styles>
    <style:style style:name="Default" style:family="paragraph">
      <style:text-properties fo:font-size="12pt"/>
    </style:style>
  </office:styles>
  <office:body>
    <office:text>
      <text:p text:style-name="Default">页数测试段落。</text:p>
    </office:text>
  </office:body>
</office:document>"#;

    fn open(doc: RoeDocument) -> u64 {
        handle::insert(DocumentEntry {
            doc,
            snapshot: None,
            is_shadow: false,
            events: std::collections::VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: handle::WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    #[test]
    fn test_page_count_writer() {
        let doc = FodtParser::parse(SAMPLE_FODT).expect("parse failed");
        let h = open(RoeDocument::Writer(doc));
        let n = roe_get_page_count(h);
        assert!(n >= 1, "writer should have at least 1 page, got {n}");
        handle::remove(h);
    }

    #[test]
    fn test_page_count_impress() {
        let pres = Presentation {
            styles: roe_model::style::StyleRegistry::default(),
            slide_size: (960.0, 540.0),
            slides: vec![
                Slide { layout_idx: None, shapes: vec![], master_idx: None,
                        animation_tree: AnimationNode::TimingRoot { children: vec![] },
                        transition: None, notes: None,
                        background: None, layout_shapes: vec![] },
                Slide { layout_idx: None, shapes: vec![], master_idx: None,
                        animation_tree: AnimationNode::TimingRoot { children: vec![] },
                        transition: None, notes: None,
                        background: None, layout_shapes: vec![] },
            ],
            master_slides: vec![],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        };
        let h = open(RoeDocument::Impress(pres));
        assert_eq!(roe_get_page_count(h), 2);
        handle::remove(h);
    }

    #[test]
    fn test_page_count_calc() {
        let wb = Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                name: "Sheet1".into(),
                cells: SparseGrid::default(),
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                ..Default::default()
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        };
        let h = open(RoeDocument::Calc(wb));
        assert_eq!(roe_get_page_count(h), -1);
        handle::remove(h);
    }

    #[test]
    fn test_page_count_invalid_handle() {
        let n = roe_get_page_count(999_999);
        // InvalidHandle = -2（取决于 RoeErrorCode 定义，只要 < 0 即可）
        assert!(n < 0, "invalid handle should return negative, got {n}");
    }
}

#[cfg(test)]
mod tests_headless_impl {
    use super::*;

    /// 新建 -> 保存 -> 重新打开 -> 关闭,三种文档各走一遍。
    #[test]
    fn roundtrip_new_save_open_close() {
        for (doc_type, hint) in [(1, 4), (2, 5), (3, 6)] {
            let h = new_document_impl(doc_type);
            assert_ne!(h, 0, "new doc_type {doc_type}");
            let bytes = save_document_ooxml_impl(h, hint)
                .unwrap_or_else(|e| panic!("save doc_type {doc_type}: {e}"));
            assert!(bytes.len() > 4 && &bytes[..2] == b"PK", "应为 zip 包");
            let h2 = open_document_bytes_impl(&bytes, hint);
            assert_ne!(h2, 0, "reopen doc_type {doc_type}");
            assert_ne!(h, h2);
            assert!(close_document_impl(h));
            assert!(close_document_impl(h2));
        }
    }

    #[test]
    fn close_twice_second_is_false() {
        let h = new_document_impl(2);
        assert!(close_document_impl(h));
        assert!(!close_document_impl(h));
    }

    #[test]
    fn save_invalid_handle_errs() {
        assert!(save_document_ooxml_impl(987_654_321, 5).is_err());
    }

    #[test]
    fn save_hint_mismatch_errs() {
        let h = new_document_impl(2); // Calc
        assert!(save_document_ooxml_impl(h, 4).is_err()); // docx 提示配 Calc
        assert!(close_document_impl(h));
    }

    #[test]
    fn open_garbage_returns_zero() {
        assert_eq!(open_document_bytes_impl(b"not a real file", 5), 0);
        assert_eq!(open_document_bytes_impl(b"x", 12345), 0);
    }
}

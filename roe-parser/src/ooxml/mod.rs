pub mod chart_common;
pub mod chart_writer;
pub mod chartex_writer;
pub mod chartex;
pub mod docx;
pub mod docx_writer;
pub mod xlsx;
pub mod xlsx_writer;
pub mod xlsx_pivot_writer;
pub mod xlsx_drawing_writer;
pub mod xlsx_styles_writer;
pub mod pptx;
pub mod pptx_writer;
pub mod pivot;

/// 从文件头字节嗅探图片 MIME 类型。PNG/JPEG/GIF/BMP 已知格式命中，
/// 未知格式兜底 "image/png"（DEVROOT-002：从 docx.rs 提升为跨
/// ooxml 子模块共享的公共函数，供 xlsx.rs 的 floating_image 解析
/// 复用，避免出现两份漂移的判断逻辑）。
pub(crate) fn sniff_image_mime(data: &[u8]) -> String {
    if data.starts_with(b"\x89PNG") {
        "image/png"
    } else if data.starts_with(b"\xff\xd8") {
        "image/jpeg"
    } else if data.starts_with(b"GIF") {
        "image/gif"
    } else if data.starts_with(b"BM") {
        "image/bmp"
    } else {
        "image/png" // 兜底
    }.to_string()
}
pub mod latex_omml;

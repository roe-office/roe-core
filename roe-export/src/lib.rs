//! roe-export：文档导出层
//!
//! - [`pdf`]：Hybrid PDF（PDF/A-3 + 内嵌 ODF 附件）
//! - [`html`]：Impress HTML 导出（浏览器可播放 + ROE 可逆向）

pub mod pdf;
pub mod html;

pub use pdf::PdfExporter;
pub use html::HtmlExporter;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("pdf error: {0}")]
    Pdf(String),
    #[error("html error: {0}")]
    Html(String),
    #[error("model error: {0}")]
    Model(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

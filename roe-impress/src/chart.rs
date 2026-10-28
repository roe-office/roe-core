//! 图表渲染公共接口，供 roe-api 调用。
//!
//! 实际的 ChartModel → SVG → 像素 渲染逻辑已下沉到
//! `roe_core::chart_render`（唯一实现，Writer/Impress/Calc 共用）。
//! 这里只负责编码为 PNG 并保持对外接口不变。

use roe_model::impress::ChartModel;

pub fn render_chart_to_png(model: &ChartModel, width: u32, height: u32) -> Option<Vec<u8>> {
    let svg = roe_core::chart_render::chart_model_to_svg(model, width, height)?;
    let mut pixmap = roe_core::chart_render::svg_to_pixmap(&svg, width, height)?;
    roe_core::chart_render::draw_chart_border(&mut pixmap, width, height, model);
    pixmap.encode_png().ok()
}

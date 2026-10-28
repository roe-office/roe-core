//! 占位符虚线框默认提示文案（产品需求，非bug修复）。
//!
//! 背景：PowerPoint/OnlyOffice等编辑器在渲染"空占位符"（用户未填写
//! 任何实际内容）时，会显示一圈虚线框+一句提示文字（如"幻灯片文本"、
//! "幻灯片标题"），引导用户点击输入。这段提示文字不存在于pptx文件
//! 本身（<a:p>段落里只有<a:endParaRPr>没有<a:r><a:t>），是编辑器应用
//! 层的本地化资源——本次调研已在OnlyOffice源码
//! （web-apps/apps/presentationeditor/main/locale/zh.json）里核实
//! 了对应的中文文案，ROE维护一份等价的映射表。
//!
//! 设计要点（详见 占位符虚线框默认提示文案 设计方案 v1）：
//! - 判空规则：ShapeContent::Text(paragraphs)里所有Run.text经trim
//!   后皆为空，视为"空占位符"。layout/master定义的具体默认文案
//!   在解析阶段就已经写入了真实Run.text，不会落入这条判空规则，
//!   因此不需要额外字段区分"来自layout的默认文案"和"用户真实输入"。
//! - dgm（SmartArt/DrawingML diagram）类型不给专属文案，统一走
//!   兜底"幻灯片文本"：ROE目前没有SmartArt渲染能力，提示文案不应该
//!   先于实际渲染能力存在。
//! - 语言固定中文，不做多语言可配置。

/// 根据占位符类型（OOXML `<p:ph type="...">`）返回兜底提示文案。
///
/// `ph_type` 为 `None` 时（`<p:ph>` 无 type 属性）按 OOXML 规范
/// 视为 body 类型，走兜底文案。
///
/// 已覆盖 11 种有专属文案的类型：title/ctrTitle/subTitle/pic/tbl/
/// chart/clipArt/media/dt/ftr/hdr/sldNum。
/// 其余（body/obj/sldImg/dgm 及 None）统一返回兜底"幻灯片文本"。
pub fn hint_text_for_ph_type(ph_type: Option<&str>) -> &'static str {
    match ph_type {
        Some("title") | Some("ctrTitle") => "幻灯片标题",
        Some("subTitle") => "幻灯片副标题",
        Some("pic") => "图片",
        Some("tbl") => "表格",
        Some("chart") => "图表",
        Some("clipArt") => "剪贴画",
        Some("media") => "媒体",
        Some("dt") => "日期和时间",
        Some("ftr") => "页脚",
        Some("hdr") => "页眉",
        Some("sldNum") => "幻灯片编号",
        // body / obj / sldImg / dgm(SmartArt，暂无渲染能力) / None
        // 统一兜底
        _ => "幻灯片文本",
    }
}

/// 判断一个占位符 Shape 是否为"空"（未填写任何实际内容），
/// 应当渲染虚线框+兜底提示文案。
///
/// 只对 `ShapeContent::Text` 变体有意义——Image/Table/Chart/Group
/// 这几个变体本身就是具体内容类型，不适用"空壳待填充"语义，一律
/// 返回 false（不触发提示渲染）。
///
/// 调用方需先确认 `shape.placeholder.is_some()`（是占位符）再调用
/// 本函数；本函数本身不检查 placeholder 字段，只负责判断内容是否
/// 为空，两个条件在 render_shape 接入点组合使用。
pub fn is_placeholder_empty(shape: &roe_model::impress::Shape) -> bool {
    match &shape.content {
        roe_model::impress::ShapeContent::Text(paragraphs) => paragraphs
            .iter()
            .all(|p| p.runs.iter().all(|r| r.text.trim().is_empty())),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::common::{ShapeGeometry, ShapeStyle, ImageData, Transform};
    use roe_model::impress::{Shape, ShapeContent, TextBodyProps};
    use roe_model::writer::{Paragraph, Run};

    #[test]
    fn hint_text_covers_all_named_types() {
        assert_eq!(hint_text_for_ph_type(Some("title")), "幻灯片标题");
        assert_eq!(hint_text_for_ph_type(Some("ctrTitle")), "幻灯片标题");
        assert_eq!(hint_text_for_ph_type(Some("subTitle")), "幻灯片副标题");
        assert_eq!(hint_text_for_ph_type(Some("pic")), "图片");
        assert_eq!(hint_text_for_ph_type(Some("tbl")), "表格");
        assert_eq!(hint_text_for_ph_type(Some("chart")), "图表");
        assert_eq!(hint_text_for_ph_type(Some("clipArt")), "剪贴画");
        assert_eq!(hint_text_for_ph_type(Some("media")), "媒体");
        assert_eq!(hint_text_for_ph_type(Some("dt")), "日期和时间");
        assert_eq!(hint_text_for_ph_type(Some("ftr")), "页脚");
        assert_eq!(hint_text_for_ph_type(Some("hdr")), "页眉");
        assert_eq!(hint_text_for_ph_type(Some("sldNum")), "幻灯片编号");
    }

    #[test]
    fn hint_text_falls_back_for_untyped_and_unmapped() {
        assert_eq!(hint_text_for_ph_type(None), "幻灯片文本");
        assert_eq!(hint_text_for_ph_type(Some("body")), "幻灯片文本");
        assert_eq!(hint_text_for_ph_type(Some("obj")), "幻灯片文本");
        assert_eq!(hint_text_for_ph_type(Some("sldImg")), "幻灯片文本");
        assert_eq!(hint_text_for_ph_type(Some("dgm")), "幻灯片文本");
        assert_eq!(hint_text_for_ph_type(Some("totally_unknown_type")), "幻灯片文本");
    }

    fn empty_run() -> Run {
        Run { text: String::new(), ..Default::default() }
    }

    fn whitespace_run() -> Run {
        Run { text: "   \n\t  ".to_string(), ..Default::default() }
    }

    fn text_run(s: &str) -> Run {
        Run { text: s.to_string(), ..Default::default() }
    }

    fn para(runs: Vec<Run>) -> Paragraph {
        Paragraph { runs, ..Default::default() }
    }

    fn make_shape(content: ShapeContent) -> Shape {
        Shape {
            id: 1,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(0.0, 0.0, 100.0, 100.0),
            style: ShapeStyle::default(),
            content,
            text_body: TextBodyProps::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        }
    }

    #[test]
    fn empty_text_paragraphs_is_empty() {
        let shape = make_shape(ShapeContent::Text(vec![]));
        assert!(is_placeholder_empty(&shape));
    }

    #[test]
    fn text_with_only_empty_runs_is_empty() {
        let shape = make_shape(ShapeContent::Text(vec![para(vec![empty_run()])]));
        assert!(is_placeholder_empty(&shape));
    }

    #[test]
    fn text_with_only_whitespace_runs_is_empty() {
        let shape = make_shape(ShapeContent::Text(vec![para(vec![whitespace_run()])]));
        assert!(is_placeholder_empty(&shape));
    }

    #[test]
    fn text_with_actual_content_is_not_empty() {
        let shape = make_shape(ShapeContent::Text(vec![para(vec![text_run("HELLO")])]));
        assert!(!is_placeholder_empty(&shape));
    }

    #[test]
    fn text_with_mixed_empty_and_content_runs_is_not_empty() {
        let shape = make_shape(ShapeContent::Text(vec![para(vec![
            empty_run(),
            text_run("real content"),
        ])]));
        assert!(!is_placeholder_empty(&shape));
    }

    #[test]
    fn non_text_content_is_never_empty() {
        let img_shape = make_shape(ShapeContent::Image(ImageData::default()));
        assert!(!is_placeholder_empty(&img_shape));
    }
}

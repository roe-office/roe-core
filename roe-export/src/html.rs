//! Impress HTML 导出
//!
//! 输出：单一 HTML5 文件
//!   - <head> 内嵌 <script type="application/roe+json">：完整 PresentationModel JSON
//!   - <body> 每张幻灯片一个 <section class="roe-slide">
//!   - 动画用 Web Animations API（WAAPI）实现
//!   - 翻页：键盘左右箭头 + 点击
//!
//! ROE 打开此 HTML 时：
//!   1. 解析 <script type="application/roe+json"> → PresentationModel
//!   2. 完整还原内存模型，可再导出为 PPTX / FODP / PAI

use crate::ExportError;
use roe_model::impress::Presentation;

pub struct HtmlExporter;

impl HtmlExporter {
    /// 导出为单一 HTML5 字符串
    pub fn export(model: &Presentation) -> Result<String, ExportError> {
        let model_json = serde_json::to_string(model)
            .map_err(|e| ExportError::Html(e.to_string()))?;

        let slides_html = render_slides_html(model);
        let animations_js = render_animations_js(model);

        let html = format!(
            r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>ROE Presentation</title>
<script type="application/roe+json">
{model_json}
</script>
<style>
{SLIDE_CSS}
</style>
</head>
<body>
<div id="roe-deck">
{slides_html}
</div>
<script>
{PLAYER_JS}
{animations_js}
</script>
</body>
</html>"#
        );

        Ok(html)
    }
}

/// 把每张幻灯片渲染成 HTML section
fn render_slides_html(model: &Presentation) -> String {
    let mut out = String::new();
    for (i, slide) in model.slides.iter().enumerate() {
        let (w, h) = model.slide_size;
        out.push_str(&format!(
            r#"<section class="roe-slide" data-slide="{i}" style="width:{w}px;height:{h}px;">"#
        ));
        for shape in &slide.shapes {
            out.push_str(&render_shape_html(shape));
        }
        out.push_str("</section>\n");
    }
    out
}

fn render_shape_html(shape: &roe_model::impress::Shape) -> String {
    use roe_model::impress::ShapeContent;
    let r = &shape.position;
    let style = format!(
        "position:absolute;left:{:.1}px;top:{:.1}px;width:{:.1}px;height:{:.1}px;",
        r.x(), r.y(), r.w(), r.h()
    );
    let id_attr = format!("data-shape-id=\"{:?}\"", shape.id);

    match &shape.content {
        ShapeContent::Text(paras) => {
            let text: String = paras.iter().map(|p| {
                let t: String = p.runs.iter().map(|r| {
                    html_escape(&r.text)
                }).collect();
                format!("<p>{t}</p>")
            }).collect();
            format!("<div class=\"roe-shape roe-text\" {id_attr} style=\"{style}\">{text}</div>\n")
        }
        ShapeContent::Image(_) => {
            format!("<div class=\"roe-shape roe-image\" {id_attr} style=\"{style}\"></div>\n")
        }
        ShapeContent::Empty => {
            format!("<div class=\"roe-shape\" {id_attr} style=\"{style}\"></div>\n")
        }
        ShapeContent::Chart(_) | ShapeContent::StatChart(_) => {
            format!("<div class=\"roe-shape roe-chart\" {id_attr} style=\"{style}\"></div>\n")
        }
        ShapeContent::Table(_) => {
            // TODO: PPTX-6 Impress 表格渲染，本次仅打通模型层
            format!("<div class=\"roe-shape roe-table\" {id_attr} style=\"{style}\"></div>\n")
        }
        ShapeContent::Group { children, .. } => {
            // PPTX-9：分组暂不做子形状递归展开（HTML导出链路本身其余
            // 分支均为浅层占位，未展开真实内容，见 Table 分支同类做法）。
            // TODO: 子 Shape 需要相对分组自身变换定位后递归调用
            // render_shape_html，本次(9a)仅打通模型+渲染层，HTML导出
            // 递归展开留待后续任务，此处占位保证子内容不丢失分组标记。
            let _ = children;
            format!("<div class=\"roe-shape roe-group\" {id_attr} style=\"{style}\"></div>\n")
        }
    }
}

/// 从动画树生成 WAAPI JS 代码（当前输出骨架，动画细节后续填充）
fn render_animations_js(model: &Presentation) -> String {
    let mut js = String::from("// ROE Animation Sequences\nconst roeAnimations = [\n");
    for (i, _slide) in model.slides.iter().enumerate() {
        js.push_str(&format!("  [], // slide {i}\n"));
    }
    js.push_str("];\n");
    js
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}

// ── 内嵌 CSS ──────────────────────────────────────────────────────────────

const SLIDE_CSS: &str = r#"
* { box-sizing: border-box; margin: 0; padding: 0; }
body { background: #1a1a1a; display: flex; justify-content: center;
       align-items: center; height: 100vh; overflow: hidden; }
#roe-deck { position: relative; }
.roe-slide { position: absolute; top: 0; left: 0; background: white;
             overflow: hidden; display: none; }
.roe-slide.active { display: block; }
.roe-shape { position: absolute; }
.roe-text { font-family: sans-serif; font-size: 16px; overflow: hidden; }
"#;

// ── 内嵌播放器 JS ─────────────────────────────────────────────────────────

const PLAYER_JS: &str = r#"
(function() {
  const deck = document.getElementById('roe-deck');
  const slides = Array.from(deck.querySelectorAll('.roe-slide'));
  let current = 0;

  function goTo(n) {
    if (n < 0 || n >= slides.length) return;
    slides[current].classList.remove('active');
    current = n;
    slides[current].classList.add('active');
    playAnimations(current);
  }

  function playAnimations(idx) {
    const anims = (typeof roeAnimations !== 'undefined') ? roeAnimations[idx] : [];
    (anims || []).forEach(a => {
      const el = deck.querySelector(`[data-shape-id="${a.shapeId}"]`);
      if (el) el.animate(a.keyframes, a.options);
    });
  }

  document.addEventListener('keydown', e => {
    if (e.key === 'ArrowRight' || e.key === ' ') goTo(current + 1);
    if (e.key === 'ArrowLeft') goTo(current - 1);
  });
  deck.addEventListener('click', () => goTo(current + 1));

  // 缩放适配窗口
  function resize() {
    if (!slides.length) return;
    const slide = slides[0];
    const sw = slide.offsetWidth || 960;
    const sh = slide.offsetHeight || 540;
    const scale = Math.min(window.innerWidth / sw, window.innerHeight / sh);
    deck.style.transform = `scale(${scale})`;
    deck.style.width = sw + 'px';
    deck.style.height = sh + 'px';
  }
  window.addEventListener('resize', resize);
  resize();
  goTo(0);
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::impress::*;
    use roe_model::common::{Transform, StyleRef};

    fn make_presentation() -> Presentation {
        Presentation {
            slide_size: (960.0, 540.0),
            styles: roe_model::style::StyleRegistry::default(),
            slides: vec![
                Slide { layout_idx: None,
                    shapes: vec![
                        Shape {
                            id: 1,
                            geometry: ShapeGeometry::Rect,
                            position: Transform::from_rect(100.0, 80.0, 760.0, 120.0),
                            style: ShapeStyle::default(),
                            placeholder: None,
                            content: ShapeContent::Text(vec![
                                roe_model::writer::Paragraph {
                                    style: StyleRef("Title".into()),
                                    runs: vec![roe_model::writer::Run { text: "Hello ROE".into(), char_style: None, ..Default::default() }],
                                    list_info: None,
                                    ..Default::default()
                                }
                            ]),
                            text_body: Default::default(),
                            st_cxn: None,
                            end_cxn: None,
                        }
                    ],
                    background: Default::default(),
                    layout_shapes: vec![],
                    master_idx: None,
                    animation_tree: AnimationNode::TimingRoot { children: vec![] },
                    notes: None,
                    transition: None,
                },
            ],
            master_slides: vec![],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        }
    }

    #[test]
    fn test_html_export_structure() {
        let model = make_presentation();
        let html = HtmlExporter::export(&model).unwrap();
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("application/roe+json"));
        assert!(html.contains("roe-slide"));
        assert!(html.contains("Hello ROE"));
    }

    #[test]
    fn test_json_roundtrip_present_in_html() {
        let model = make_presentation();
        let html = HtmlExporter::export(&model).unwrap();
        // JSON 应该包含 slide_size
        assert!(html.contains("slide_size"));
        assert!(html.contains("960"));
    }

    #[test]
    fn test_save_html_to_tmp() {
        let model = make_presentation();
        let html = HtmlExporter::export(&model).unwrap();
        std::fs::write("/tmp/roe_presentation.html", &html).unwrap();
        println!("saved to /tmp/roe_presentation.html ({} bytes)", html.len());
    }
}

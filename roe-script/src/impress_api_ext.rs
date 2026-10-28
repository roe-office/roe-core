//! Impress Rhai 扩展(P0): inspect_presentation。
//! 作为 impress_api 的子模块加载(#[path]),可直接使用父模块私有的 with_pres。
//! 新函数写在这里,不改 impress_api.rs 里已有函数。
//! 失败语义:没有打开演示文稿时返回以 "err:" 开头的字符串。

use rhai::Engine;
use roe_model::common::ShapeGeometry;
use roe_model::impress::ShapeContent;

use super::with_pres;

const TEXT_PREVIEW_CHARS: usize = 40;

fn geometry_name(g: &ShapeGeometry) -> &'static str {
    match g {
        ShapeGeometry::Rect => "rect",
        ShapeGeometry::RoundRect { .. } => "round_rect",
        ShapeGeometry::Ellipse => "ellipse",
        ShapeGeometry::Triangle => "triangle",
        ShapeGeometry::Line => "line",
        ShapeGeometry::PresetPath(_) => "preset_path",
        ShapeGeometry::Custom(_) => "custom",
    }
}

fn r2(v: f32) -> f64 {
    ((v as f64) * 100.0).round() / 100.0
}

/// 只读函数集(纯查询)。由 register 与 impress_api::register_readonly 共用。
pub fn register_read(engine: &mut Engine) {
    use roe_model::common::CellContent;
    register_chart_read(engine);
    register_style_read(engine);
    // inspect_presentation: 一次返回整份演示文稿的结构 JSON。
    // 每个形状同时给 index(当前下标,删除形状后会变)和 id(稳定),
    // 后续新函数一律以 id 为准。文字只给前 40 字符 + 总长度。
    engine.register_fn("inspect_presentation", || -> String {
        with_pres(|p| {
            let slides: Vec<serde_json::Value> = p.slides.iter().enumerate().map(|(si, s)| {
                let shapes: Vec<serde_json::Value> = s.shapes.iter().enumerate().map(|(hi, sh)| {
                    let mut obj = serde_json::json!({
                        "index": hi,
                        "id": sh.id,
                        "geometry": geometry_name(&sh.geometry),
                        "x": r2(sh.position.x()),
                        "y": r2(sh.position.y()),
                        "w": r2(sh.position.w()),
                        "h": r2(sh.position.h()),
                        "rotation": r2(sh.position.rotation.unwrap_or(0.0)),
                        "placeholder": sh.placeholder.as_ref()
                            .map(|ph| ph.ph_type.clone().unwrap_or_else(|| "body".to_string())),
                    });
                    let m = obj.as_object_mut().unwrap();
                    match &sh.content {
                        ShapeContent::Text(paras) => {
                            let text: String = paras.iter()
                                .flat_map(|pa| pa.runs.iter())
                                .map(|r| r.text.as_str())
                                .collect::<Vec<_>>()
                                .join("");
                            let total = text.chars().count();
                            let preview: String = text.chars().take(TEXT_PREVIEW_CHARS).collect();
                            m.insert("type".to_string(), serde_json::json!("text"));
                            m.insert("text".to_string(), serde_json::json!(preview));
                            m.insert("text_len".to_string(), serde_json::json!(total));
                            m.insert("text_truncated".to_string(), serde_json::json!(total > TEXT_PREVIEW_CHARS));
                            m.insert("paragraphs".to_string(), serde_json::json!(paras.len()));
                        }
                        ShapeContent::Image(img) => {
                            m.insert("type".to_string(), serde_json::json!("image"));
                            m.insert("mime".to_string(), serde_json::json!(img.mime));
                        }
                        ShapeContent::Table(table) => {
                            m.insert("type".to_string(), serde_json::json!("table"));
                            m.insert("rows".to_string(), serde_json::json!(table.rows.len()));
                            m.insert("cols".to_string(), serde_json::json!(
                                table.rows.first().map(|r| r.cells.len()).unwrap_or(0)));
                        }
                        ShapeContent::Chart(chart) => {
                            m.insert("type".to_string(), serde_json::json!("chart"));
                            m.insert("kind".to_string(), serde_json::json!(format!("{:?}", chart.kind)));
                            m.insert("title".to_string(), serde_json::json!(chart.title));
                            m.insert("series_count".to_string(), serde_json::json!(chart.series.len()));
                        }
                        ShapeContent::StatChart(sc) => {
                            m.insert("type".to_string(), serde_json::json!("stat_chart"));
                            m.insert("kind".to_string(), serde_json::json!(format!("{:?}", sc.kind)));
                            m.insert("title".to_string(), serde_json::json!(sc.title));
                            m.insert("series_count".to_string(), serde_json::json!(sc.series.len()));
                        }
                        ShapeContent::Group { children, .. } => {
                            m.insert("type".to_string(), serde_json::json!("group"));
                            m.insert("children_ids".to_string(), serde_json::json!(
                                children.iter().map(|c| c.id).collect::<Vec<_>>()));
                        }
                        ShapeContent::Empty => {
                            m.insert("type".to_string(), serde_json::json!("empty"));
                        }
                    }
                    obj
                }).collect();
                serde_json::json!({
                    "index": si,
                    "master_idx": s.master_idx,
                    "layout_idx": s.layout_idx,
                    "has_notes": s.notes.as_ref().map(|n| !n.is_empty()).unwrap_or(false),
                    "shape_count": s.shapes.len(),
                    "shapes": shapes,
                })
            }).collect();
            serde_json::json!({
                "slide_count": p.slides.len(),
                "master_count": p.master_slides.len(),
                "slide_size": { "w": r2(p.slide_size.0), "h": r2(p.slide_size.1) },
                "slides": slides,
            }).to_string()
        })
        .unwrap_or_else(|| "err: 没有打开的演示文稿".to_string())
    });

    // list_layouts: 列出该页所用母版的全部版式(JSON 数组:index/name/kind/placeholders/current)。
    // current=true 表示该页当前使用的版式。
    engine.register_fn("list_layouts", |slide: i64| -> String {
        with_pres(|p| {
            if slide < 0 || slide as usize >= p.slides.len() {
                return format!("err: slide {} 越界(共 {} 页)", slide, p.slides.len());
            }
            let sl = &p.slides[slide as usize];
            let mi = sl.master_idx.unwrap_or(0);
            let cur = sl.layout_idx;
            let arr: Vec<serde_json::Value> = match p.master_slides.get(mi) {
                Some(m) => m.layouts.iter().enumerate().map(|(i, l)| serde_json::json!({
                    "index": i,
                    "name": l.name,
                    "kind": format!("{:?}", l.kind),
                    "placeholders": l.shapes.iter().filter(|x| x.placeholder.is_some()).count(),
                    "current": cur == Some(i),
                })).collect(),
                None => Vec::new(),
            };
            serde_json::Value::Array(arr).to_string()
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });

    // get_shape_rect: 读位置/尺寸/旋转/翻转(pt)。出错返回 "err:..."。
    engine.register_fn("get_shape_rect", |slide: i64, shape_id: i64| -> String {
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => {
                let t = &p.slides[si].shapes[hi].position;
                serde_json::json!({
                    "id": shape_id,
                    "x": r2(t.offset.0), "y": r2(t.offset.1),
                    "w": r2(t.extent.0), "h": r2(t.extent.1),
                    "rotation": r2(t.rotation.unwrap_or(0.0)),
                    "flip_h": t.flip_h.unwrap_or(false),
                    "flip_v": t.flip_v.unwrap_or(false),
                }).to_string()
            }
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });

    // get_shape_text_by_id: 按 id 读文本形状文字(多段用 \n 连接)。非文本形状返回 err:。
    engine.register_fn("get_shape_text_by_id", |slide: i64, shape_id: i64| -> String {
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => match &p.slides[si].shapes[hi].content {
                ShapeContent::Text(paras) => paras_text(paras),
                _ => format!("err: id={} 的形状不是文本形状", shape_id),
            },
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });

    // get_shape_table_cell: 读表格单元格文字。
    engine.register_fn("get_shape_table_cell", |slide: i64, shape_id: i64, row: i64, col: i64| -> String {
        with_pres(|p| match table_cell_mut(p, slide, shape_id, row, col) {
            Ok(cell) => {
                if cell.hidden {
                    return "err: 该单元格是合并单元格的延续格,请读合并区域的起始格".to_string();
                }
                cell.content.iter().filter_map(|c| match c {
                    CellContent::Paragraph(pa) => Some(pa.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("")),
                    _ => None,
                }).collect::<Vec<_>>().join("\n")
            }
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });

}

pub fn register(engine: &mut Engine) {
    register_read(engine);
    register_geom(engine);
    register_content(engine);
    register_style(engine);
    register_layout(engine);
    register_style_range(engine);
    register_image(engine);
    register_chart(engine);
}

// ───────── 几何与层级(P1):按 shape id 定位,目标值式 ─────────
// 说明:下面两个 reflow 函数复制自 roe-api/src/impress_geometry.rs(roe-script 不能依赖 roe-api),
// 改动其一时请同步另一份;日后可统一搬进 roe-model。

fn reflow_path_data(data: &mut roe_model::common::PresetPathData, old_w: f32, old_h: f32, new_w: f32, new_h: f32) {
    use roe_model::common::PathCommand as P;
    if (old_w - new_w).abs() < 1e-4 && (old_h - new_h).abs() < 1e-4 {
        return;
    }
    if !data.preset_name.is_empty() {
        if let Some(formula) = roe_model::preset_geometry::find_preset_shape(&data.preset_name) {
            let cmds = {
                let adj_map: std::collections::HashMap<&str, f64> =
                    data.adj_overrides.iter().map(|(n, v)| (n.as_str(), *v)).collect();
                roe_model::preset_geometry::eval_preset_shape(formula, new_w as f64, new_h as f64, &adj_map)
            };
            data.commands = cmds;
            return;
        }
    }
    if old_w <= 0.0 || old_h <= 0.0 {
        return;
    }
    let (sx, sy) = (new_w / old_w, new_h / old_h);
    for c in data.commands.iter_mut() {
        *c = match *c {
            P::MoveTo { x, y } => P::MoveTo { x: x * sx, y: y * sy },
            P::LineTo { x, y } => P::LineTo { x: x * sx, y: y * sy },
            P::CubicTo { x1, y1, x2, y2, x, y } => P::CubicTo { x1: x1 * sx, y1: y1 * sy, x2: x2 * sx, y2: y2 * sy, x: x * sx, y: y * sy },
            P::QuadTo { x1, y1, x, y } => P::QuadTo { x1: x1 * sx, y1: y1 * sy, x: x * sx, y: y * sy },
            P::ArcTo { cx, cy, rx, ry, start_deg, sweep_deg } => P::ArcTo { cx: cx * sx, cy: cy * sy, rx: rx * sx, ry: ry * sy, start_deg, sweep_deg },
            P::Close => P::Close,
        };
    }
}

fn reflow_geometry(shape: &mut roe_model::impress::Shape, old_w: f32, old_h: f32) {
    let (nw, nh) = shape.position.extent;
    if let ShapeContent::Table(t) = &mut shape.content {
        if old_w > 0.5 && old_h > 0.5 && nw > 0.0 && nh > 0.0 {
            let (kx, ky) = (nw / old_w, nh / old_h);
            for w in t.col_widths.iter_mut() { *w *= kx; }
            for r in t.rows.iter_mut() {
                if let Some(h) = r.row_height.as_mut() { *h *= ky; }
            }
        }
    }
    if let ShapeGeometry::PresetPath(data) = &mut shape.geometry {
        reflow_path_data(data, old_w, old_h, nw, nh);
    }
}

const NO_PRES: &str = "err: 没有打开的演示文稿";

/// 按 (页下标, 形状 id) 找到形状在页内的下标。只查页顶层形状。
fn locate(p: &roe_model::impress::Presentation, slide: i64, shape_id: i64) -> Result<(usize, usize), String> {
    if slide < 0 || slide as usize >= p.slides.len() {
        return Err(format!("err: slide {} 越界(共 {} 页)", slide, p.slides.len()));
    }
    let si = slide as usize;
    if shape_id < 0 {
        return Err(format!("err: shape_id {} 不存在(用 inspect_presentation() 查看各形状 id)", shape_id));
    }
    match p.slides[si].shapes.iter().position(|s| s.id as i64 == shape_id) {
        Some(hi) => Ok((si, hi)),
        None => Err(format!("err: 第 {} 页没有 id={} 的形状(用 inspect_presentation() 查看各形状 id)", slide, shape_id)),
    }
}

fn edit_shape(slide: i64, shape_id: i64, f: impl FnOnce(&mut roe_model::impress::Shape)) -> String {
    with_pres(|p| match locate(p, slide, shape_id) {
        Ok((si, hi)) => { f(&mut p.slides[si].shapes[hi]); "ok".to_string() }
        Err(e) => e,
    })
    .unwrap_or_else(|| NO_PRES.to_string())
}

fn edit_order(slide: i64, shape_id: i64, f: impl FnOnce(&mut Vec<roe_model::impress::Shape>, usize)) -> String {
    with_pres(|p| match locate(p, slide, shape_id) {
        Ok((si, hi)) => { f(&mut p.slides[si].shapes, hi); "ok".to_string() }
        Err(e) => e,
    })
    .unwrap_or_else(|| NO_PRES.to_string())
}

// ───────── P1 样式:set_shape_fill / set_shape_stroke / set_shape_text_style ─────────
// 逻辑复制自 roe-api(roe-script 不能依赖 roe-api,改一处请同步另一处)。
fn parse_hex_rgb(s: &str) -> Option<roe_model::common::Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(roe_model::common::Color { r, g, b, a: 255 })
}

fn apply_run_style(mut runs: Vec<&mut roe_model::writer::Run>, cmd: &str) -> Result<(), String> {
    let cmd = cmd.trim();
    let (k, v) = match cmd.split_once(':') {
        Some((k, v)) => (k.trim(), v.trim()),
        None => (cmd, if matches!(cmd, "bold" | "italic" | "underline" | "strike") { "toggle" } else { "" }),
    };
    if runs.is_empty() {
        return Err("形状里没有可改样式的文字".to_string());
    }
    match k {
        "bold" | "italic" | "underline" | "strike" => {
            let get = |r: &roe_model::writer::Run| match k {
                "bold" => r.bold,
                "italic" => r.italic,
                "underline" => r.underline,
                _ => r.strikethrough,
            };
            let target = match v {
                "1" => true,
                "0" => false,
                "toggle" => !runs.iter().all(|r| get(&**r) == Some(true)),
                _ => return Err(format!("{} 的值必须是 1、0 或 toggle,收到 {}", k, v)),
            };
            for r in runs.iter_mut() {
                match k {
                    "bold" => r.bold = Some(target),
                    "italic" => r.italic = Some(target),
                    "underline" => {
                        r.underline = Some(target);
                        if !target { r.underline_style = None; }
                    }
                    _ => r.strikethrough = Some(target),
                }
            }
        }
        "script" => {
            use roe_model::writer::ScriptPosition;
            let want = match v {
                "super" => ScriptPosition::Superscript,
                "sub" => ScriptPosition::Subscript,
                _ => return Err(format!("script 的值必须是 super 或 sub,收到 {}", v)),
            };
            let all = runs.iter().all(|r| r.script == Some(want));
            for r in runs.iter_mut() { r.script = if all { None } else { Some(want) }; }
        }
        "size" => {
            let relative = v.starts_with('+') || v.starts_with('-');
            let n: f32 = v.parse().map_err(|_| format!("非法字号 {}", v))?;
            if !n.is_finite() { return Err(format!("非法字号 {}", v)); }
            for r in runs.iter_mut() {
                let cur = r.font_size.unwrap_or(18.0);
                let next = if relative { cur + n } else { n };
                r.font_size = Some(next.clamp(1.0, 400.0));
            }
        }
        "color" if v == "auto" => {
            for r in runs.iter_mut() { r.color = None; }
        }
        "color" => {
            let c = parse_hex_rgb(v).ok_or_else(|| format!("非法文字颜色 {}(用 #rrggbb 或 auto)", v))?;
            for r in runs.iter_mut() { r.color = Some(c); }
        }
        "background" => {
            if v == "none" {
                for r in runs.iter_mut() { r.background = None; }
            } else {
                let c = parse_hex_rgb(v).ok_or_else(|| format!("非法高亮颜色 {}(用 #rrggbb 或 none)", v))?;
                for r in runs.iter_mut() { r.background = Some(c); }
            }
        }
        "font" => {
            if v.is_empty() { return Err("字体名为空".to_string()); }
            for r in runs.iter_mut() {
                r.font_family = Some(v.to_string());
                r.font_family_ascii = Some(v.to_string());
            }
        }
        "clear" => {
            for r in runs.iter_mut() {
                r.bold = None; r.italic = None; r.underline = None; r.underline_style = None;
                r.strikethrough = None; r.font_size = None; r.font_family = None;
                r.font_family_ascii = None; r.color = None; r.background = None; r.script = None;
            }
        }
        _ => return Err(format!("未知文字样式 {}(支持 bold/italic/underline/strike/script/size/color/background/font/clear)", k)),
    }
    Ok(())
}

pub fn register_style(engine: &mut Engine) {
    // set_shape_fill: color 为 "#rrggbb" 设纯色填充,"none" 写入显式无填充(透明,Some(FillStyle::None))。
    // 注意不能写 Option::None:渲染器会把"没有填充记录"的普通形状画成主题 accent1 半透明色。
    engine.register_fn("set_shape_fill", |slide: i64, shape_id: i64, color: String| -> String {
        let c = color.trim().to_string();
        let fill = if c == "none" {
            Some(roe_model::common::FillStyle::None)
        } else {
            match parse_hex_rgb(&c) {
                Some(col) => Some(roe_model::common::FillStyle::Solid(col)),
                None => return format!("err: 非法填充颜色 {}(只认 #rrggbb 或 none,例:红 #FF0000、蓝 #0000FF、白 #FFFFFF、黑 #000000)", c),
            }
        };
        edit_shape(slide, shape_id, move |sh| { sh.style.fill = fill; })
    });

    // set_shape_stroke: color "" 不改颜色,"none" 清除描边(此时 width 被忽略),"#rrggbb" 设色;
    // width < 0 不改宽度。无描边时设宽度会用默认色 #2b579a。width 必须写小数,如 2.0。
    engine.register_fn("set_shape_stroke", |slide: i64, shape_id: i64, color: String, width: f64| -> String {
        if !width.is_finite() {
            return "err: width 必须是有限数(不改宽度请传 -1.0)".to_string();
        }
        let c = color.trim().to_string();
        let clear = c == "none";
        let new_color = if c.is_empty() || clear {
            None
        } else {
            match parse_hex_rgb(&c) {
                Some(col) => Some(col),
                None => return format!("err: 非法边框颜色 {}(只认 #rrggbb、none 或空字符串,例:红 #FF0000、蓝 #0000FF、白 #FFFFFF、黑 #000000)", c),
            }
        };
        edit_shape(slide, shape_id, move |sh| {
            if clear { sh.style.stroke = None; return; }
            if let Some(col) = new_color {
                let w = sh.style.stroke.as_ref().map(|x| x.width).unwrap_or(1.0);
                sh.style.stroke = Some(roe_model::common::StrokeStyle { color: col, width: w });
            }
            if width >= 0.0 {
                let w = width as f32;
                match sh.style.stroke.as_mut() {
                    Some(st) => st.width = w,
                    None => {
                        sh.style.stroke = Some(roe_model::common::StrokeStyle {
                            color: roe_model::common::Color { r: 0x2b, g: 0x57, b: 0x9a, a: 255 },
                            width: w,
                        });
                    }
                }
            }
        })
    });

    // set_shape_text_style: 改形状内全部文字的样式。cmd 形如 "bold:toggle" "size:+2" "color:#ff0000"。
    // 支持 bold/italic/underline/strike(1|0|toggle,可省略冒号=toggle)、size:24 或 size:+2、
    // color:#rrggbb|auto、background:#rrggbb|none、font:名、script:super|sub(切换)、clear。
    engine.register_fn("set_shape_text_style", |slide: i64, shape_id: i64, cmd: String| -> String {
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => match &mut p.slides[si].shapes[hi].content {
                ShapeContent::Text(paras) => {
                    let runs: Vec<&mut roe_model::writer::Run> =
                        paras.iter_mut().flat_map(|pa| pa.runs.iter_mut()).collect();
                    match apply_run_style(runs, &cmd) {
                        Ok(()) => "ok".to_string(),
                        Err(e) => format!("err: {}", e),
                    }
                }
                _ => "err: 该形状没有文字(只有文本形状可用)".to_string(),
            },
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });
}

// ───────── P1 版式:list_layouts / set_slide_layout / insert_slide_with_layout ─────────
// 逻辑复制自 roe-api/src/impress_slide_ops.rs(roe-script 不能依赖 roe-api,改一处请同步另一处)。
fn lay_ph_key(s: &roe_model::impress::Shape) -> Option<(String, u32)> {
    s.placeholder.as_ref().map(|p| (p.ph_type.clone().unwrap_or_default(), p.index.unwrap_or(0)))
}

fn lay_ph_family(ty: &str) -> &'static str {
    match ty {
        "ctrTitle" | "title" => "T",
        "subTitle" | "body" | "obj" | "" => "B",
        _ => "O",
    }
}

fn lay_text_empty(s: &roe_model::impress::Shape) -> bool {
    match &s.content {
        ShapeContent::Text(paras) => paras.iter().all(|p| p.runs.iter().map(|r| r.text.len()).sum::<usize>() == 0),
        _ => false,
    }
}

fn lay_insert_blank_after(pres: &mut roe_model::impress::Presentation, after: usize) -> usize {
    let (master_idx, background, layout_shapes, at) = if pres.slides.is_empty() {
        (None, None, Vec::new(), 0)
    } else {
        let r = after.min(pres.slides.len() - 1);
        let s = &pres.slides[r];
        (s.master_idx, s.background.clone(), s.layout_shapes.clone(), r + 1)
    };
    let slide = roe_model::impress::Slide {
        layout_idx: None,
        shapes: vec![],
        master_idx,
        animation_tree: Default::default(),
        transition: None,
        notes: None,
        background,
        layout_shapes,
    };
    pres.slides.insert(at, slide);
    at
}

fn lay_apply(pres: &mut roe_model::impress::Presentation, slide_idx: usize, layout_idx: usize) -> Result<(), String> {
    if slide_idx >= pres.slides.len() {
        return Err(format!("slide_idx={slide_idx} out of range"));
    }
    let master_idx = pres.slides[slide_idx].master_idx.unwrap_or(0);
    let master = pres.master_slides.get(master_idx).ok_or_else(|| format!("master_idx={master_idx} missing"))?;
    let layout = master.layouts.get(layout_idx).ok_or_else(|| format!("layout_idx={layout_idx} missing"))?;
    let layout_ph: Vec<roe_model::impress::Shape> = layout.shapes.iter().filter(|x| x.placeholder.is_some()).cloned().collect();
    let layout_deco: Vec<roe_model::impress::Shape> = layout.shapes.iter().filter(|x| x.placeholder.is_none()).cloned().collect();
    let layout_ids: std::collections::HashSet<u32> = layout_deco.iter().map(|x| x.id).collect();
    let mut cache: Vec<roe_model::impress::Shape> = master.shapes.iter()
        .filter(|x| x.placeholder.is_none() && !layout_ids.contains(&x.id))
        .cloned()
        .collect();
    if !layout.show_master_shapes {
        cache.clear();
    }
    cache.extend(layout_deco);
    let background = layout.background.clone().or_else(|| master.background.clone());

    let slide = &mut pres.slides[slide_idx];
    let mut used = vec![false; layout_ph.len()];
    let mut remove_ids: Vec<u32> = Vec::new();
    for sh in slide.shapes.iter_mut() {
        let (ty, ix) = match sh.placeholder.as_ref() {
            Some(info) => (info.ph_type.clone().unwrap_or_default(), info.index.unwrap_or(0)),
            None => continue,
        };
        let mut found = (0..layout_ph.len()).find(|&i| !used[i] && lay_ph_key(&layout_ph[i]) == Some((ty.clone(), ix)));
        if found.is_none() {
            found = (0..layout_ph.len()).find(|&i| {
                !used[i] && layout_ph[i].placeholder.as_ref().and_then(|p| p.ph_type.clone()).unwrap_or_default() == ty
            });
        }
        if found.is_none() {
            found = (0..layout_ph.len()).find(|&i| {
                !used[i] && lay_ph_family(&layout_ph[i].placeholder.as_ref().and_then(|p| p.ph_type.clone()).unwrap_or_default()) == lay_ph_family(&ty)
            });
        }
        match found {
            Some(i) => {
                used[i] = true;
                let l = &layout_ph[i];
                sh.position = l.position.clone();
                if let ShapeContent::Text(_) = &sh.content {
                    sh.text_body = l.text_body.clone();
                }
                if let (Some(dst), Some(src)) = (sh.placeholder.as_mut(), l.placeholder.as_ref()) {
                    dst.ph_type = src.ph_type.clone();
                    dst.index = src.index;
                    dst.hint_style = src.hint_style.clone();
                }
            }
            None => {
                if lay_text_empty(sh) {
                    remove_ids.push(sh.id);
                }
            }
        }
    }
    slide.shapes.retain(|s| !remove_ids.contains(&s.id));
    let mut next_id = slide.shapes.iter().map(|x| x.id).max().unwrap_or(1) + 1;
    for (i, mut ph) in layout_ph.into_iter().enumerate() {
        if used[i] { continue; }
        ph.id = next_id;
        next_id += 1;
        slide.shapes.push(ph);
    }
    slide.layout_idx = Some(layout_idx);
    slide.layout_shapes = cache;
    slide.background = background;
    Ok(())
}

fn layout_count(p: &roe_model::impress::Presentation, slide_idx: usize) -> usize {
    let mi = p.slides[slide_idx].master_idx.unwrap_or(0);
    p.master_slides.get(mi).map(|m| m.layouts.len()).unwrap_or(0)
}

pub fn register_layout(engine: &mut Engine) {

    // set_slide_layout: 给已有页换版式(也可当"重置"用:补齐缺失占位符)。
    // 已有占位符按 类型+序号、类型、族 匹配到版式并重置几何,保留文字;匹配不上且为空的占位符被删除。
    // 换版式后形状 id/下标可能变化,请再用 inspect_presentation() 查看。
    engine.register_fn("set_slide_layout", |slide: i64, layout_idx: i64| -> String {
        with_pres(|p| {
            if slide < 0 || slide as usize >= p.slides.len() {
                return format!("err: slide {} 越界(共 {} 页)", slide, p.slides.len());
            }
            let n = layout_count(p, slide as usize);
            if layout_idx < 0 || layout_idx as usize >= n {
                return format!("err: layout_idx {} 无效(该页母版共 {} 个版式,用 list_layouts 查看)", layout_idx, n);
            }
            match lay_apply(p, slide as usize, layout_idx as usize) {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("err: {}", e),
            }
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });

    // insert_slide_with_layout: 在第 after 页之后新建一页并套用版式,返回 {"slide":新页下标}。
    engine.register_fn("insert_slide_with_layout", |after: i64, layout_idx: i64| -> String {
        with_pres(|p| {
            if after < 0 || (!p.slides.is_empty() && after as usize >= p.slides.len()) {
                return format!("err: after {} 越界(共 {} 页)", after, p.slides.len());
            }
            if p.slides.is_empty() {
                return "err: 演示文稿没有任何页,无法确定母版".to_string();
            }
            let ref_idx = after as usize;
            let n = layout_count(p, ref_idx);
            if layout_idx < 0 || layout_idx as usize >= n {
                return format!("err: layout_idx {} 无效(该页母版共 {} 个版式,用 list_layouts 查看)", layout_idx, n);
            }
            let at = lay_insert_blank_after(p, ref_idx);
            match lay_apply(p, at, layout_idx as usize) {
                Ok(()) => serde_json::json!({ "slide": at }).to_string(),
                Err(e) => { p.slides.remove(at); format!("err: {}", e) }
            }
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });
}

// ───────── P1 部分文字样式:set_shape_text_style_range ─────────
// 逻辑复制自 roe-api/impress_geometry.rs(split_run_at_v318s、apply_text_style_range_v318s),
// 改一处请同步另一处。与内核不同:越界/不在字符边界时报错而不是悄悄忽略。
fn rng_split_run_at(p: &mut roe_model::writer::Paragraph, offset: usize) {
    let mut acc = 0usize;
    for i in 0..p.runs.len() {
        let len = p.runs[i].text.len();
        if offset > acc && offset < acc + len {
            let at = offset - acc;
            if !p.runs[i].text.is_char_boundary(at) { return; }
            let mut right = p.runs[i].clone();
            right.text = p.runs[i].text.split_off(at);
            p.runs.insert(i + 1, right);
            return;
        }
        acc += len;
    }
}

fn rng_check_point(paras: &[roe_model::writer::Paragraph], pt: (usize, usize)) -> Result<(), String> {
    let p = paras.get(pt.0).ok_or_else(|| format!("段落号 {} 越界(共 {} 段)", pt.0, paras.len()))?;
    let total: usize = p.runs.iter().map(|r| r.text.len()).sum();
    if pt.1 > total {
        return Err(format!("段落 {} 的偏移 {} 超出段落长度 {}(偏移是 UTF-8 字节数,一个汉字占 3 字节)", pt.0, pt.1, total));
    }
    let mut acc = 0usize;
    for r in &p.runs {
        let len = r.text.len();
        if pt.1 >= acc && pt.1 <= acc + len {
            if !r.text.is_char_boundary(pt.1 - acc) {
                return Err(format!("段落 {} 的偏移 {} 落在字符中间(一个汉字占 3 字节)", pt.0, pt.1));
            }
            return Ok(());
        }
        acc += len;
    }
    Ok(())
}

fn rng_apply(
    paras: &mut [roe_model::writer::Paragraph],
    s: (usize, usize),
    e: (usize, usize),
    cmd: &str,
) -> Result<(), String> {
    let (s, e) = if s <= e { (s, e) } else { (e, s) };
    rng_check_point(paras, s)?;
    rng_check_point(paras, e)?;
    if s == e { return Err("选区为空".to_string()); }
    // 先在副本上跑一遍,命令非法时不留下被拆开的 run
    let mut work = paras.to_vec();
    if let Some(p) = work.get_mut(s.0) { rng_split_run_at(p, s.1); }
    if let Some(p) = work.get_mut(e.0) { rng_split_run_at(p, e.1); }
    let mut sel: Vec<&mut roe_model::writer::Run> = Vec::new();
    for (pi, p) in work.iter_mut().enumerate() {
        if pi < s.0 || pi > e.0 { continue; }
        let mut acc = 0usize;
        for r in p.runs.iter_mut() {
            let len = r.text.len();
            let (rs, re) = (acc, acc + len);
            acc = re;
            let from = if pi == s.0 { s.1 } else { 0 };
            let to = if pi == e.0 { e.1 } else { usize::MAX };
            if len > 0 && rs >= from && re <= to { sel.push(r); }
        }
    }
    apply_run_style(sel, cmd)?;
    paras.clone_from_slice(&work);
    Ok(())
}

pub fn register_style_range(engine: &mut Engine) {
    // set_shape_text_style_range: 只改选区 (p1,o1)-(p2,o2) 的文字样式。
    // 段落号从 0 起;偏移是段内 UTF-8 字节数(一个汉字 3 字节),可先用 get_shape_text_by_id 看文字。
    // 起点大于终点会自动对调;空选区、越界、偏移落在字符中间都返回 err: 且不改动。
    // cmd 与 set_shape_text_style 相同。
    engine.register_fn("set_shape_text_style_range", |slide: i64, shape_id: i64, p1: i64, o1: i64, p2: i64, o2: i64, cmd: String| -> String {
        if p1 < 0 || o1 < 0 || p2 < 0 || o2 < 0 {
            return "err: 段落号和偏移不能为负数".to_string();
        }
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => match &mut p.slides[si].shapes[hi].content {
                ShapeContent::Text(paras) => {
                    match rng_apply(paras, (p1 as usize, o1 as usize), (p2 as usize, o2 as usize), &cmd) {
                        Ok(()) => "ok".to_string(),
                        Err(e) => format!("err: {}", e),
                    }
                }
                _ => "err: 该形状没有文字(只有文本形状可用)".to_string(),
            },
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });
}

// ───────── P1 图片:insert_slide_image / set_slide_background_image ─────────
// 图片从本地文件路径读取,只认 PNG / JPEG(按文件头判断,与 roe-api 内核一致)。
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

fn image_from_bytes(data: Vec<u8>, label: &str) -> Result<roe_model::common::ImageData, String> {
    if data.is_empty() {
        return Err(format!("{} 是空文件", label));
    }
    if data.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!("{} 超过 20MB 上限", label));
    }
    let mime = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else {
        return Err(format!("{} 不是 PNG 或 JPEG 图片", label));
    };
    Ok(roe_model::common::ImageData {
        data,
        mime: mime.to_string(),
        crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
    })
}

fn load_image(path: &str) -> Result<roe_model::common::ImageData, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("路径为空".to_string());
    }
    // 工作区虚拟路径:仅当宿主绑定了媒体来源(Agent 页)时走它;
    // 未绑定(面板、CLI)时一律按真实路径处理,行为不变。
    let is_virtual = ["/inputs/", "/media/", "/exports/"].iter().any(|p| path.starts_with(p));
    if is_virtual {
        if let Some(r) = crate::media_context::with_media(|src| src.read_media(path)) {
            let data = r.map_err(|e| format!("无法读取 {}: {}", path, e))?;
            return image_from_bytes(data, path);
        }
    }
    let meta = std::fs::metadata(path).map_err(|e| format!("无法读取 {}: {}", path, e))?;
    if !meta.is_file() {
        return Err(format!("{} 不是文件", path));
    }
    if meta.len() == 0 {
        return Err(format!("{} 是空文件", path));
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!("{} 超过 20MB 上限", path));
    }
    let data = std::fs::read(path).map_err(|e| format!("无法读取 {}: {}", path, e))?;
    image_from_bytes(data, path)
}

pub fn register_image(engine: &mut Engine) {
    // insert_slide_image: 在指定页插入本地图片(PNG/JPEG),x/y/w/h 单位 pt(必须写小数),
    // 无填充无描边。成功返回新 shape_id(失败 -1,原因见 get_last_error)。
    engine.register_fn("insert_slide_image", |slide: i64, path: String, x: f64, y: f64, w: f64, h: f64| -> i64 {
        if bad_rect(x, y, w, h) {
            return fail("err: x/y/w/h 必须是有限数,且 w、h 必须大于 0");
        }
        let img = match load_image(&path) {
            Ok(i) => i,
            Err(e) => return fail(format!("err: {}", e)),
        };
        insert_on_slide(slide, move |id| roe_model::impress::Shape {
            id,
            geometry: ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x as f32, y as f32, w as f32, h as f32),
            style: roe_model::common::ShapeStyle { fill: None, stroke: None, ..Default::default() },
            content: ShapeContent::Image(img),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        })
    });

    // set_slide_background_image: 把本页背景设为本地图片(PNG/JPEG,拉伸铺满)。成功返回 ok。
    // 清除背景用 Calc 侧之外的纯色背景接口,本函数只负责设置图片。
    engine.register_fn("set_slide_background_image", |slide: i64, path: String| -> String {
        let img = match load_image(&path) {
            Ok(i) => i,
            Err(e) => return format!("err: {}", e),
        };
        with_pres(|p| {
            if slide < 0 || slide as usize >= p.slides.len() {
                return format!("err: slide {} 越界(共 {} 页)", slide, p.slides.len());
            }
            p.slides[slide as usize].background = Some(roe_model::common::FillStyle::Picture(img));
            "ok".to_string()
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });
}

pub fn register_geom(engine: &mut Engine) {

    // set_shape_rect: 一步设置位置和尺寸(pt,绝对值)。w、h 必须 > 0。
    engine.register_fn("set_shape_rect", |slide: i64, shape_id: i64, x: f64, y: f64, w: f64, h: f64| -> String {
        if !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
            return "err: x/y/w/h 必须是有限数,且 w、h 必须大于 0".to_string();
        }
        edit_shape(slide, shape_id, |sh| {
            sh.position.offset = (x as f32, y as f32);
            let (ow, oh) = sh.position.extent;
            sh.position.extent = (w as f32, h as f32);
            reflow_geometry(sh, ow, oh);
        })
    });

    // set_shape_rotation: 设置绝对旋转角度(度,不是累加)。
    engine.register_fn("set_shape_rotation", |slide: i64, shape_id: i64, degrees: f64| -> String {
        if !degrees.is_finite() {
            return "err: degrees 必须是有限数".to_string();
        }
        edit_shape(slide, shape_id, |sh| { sh.position.rotation = Some(degrees as f32); })
    });

    // set_shape_shear: 设置绝对剪切角度(度,不是累加),shear_x 水平、shear_y 垂直,两者一起设置。
    engine.register_fn("set_shape_shear", |slide: i64, shape_id: i64, shear_x: f64, shear_y: f64| -> String {
        if !shear_x.is_finite() || !shear_y.is_finite() {
            return "err: shear 必须是有限数".to_string();
        }
        edit_shape(slide, shape_id, |sh| {
            sh.position.shear_x = Some(shear_x as f32);
            sh.position.shear_y = Some(shear_y as f32);
        })
    });

    // flip_shape_h / flip_shape_v: 切换(不是设置)水平/垂直镜像,调用两次恢复原状。
    engine.register_fn("flip_shape_h", |slide: i64, shape_id: i64| -> String {
        edit_shape(slide, shape_id, |sh| { sh.position.flip_h = Some(!sh.position.flip_h.unwrap_or(false)); })
    });
    engine.register_fn("flip_shape_v", |slide: i64, shape_id: i64| -> String {
        edit_shape(slide, shape_id, |sh| { sh.position.flip_v = Some(!sh.position.flip_v.unwrap_or(false)); })
    });

    // 层级:shapes 数组顺序即叠放顺序(靠后在上)。操作后形状下标会变,id 不变。
    engine.register_fn("shape_bring_to_front", |slide: i64, shape_id: i64| -> String {
        edit_order(slide, shape_id, |v, i| { let s = v.remove(i); v.push(s); })
    });
    engine.register_fn("shape_send_to_back", |slide: i64, shape_id: i64| -> String {
        edit_order(slide, shape_id, |v, i| { let s = v.remove(i); v.insert(0, s); })
    });
    engine.register_fn("shape_move_forward", |slide: i64, shape_id: i64| -> String {
        edit_order(slide, shape_id, |v, i| { if i + 1 < v.len() { v.swap(i, i + 1); } })
    });
    engine.register_fn("shape_move_backward", |slide: i64, shape_id: i64| -> String {
        edit_order(slide, shape_id, |v, i| { if i > 0 { v.swap(i, i - 1); } })
    });
}

// ───────── 插入与内容(P1):按 shape id 定位 ─────────
// 插入类成功返回新 shape_id(i64),失败返回 -1,原因记入 LAST_ERR(get_last_error 读取)。

fn make_paragraphs(text: &str) -> Vec<roe_model::writer::Paragraph> {
    text.split('\n').map(|l| roe_model::writer::Paragraph {
        runs: if l.is_empty() { vec![] } else {
            vec![roe_model::writer::Run { text: l.to_string(), ..Default::default() }]
        },
        ..Default::default()
    }).collect()
}

fn paras_text(paras: &[roe_model::writer::Paragraph]) -> String {
    paras.iter()
        .map(|p| p.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 创建类失败:记录原因(get_last_error 读取)并返回 -1。
fn fail(msg: impl Into<String>) -> i64 {
    crate::impress_api::set_last_err(msg);
    -1
}

/// 在指定页追加形状,成功返回新 shape_id,失败返回 -1(原因记入 LAST_ERR)。
fn insert_on_slide(slide: i64, make: impl FnOnce(u32) -> roe_model::impress::Shape) -> i64 {
    with_pres(|p| {
        if slide < 0 || slide as usize >= p.slides.len() {
            return fail(format!("err: slide {} 越界(共 {} 页)", slide, p.slides.len()));
        }
        let s = &mut p.slides[slide as usize];
        let id = s.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
        s.shapes.push(make(id));
        id as i64
    })
    .unwrap_or_else(|| fail(NO_PRES))
}

fn bad_rect(x: f64, y: f64, w: f64, h: f64) -> bool {
    !(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0
}

fn build_table(rows: usize, cols: usize, w: f32, h: f32) -> roe_model::common::Table {
    use roe_model::common::{CellContent, Color, Table, TableCell, TableRow};
    let white = Color { r: 255, g: 255, b: 255, a: 255 };
    let mk_cell = |ri: usize| {
        let (r, g, b) = if ri == 0 { (0x5B, 0x9B, 0xD5) }
            else if ri % 2 == 1 { (0xD2, 0xDE, 0xEF) } else { (0xEA, 0xEF, 0xF7) };
        TableCell {
            content: vec![CellContent::Paragraph(Default::default())],
            col_span: 1, row_span: 1, style: None, hidden: false,
            bg_color: Some(Color { r, g, b, a: 255 }),
            border_color: Some(white.clone()), border_width: None,
            pad_left: None, pad_right: None, pad_top: None, pad_bottom: None,
        }
    };
    Table {
        rows: (0..rows).map(|ri| TableRow {
            cells: (0..cols).map(|_| mk_cell(ri)).collect(),
            row_height: Some(h / rows as f32),
        }).collect(),
        col_widths: vec![w / cols as f32; cols],
        style_id: Some("{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}".to_string()),
        first_row: true, first_col: false, last_row: false, last_col: false,
        band_row: true, band_col: false,
        default_border_color: None, default_border_width: None,
        inside_h_color: None, inside_h_width: None,
        inside_v_color: None, inside_v_width: None,
        float_pos: None,
    }
}

fn table_cell_mut<'a>(
    p: &'a mut roe_model::impress::Presentation, slide: i64, shape_id: i64, row: i64, col: i64,
) -> Result<&'a mut roe_model::common::TableCell, String> {
    let (si, hi) = locate(p, slide, shape_id)?;
    let t = match &mut p.slides[si].shapes[hi].content {
        ShapeContent::Table(t) => t,
        _ => return Err(format!("err: id={} 的形状不是表格(用 inspect_presentation() 查看类型)", shape_id)),
    };
    let rows = t.rows.len();
    if row < 0 || row as usize >= rows {
        return Err(format!("err: row {} 越界(表格共 {} 行)", row, rows));
    }
    let cols = t.rows[row as usize].cells.len();
    if col < 0 || col as usize >= cols {
        return Err(format!("err: col {} 越界(第 {} 行共 {} 列)", col, row, cols));
    }
    Ok(&mut t.rows[row as usize].cells[col as usize])
}

pub fn register_content(engine: &mut Engine) {
    use roe_model::common::CellContent;

    // insert_textbox: 在指定页新建文本框(矩形,无占位符),返回新 shape_id(失败 -1,原因见 get_last_error)。文字里 \n 分段。
    engine.register_fn("insert_textbox", |slide: i64, x: f64, y: f64, w: f64, h: f64, text: String| -> i64 {
        if bad_rect(x, y, w, h) {
            return fail("err: x/y/w/h 必须是有限数,且 w、h 必须大于 0");
        }
        insert_on_slide(slide, |id| roe_model::impress::Shape {
            id,
            geometry: ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x as f32, y as f32, w as f32, h as f32),
            style: roe_model::common::ShapeStyle { fill: Some(roe_model::common::FillStyle::None), stroke: None, ..Default::default() },
            content: ShapeContent::Text(make_paragraphs(&text)),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        })
    });

    // insert_slide_table: 新建 rows×cols 空表格(1..=100 行,1..=50 列),返回新 shape_id(失败 -1,原因见 get_last_error)。
    engine.register_fn("insert_slide_table", |slide: i64, x: f64, y: f64, w: f64, h: f64, rows: i64, cols: i64| -> i64 {
        if bad_rect(x, y, w, h) {
            return fail("err: x/y/w/h 必须是有限数,且 w、h 必须大于 0");
        }
        if rows < 1 || rows > 100 || cols < 1 || cols > 50 {
            return fail("err: 行列数越界(1..=100 行, 1..=50 列)");
        }
        let table = build_table(rows as usize, cols as usize, w as f32, h as f32);
        insert_on_slide(slide, |id| roe_model::impress::Shape {
            id,
            geometry: ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x as f32, y as f32, w as f32, h as f32),
            style: roe_model::common::ShapeStyle { fill: None, stroke: None, ..Default::default() },
            content: ShapeContent::Table(table),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        })
    });


    // set_shape_text_by_id: 按 id 替换文字,保留首段样式与首个 run 格式;\n 分段。成功 "ok"。
    engine.register_fn("set_shape_text_by_id", |slide: i64, shape_id: i64, text: String| -> String {
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => match &mut p.slides[si].shapes[hi].content {
                ShapeContent::Text(paras) => {
                    let tpl = paras.first().cloned().unwrap_or_default();
                    let run_tpl = tpl.runs.first().cloned().unwrap_or_default();
                    *paras = text.split('\n').map(|l| {
                        let mut pa = tpl.clone();
                        let mut r = run_tpl.clone();
                        r.text = l.to_string();
                        pa.runs = if l.is_empty() { vec![] } else { vec![r] };
                        pa
                    }).collect();
                    "ok".to_string()
                }
                _ => format!("err: id={} 的形状不是文本形状", shape_id),
            },
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });


    // set_shape_table_cell: 写表格单元格文字,保留首段样式与首个 run 格式;\n 分段。成功 "ok"。
    engine.register_fn("set_shape_table_cell", |slide: i64, shape_id: i64, row: i64, col: i64, text: String| -> String {
        with_pres(|p| match table_cell_mut(p, slide, shape_id, row, col) {
            Ok(cell) => {
                if cell.hidden {
                    return "err: 该单元格是合并单元格的延续格,请写合并区域的起始格".to_string();
                }
                let tpl = cell.content.iter().find_map(|c| match c {
                    CellContent::Paragraph(pa) => Some(pa.clone()),
                    _ => None,
                }).unwrap_or_default();
                let run_tpl = tpl.runs.first().cloned().unwrap_or_default();
                let mut new_content: Vec<CellContent> = text.split('\n').map(|l| {
                    let mut pa = tpl.clone();
                    let mut r = run_tpl.clone();
                    r.text = l.to_string();
                    pa.runs = if l.is_empty() { vec![] } else { vec![r] };
                    CellContent::Paragraph(pa)
                }).collect();
                new_content.extend(cell.content.drain(..).filter(|c| !matches!(c, CellContent::Paragraph(_))));
                cell.content = new_content;
                "ok".to_string()
            }
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });
}

// ───────── P2-H 只读样式:get_shape_style / get_slide_background ─────────
// 背景 JSON 与 set_slide_background 的输入对称(solid/gradient/none 可直接喂回);图片背景只报类型。

fn sgr_hex(c: &roe_model::common::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

fn sgr_color(c: &roe_model::common::Color) -> serde_json::Value {
    serde_json::json!({ "r": c.r, "g": c.g, "b": c.b, "a": c.a })
}

fn sgr_fill(f: &roe_model::common::FillStyle) -> serde_json::Value {
    use roe_model::common::{FillStyle, GradientKind};
    match f {
        FillStyle::None => serde_json::json!({ "type": "none" }),
        FillStyle::Solid(c) => serde_json::json!({ "type": "solid", "color": sgr_color(c) }),
        FillStyle::Gradient { stops, kind, rotate_with_shape } => {
            let st: Vec<serde_json::Value> = stops.iter()
                .map(|s| serde_json::json!({ "pos": r2(s.pos), "color": sgr_color(&s.color) }))
                .collect();
            let mut v = serde_json::json!({ "type": "gradient", "stops": st, "rotate_with_shape": rotate_with_shape });
            let m = v.as_object_mut().unwrap();
            match kind {
                GradientKind::Linear { angle } => {
                    m.insert("kind".to_string(), serde_json::json!("linear"));
                    m.insert("angle".to_string(), serde_json::json!(r2(*angle)));
                }
                GradientKind::Radial => { m.insert("kind".to_string(), serde_json::json!("radial")); }
                GradientKind::Rect => { m.insert("kind".to_string(), serde_json::json!("rect")); }
                _ => { m.insert("kind".to_string(), serde_json::json!("other")); }
            }
            v
        }
        FillStyle::Picture(_) => serde_json::json!({ "type": "picture" }),
        FillStyle::PictureTiled { .. } => serde_json::json!({ "type": "picture_tiled" }),
        #[allow(unreachable_patterns)]
        _ => serde_json::json!({ "type": "other" }),
    }
}

fn register_style_read(engine: &mut Engine) {
    // get_shape_style: 返回 JSON {fill, fill_detail, stroke, stroke_w, rotation, flip_h, flip_v, z_index, shadow}。
    // fill/stroke 为 "#rrggbb" 或 "none"(渐变/图片填充时 fill 为 gradient/picture/picture_tiled,细节见 fill_detail)。
    engine.register_fn("get_shape_style", |slide: i64, shape_id: i64| -> String {
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => {
                let sh = &p.slides[si].shapes[hi];
                let detail = sh.style.fill.as_ref().map(sgr_fill).unwrap_or_else(|| serde_json::json!({ "type": "none" }));
                let fill = match detail["type"].as_str().unwrap_or("none") {
                    "solid" => match sh.style.fill.as_ref() {
                        Some(roe_model::common::FillStyle::Solid(c)) => sgr_hex(c),
                        _ => "none".to_string(),
                    },
                    t => t.to_string(),
                };
                let (stroke, stroke_w) = match sh.style.stroke.as_ref() {
                    Some(s) => (sgr_hex(&s.color), r2(s.width)),
                    None => ("none".to_string(), 0.0),
                };
                serde_json::json!({
                    "fill": fill,
                    "fill_detail": detail,
                    "stroke": stroke,
                    "stroke_w": stroke_w,
                    "rotation": r2(sh.position.rotation.unwrap_or(0.0)),
                    "flip_h": sh.position.flip_h.unwrap_or(false),
                    "flip_v": sh.position.flip_v.unwrap_or(false),
                    "z_index": hi,
                    "shadow": sh.style.shadow.is_some(),
                }).to_string()
            }
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });

    // get_shape_char_format_by_id: 按形状 id 读单个 run 的字符格式(含颜色),返回 JSON;失败 err:。
    engine.register_fn("get_shape_char_format_by_id", |slide: i64, shape_id: i64, para_idx: i64, run_idx: i64| -> String {
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => match &p.slides[si].shapes[hi].content {
                ShapeContent::Text(paras) => {
                    if para_idx < 0 || para_idx as usize >= paras.len() {
                        return format!("err: para_idx {} 越界(共 {} 段)", para_idx, paras.len());
                    }
                    let runs = &paras[para_idx as usize].runs;
                    if run_idx < 0 || run_idx as usize >= runs.len() {
                        return format!("err: run_idx {} 越界(该段共 {} 个 run;空段落没有 run,请先 set_shape_text)", run_idx, runs.len());
                    }
                    let r = &runs[run_idx as usize];
                    let script = r.script.as_ref().map(|s| format!("{:?}", s).to_lowercase());
                    serde_json::json!({
                        "bold": r.bold.unwrap_or(false),
                        "italic": r.italic.unwrap_or(false),
                        "underline": r.underline.unwrap_or(false),
                        "strikethrough": r.strikethrough.unwrap_or(false),
                        "font_size": r.font_size,
                        "font_family": r.font_family,
                        "color": r.color.as_ref().map(sgr_hex),
                        "background": r.background.as_ref().map(sgr_hex),
                        "script": script,
                        "char_style": r.char_style.as_ref().map(|s| &s.0),
                    }).to_string()
                }
                _ => "err: 该形状没有文字(只有文本形状可用)".to_string(),
            },
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });

    // get_slide_background: 返回背景 JSON,无背景为 {"type":"none"}。
    engine.register_fn("get_slide_background", |slide_idx: i64| -> String {
        with_pres(|p| {
            if slide_idx < 0 || slide_idx as usize >= p.slides.len() {
                return format!("err: slide {} 越界(共 {} 页)", slide_idx, p.slides.len());
            }
            match p.slides[slide_idx as usize].background.as_ref() {
                None => serde_json::json!({ "type": "none" }).to_string(),
                Some(f) => sgr_fill(f).to_string(),
            }
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });
}

// ───────── P2-C 图表:insert_slide_chart / insert_slide_stat_chart / get_slide_chart_json / set_slide_chart_json ─────────
// 逻辑复制自 roe-api/src/impress_chart_ops.rs(roe-script 不能依赖 roe-api,改一处请同步另一份)。
// 统计图只支持插入;读写只支持普通图表。

fn slchart_parse(json: &str) -> Result<roe_model::impress::ChartModel, String> {
    serde_json::from_str::<roe_model::impress::ChartModel>(json)
        .map_err(|e| format!("err: 图表 JSON 解析失败: {e}"))
}

fn slchart_shape(id: u32, x: f64, y: f64, w: f64, h: f64, content: ShapeContent) -> roe_model::impress::Shape {
    roe_model::impress::Shape {
        id,
        geometry: ShapeGeometry::Rect,
        position: roe_model::common::Transform::from_rect(x as f32, y as f32, w as f32, h as f32),
        style: roe_model::common::ShapeStyle { fill: None, stroke: None, ..Default::default() },
        content,
        text_body: Default::default(),
        placeholder: None,
        st_cxn: None,
        end_cxn: None,
    }
}

/// 只读:get_slide_chart_json。由 register_read 调用,只读引擎也会注册。
fn register_chart_read(engine: &mut Engine) {
    // get_slide_chart_json: 返回普通图表的完整 ChartModel JSON;非图表/统计图返回 err:。
    engine.register_fn("get_slide_chart_json", |slide: i64, shape_id: i64| -> String {
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => match &p.slides[si].shapes[hi].content {
                ShapeContent::Chart(m) => serde_json::to_string(m)
                    .unwrap_or_else(|e| format!("err: 序列化失败: {e}")),
                ShapeContent::StatChart(_) => "err: 该形状是统计图,不支持读取 JSON".to_string(),
                _ => "err: 该形状不是图表".to_string(),
            },
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });
}

/// 写函数:三个图表写入口。
pub fn register_chart(engine: &mut Engine) {
    // insert_slide_chart: 插入普通图表(ChartModel JSON,必填 kind/x_labels/series),返回新 shape_id(失败 -1,原因见 get_last_error)。
    engine.register_fn("insert_slide_chart", |slide: i64, chart_json: String, x: f64, y: f64, w: f64, h: f64| -> i64 {
        if bad_rect(x, y, w, h) {
            return fail("err: x/y/w/h 必须是有限数,且 w、h 必须大于 0");
        }
        let model = match slchart_parse(&chart_json) {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        insert_on_slide(slide, move |id| {
            let mut m = model;
            m.id = id as u64;
            m.x_pt = x as f32;
            m.y_pt = y as f32;
            m.width_pt = w as f32;
            m.height_pt = h as f32;
            slchart_shape(id, x, y, w, h, ShapeContent::Chart(m))
        })
    });

    // insert_slide_stat_chart: 插入统计图(StatChartModel 完整 JSON),返回新 shape_id(失败 -1,原因见 get_last_error)。
    engine.register_fn("insert_slide_stat_chart", |slide: i64, stat_json: String, x: f64, y: f64, w: f64, h: f64| -> i64 {
        if bad_rect(x, y, w, h) {
            return fail("err: x/y/w/h 必须是有限数,且 w、h 必须大于 0");
        }
        let model = match serde_json::from_str::<roe_model::stat_chart::StatChartModel>(&stat_json) {
            Ok(m) => m,
            Err(e) => return fail(format!("err: 统计图 JSON 解析失败: {e}")),
        };
        insert_on_slide(slide, move |id| slchart_shape(id, x, y, w, h, ShapeContent::StatChart(model)))
    });

    // set_slide_chart_json: 整体替换普通图表内容,保留原 id、位置、尺寸。先解析再修改。
    engine.register_fn("set_slide_chart_json", |slide: i64, shape_id: i64, chart_json: String| -> String {
        let new_model = match slchart_parse(&chart_json) {
            Ok(m) => m,
            Err(e) => return e,
        };
        with_pres(|p| match locate(p, slide, shape_id) {
            Ok((si, hi)) => match &mut p.slides[si].shapes[hi].content {
                ShapeContent::Chart(old) => {
                    let mut nm = new_model;
                    nm.id = old.id;
                    nm.x_pt = old.x_pt;
                    nm.y_pt = old.y_pt;
                    nm.width_pt = old.width_pt;
                    nm.height_pt = old.height_pt;
                    *old = nm;
                    "ok".to_string()
                }
                ShapeContent::StatChart(_) => "err: 该形状是统计图,不支持改写 JSON".to_string(),
                _ => "err: 该形状不是图表".to_string(),
            },
            Err(e) => e,
        })
        .unwrap_or_else(|| NO_PRES.to_string())
    });
}

#[cfg(test)]
mod tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};

    fn run(script: &str) -> String {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(&mut pres, || {
            out = e.eval::<String>(script).unwrap();
        });
        out
    }

    #[test]
    fn inspect_basic() {
        let out = run("inspect_presentation()");
        let v: serde_json::Value = serde_json::from_str(&out).expect("应是合法 JSON");
        assert_eq!(v["slide_count"], 1);
        let sh = &v["slides"][0]["shapes"][0];
        assert_eq!(sh["index"], 0);
        assert!(sh["id"].is_u64());
        assert_eq!(sh["type"], "text");
        assert_eq!(sh["text"], "Hello Slide");
        assert_eq!(sh["text_len"], 11);
        assert_eq!(sh["text_truncated"], false);
    }

    #[test]
    fn inspect_truncates_long_text() {
        let out = run("let s = \"\"; for i in 0..100 { s += \"a\"; } set_shape_text(0, 0, s); inspect_presentation()");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let sh = &v["slides"][0]["shapes"][0];
        assert_eq!(sh["text"].as_str().unwrap().chars().count(), 40);
        assert_eq!(sh["text_len"], 100);
        assert_eq!(sh["text_truncated"], true);
    }

    #[test]
    fn inspect_without_presentation_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let out = e.eval::<String>("inspect_presentation()").unwrap();
        assert!(out.starts_with("err:"), "应返回 err: 开头,实际 {out}");
    }
}

#[cfg(test)]
mod geom_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::impress::Presentation;

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    /// 两个形状:ids = (a, b),b 在上层。
    fn two_shapes() -> (Presentation, u32, u32) {
        let mut pres = make_pres();
        let a = pres.slides[0].shapes[0].id;
        let mut second = pres.slides[0].shapes[0].clone();
        second.id = a + 1000;
        pres.slides[0].shapes.push(second);
        (pres, a, a + 1000)
    }

    fn order(pres: &Presentation) -> Vec<u32> {
        pres.slides[0].shapes.iter().map(|s| s.id).collect()
    }

    #[test]
    fn set_rect_and_get_rect() {
        let (mut pres, a, _) = two_shapes();
        let r = run(&mut pres, &format!("set_shape_rect(0, {a}, 10.0, 20.0, 300.0, 120.0)"));
        assert_eq!(r, "ok");
        let t = &pres.slides[0].shapes[0].position;
        assert_eq!(t.offset, (10.0, 20.0));
        assert_eq!(t.extent, (300.0, 120.0));
        let j = run(&mut pres, &format!("get_shape_rect(0, {a})"));
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["x"], 10.0);
        assert_eq!(v["w"], 300.0);
        assert_eq!(v["flip_h"], false);
    }

    #[test]
    fn shape_shear_set() {
        let (mut pres, a, _) = two_shapes();
        assert_eq!(run(&mut pres, &format!("set_shape_shear(0, {a}, 15.0, -5.0)")), "ok");
        let p = &pres.slides[0].shapes[0].position;
        assert_eq!(p.shear_x, Some(15.0));
        assert_eq!(p.shear_y, Some(-5.0));
        assert_eq!(run(&mut pres, &format!("set_shape_shear(0, {a}, 0.0, 30.0)")), "ok");
        let p = &pres.slides[0].shapes[0].position;
        assert_eq!(p.shear_x, Some(0.0));
        assert_eq!(p.shear_y, Some(30.0));
        assert!(run(&mut pres, "set_shape_shear(0, 99999, 1.0, 1.0)").starts_with("err:"));
        assert_eq!(pres.slides[0].shapes[0].position.shear_y, Some(30.0));
    }

    #[test]
    fn rotation_and_flip() {
        let (mut pres, a, _) = two_shapes();
        assert_eq!(run(&mut pres, &format!("set_shape_rotation(0, {a}, 45.0)")), "ok");
        assert_eq!(pres.slides[0].shapes[0].position.rotation, Some(45.0));
        run(&mut pres, &format!("flip_shape_h(0, {a})"));
        assert_eq!(pres.slides[0].shapes[0].position.flip_h, Some(true));
        run(&mut pres, &format!("flip_shape_h(0, {a})"));
        assert_eq!(pres.slides[0].shapes[0].position.flip_h, Some(false));
        run(&mut pres, &format!("flip_shape_v(0, {a})"));
        assert_eq!(pres.slides[0].shapes[0].position.flip_v, Some(true));
    }

    #[test]
    fn z_order_ops() {
        let (mut pres, a, b) = two_shapes();
        assert_eq!(order(&pres), vec![a, b]);
        run(&mut pres, &format!("shape_bring_to_front(0, {a})"));
        assert_eq!(order(&pres), vec![b, a]);
        run(&mut pres, &format!("shape_send_to_back(0, {a})"));
        assert_eq!(order(&pres), vec![a, b]);
        run(&mut pres, &format!("shape_move_forward(0, {a})"));
        assert_eq!(order(&pres), vec![b, a]);
        run(&mut pres, &format!("shape_move_backward(0, {a})"));
        assert_eq!(order(&pres), vec![a, b]);
        // 边界:已在最底再下移,不报错、顺序不变
        assert_eq!(run(&mut pres, &format!("shape_move_backward(0, {a})")), "ok");
        assert_eq!(order(&pres), vec![a, b]);
    }

    #[test]
    fn bad_input_reports_err_and_changes_nothing() {
        let (mut pres, a, _) = two_shapes();
        let before = pres.slides[0].shapes[0].position.extent;
        for script in [
            format!("set_shape_rect(0, 99999, 1.0, 1.0, 10.0, 10.0)"),
            format!("set_shape_rect(5, {a}, 1.0, 1.0, 10.0, 10.0)"),
            format!("set_shape_rect(-1, {a}, 1.0, 1.0, 10.0, 10.0)"),
            format!("set_shape_rect(0, -3, 1.0, 1.0, 10.0, 10.0)"),
            format!("set_shape_rect(0, {a}, 1.0, 1.0, 0.0, 10.0)"),
            format!("set_shape_rect(0, {a}, 1.0, 1.0, 10.0, -5.0)"),
            format!("set_shape_rotation(0, 99999, 10.0)"),
            format!("flip_shape_h(0, 99999)"),
            format!("shape_bring_to_front(0, 99999)"),
            format!("get_shape_rect(0, 99999)"),
        ] {
            let out = run(&mut pres, &script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        assert_eq!(pres.slides[0].shapes[0].position.extent, before);
    }

    #[test]
    fn without_presentation_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        for script in ["get_shape_rect(0,0)", "set_shape_rect(0,0,1.0,1.0,1.0,1.0)", "flip_shape_h(0,0)", "shape_send_to_back(0,0)"] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }
}

#[cfg(test)]
mod content_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::impress::Presentation;

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    fn id_of(s: &str) -> i64 {
        s.trim().parse::<i64>().unwrap_or_else(|_| panic!("不是整数 id: {s}"))
    }

    #[test]
    fn textbox_insert_read_write() {
        let mut pres = make_pres();
        let r = run(&mut pres, r#"insert_textbox(0, 10.0, 10.0, 200.0, 50.0, "hi")"#);
        let id = id_of(&r);
        assert_eq!(run(&mut pres, &format!("get_shape_text_by_id(0, {id})")), "hi");
        assert_eq!(run(&mut pres, &format!(r#"set_shape_text_by_id(0, {id}, "a\nb")"#)), "ok");
        assert_eq!(run(&mut pres, &format!("get_shape_text_by_id(0, {id})")), "a\nb");
    }

    #[test]
    fn ids_are_unique_within_slide() {
        let mut pres = make_pres();
        let a = id_of(&run(&mut pres, r#"insert_textbox(0, 1.0, 1.0, 10.0, 10.0, "x")"#));
        let b = id_of(&run(&mut pres, r#"insert_textbox(0, 1.0, 1.0, 10.0, 10.0, "y")"#));
        assert_ne!(a, b);
    }

    #[test]
    fn table_insert_and_cell_roundtrip() {
        let mut pres = make_pres();
        let id = id_of(&run(&mut pres, "insert_slide_table(0, 0.0, 0.0, 300.0, 100.0, 2, 3)"));
        assert_eq!(run(&mut pres, &format!(r#"set_shape_table_cell(0, {id}, 1, 2, "x")"#)), "ok");
        assert_eq!(run(&mut pres, &format!("get_shape_table_cell(0, {id}, 1, 2)")), "x");
        assert_eq!(run(&mut pres, &format!("get_shape_table_cell(0, {id}, 0, 0)")), "");
        let j = run(&mut pres, "inspect_presentation()");
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        let shapes = v["slides"][0]["shapes"].as_array().unwrap();
        let t = shapes.iter().find(|s| s["id"].as_i64() == Some(id)).unwrap();
        assert_eq!(t["type"], "table");
        assert_eq!(t["rows"], 2);
        assert_eq!(t["cols"], 3);
    }

    #[test]
    fn bad_input_reports_err() {
        let mut pres = make_pres();
        let tid = id_of(&run(&mut pres, "insert_slide_table(0, 0.0, 0.0, 300.0, 100.0, 2, 3)"));
        let before = pres.slides[0].shapes.len();
        fn assert_create_fails(pres: &mut Presentation, script: &str) {
            let out = run(pres, &format!("let r = {script}; r.to_string() + \"|\" + get_last_error()"));
            assert!(out.starts_with("-1|err:"), "{script} -> {out}");
        }
        for script in [
            "insert_slide_table(0, 0.0, 0.0, 300.0, 100.0, 0, 3)".to_string(),
            "insert_slide_table(0, 0.0, 0.0, 300.0, 100.0, 2, 99)".to_string(),
            "insert_slide_table(5, 0.0, 0.0, 300.0, 100.0, 2, 3)".to_string(),
            "insert_slide_table(0, 0.0, 0.0, 0.0, 100.0, 2, 3)".to_string(),
            r#"insert_textbox(9, 0.0, 0.0, 10.0, 10.0, "x")"#.to_string(),
            r#"insert_textbox(0, 0.0, 0.0, -1.0, 10.0, "x")"#.to_string(),
        ] {
            assert_create_fails(&mut pres, &script);
        }
        for script in [
            format!("get_shape_table_cell(0, {tid}, 5, 0)"),
            format!("get_shape_table_cell(0, {tid}, 0, 9)"),
            format!(r#"set_shape_table_cell(0, {tid}, 0, 9, "x")"#),
            "get_shape_table_cell(0, 99999, 0, 0)".to_string(),
            format!("get_shape_text_by_id(0, {tid})"),
            format!(r#"set_shape_text_by_id(0, {tid}, "x")"#),
            r#"set_shape_text_by_id(0, 99999, "x")"#.to_string(),
        ] {
            let out = run(&mut pres, &script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        assert_eq!(pres.slides[0].shapes.len(), before, "失败的调用不应新增形状");
    }

    #[test]
    fn without_presentation_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        assert_eq!(e.eval::<i64>(r#"insert_textbox(0, 0.0, 0.0, 1.0, 1.0, "x")"#).unwrap(), -1);
        assert_eq!(e.eval::<i64>("insert_slide_table(0, 0.0, 0.0, 1.0, 1.0, 1, 1)").unwrap(), -1);
        for script in [
            "get_shape_text_by_id(0, 0)",
            r#"set_shape_text_by_id(0, 0, "x")"#,
            "get_shape_table_cell(0, 0, 0, 0)",
            r#"set_shape_table_cell(0, 0, 0, 0, "x")"#,
        ] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }
}

#[cfg(test)]
mod style_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::common::FillStyle;
    use roe_model::impress::{Presentation, ShapeContent};

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    fn id_of(s: &str) -> i64 {
        s.trim().parse::<i64>().unwrap_or_else(|_| panic!("不是整数 id: {s}"))
    }

    fn runs_of(pres: &Presentation) -> Vec<roe_model::writer::Run> {
        match &pres.slides[0].shapes[0].content {
            ShapeContent::Text(ps) => ps.iter().flat_map(|p| p.runs.iter().cloned()).collect(),
            _ => panic!("not text"),
        }
    }

    #[test]
    fn fill_set_and_clear() {
        let mut pres = make_pres();
        assert_eq!(run(&mut pres, r##"set_shape_fill(0, 1, "#ff0000")"##), "ok");
        assert!(matches!(&pres.slides[0].shapes[0].style.fill,
            Some(FillStyle::Solid(c)) if c.r == 255 && c.g == 0 && c.b == 0));
        assert_eq!(run(&mut pres, r#"set_shape_fill(0, 1, "none")"#), "ok");
        assert!(matches!(&pres.slides[0].shapes[0].style.fill, Some(FillStyle::None)));
    }

    #[test]
    fn stroke_set_keep_clear_and_default() {
        let mut pres = make_pres();
        assert_eq!(run(&mut pres, r#"set_shape_stroke(0, 1, "none", -1.0)"#), "ok");
        assert!(pres.slides[0].shapes[0].style.stroke.is_none());
        // 无描边时只设宽度:用默认色 #2b579a
        assert_eq!(run(&mut pres, r#"set_shape_stroke(0, 1, "", 2.0)"#), "ok");
        let st = pres.slides[0].shapes[0].style.stroke.clone().unwrap();
        assert_eq!((st.color.r, st.color.g, st.color.b), (0x2b, 0x57, 0x9a));
        assert_eq!(st.width, 2.0);
        // 设颜色与宽度
        assert_eq!(run(&mut pres, r##"set_shape_stroke(0, 1, "#00ff00", 3.0)"##), "ok");
        let st = pres.slides[0].shapes[0].style.stroke.clone().unwrap();
        assert_eq!((st.color.r, st.color.g, st.color.b), (0, 255, 0));
        assert_eq!(st.width, 3.0);
        // 颜色传空、宽度 -1 都不改
        assert_eq!(run(&mut pres, r#"set_shape_stroke(0, 1, "", -1.0)"#), "ok");
        let st = pres.slides[0].shapes[0].style.stroke.clone().unwrap();
        assert_eq!((st.color.g, st.width), (255, 3.0));
        // 只改宽度
        assert_eq!(run(&mut pres, r#"set_shape_stroke(0, 1, "", 5.0)"#), "ok");
        assert_eq!(pres.slides[0].shapes[0].style.stroke.clone().unwrap().width, 5.0);
        // 清除
        assert_eq!(run(&mut pres, r#"set_shape_stroke(0, 1, "none", 9.0)"#), "ok");
        assert!(pres.slides[0].shapes[0].style.stroke.is_none());
    }

    #[test]
    fn text_style_toggle_and_values() {
        let mut pres = make_pres();
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "bold:0")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.bold == Some(false)));
        // 省略冒号 = toggle
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "bold")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.bold == Some(true)));
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "bold:toggle")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.bold == Some(false)));
        // 字号:绝对与相对
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "size:20")"#), "ok");
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "size:+2")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.font_size == Some(22.0)));
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "size:-3")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.font_size == Some(19.0)));
        // 颜色、高亮、字体、上标
        assert_eq!(run(&mut pres, r##"set_shape_text_style(0, 1, "color:#112233")"##), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.color.map(|c| (c.r, c.g, c.b)) == Some((0x11, 0x22, 0x33))));
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "color:auto")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.color.is_none()));
        assert_eq!(run(&mut pres, r##"set_shape_text_style(0, 1, "background:#ffff00")"##), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.background.is_some()));
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "background:none")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.background.is_none()));
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "font:Arial")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.font_family.as_deref() == Some("Arial")));
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "script:super")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.script == Some(roe_model::writer::ScriptPosition::Superscript)));
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "script:super")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.script.is_none()));
    }

    #[test]
    fn text_style_clear_without_colon() {
        let mut pres = make_pres();
        run(&mut pres, r#"set_shape_text_style(0, 1, "bold:1")"#);
        run(&mut pres, r#"set_shape_text_style(0, 1, "size:30")"#);
        assert_eq!(run(&mut pres, r#"set_shape_text_style(0, 1, "clear")"#), "ok");
        assert!(runs_of(&pres).iter().all(|r| r.bold.is_none() && r.font_size.is_none()));
    }

    #[test]
    fn bad_input_reports_err_and_changes_nothing() {
        let mut pres = make_pres();
        run(&mut pres, r##"set_shape_fill(0, 1, "#ff0000")"##);
        run(&mut pres, r#"set_shape_text_style(0, 1, "size:20")"#);
        let before = serde_json::to_string(&pres.slides[0].shapes[0].style).unwrap();
        let runs_before = runs_of(&pres).iter().map(|r| r.font_size).collect::<Vec<_>>();
        for script in [
            r#"set_shape_fill(0, 1, "red")"#,
            r##"set_shape_fill(0, 1, "#12345")"##,
            r#"set_shape_fill(9, 1, "none")"#,
            r#"set_shape_fill(0, 99, "none")"#,
            r#"set_shape_stroke(0, 1, "zzz", 1.0)"#,
            r#"set_shape_stroke(0, 99, "", 1.0)"#,
            r#"set_shape_text_style(0, 1, "size:abc")"#,
            r#"set_shape_text_style(0, 1, "bold:maybe")"#,
            r#"set_shape_text_style(0, 1, "script:middle")"#,
            r#"set_shape_text_style(0, 1, "color:blue")"#,
            r#"set_shape_text_style(0, 1, "font:")"#,
            r#"set_shape_text_style(0, 1, "wobble:1")"#,
            r#"set_shape_text_style(0, 99, "bold")"#,
        ] {
            let out = run(&mut pres, script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        assert_eq!(serde_json::to_string(&pres.slides[0].shapes[0].style).unwrap(), before);
        assert_eq!(runs_of(&pres).iter().map(|r| r.font_size).collect::<Vec<_>>(), runs_before);
    }

    #[test]
    fn text_style_on_non_text_shape_errs() {
        let mut pres = make_pres();
        let tid = id_of(&run(&mut pres, "insert_slide_table(0, 0.0, 0.0, 100.0, 50.0, 2, 2)"));
        let out = run(&mut pres, &format!(r#"set_shape_text_style(0, {tid}, "bold")"#));
        assert!(out.starts_with("err:"), "{out}");
        // 填充对非文字形状仍可用
        assert_eq!(run(&mut pres, &format!(r##"set_shape_fill(0, {tid}, "#abcdef")"##)), "ok");
    }

    #[test]
    fn without_presentation_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        for script in [
            r#"set_shape_fill(0, 1, "none")"#,
            r#"set_shape_stroke(0, 1, "", 1.0)"#,
            r#"set_shape_text_style(0, 1, "bold")"#,
        ] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::impress::{
        MasterSlide, PlaceholderInfo, Presentation, Shape, ShapeContent, SlideLayout, SlideLayoutKind,
    };

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    fn ph(base: &Shape, id: u32, ty: &str, idx: u32, y: f32) -> Shape {
        let mut s = base.clone();
        s.id = id;
        s.position = roe_model::common::Transform::from_rect(10.0, y, 300.0, 40.0);
        s.content = ShapeContent::Text(vec![]);
        s.placeholder = Some(PlaceholderInfo { ph_type: Some(ty.to_string()), index: Some(idx), hint_style: None });
        s
    }

    fn pres_with_layouts() -> Presentation {
        let mut pres = make_pres();
        let base = pres.slides[0].shapes[0].clone();
        let l0 = SlideLayout {
            name: "Title+Body".into(),
            kind: SlideLayoutKind::TitleAndContent,
            shapes: vec![ph(&base, 101, "title", 0, 10.0), ph(&base, 102, "body", 1, 100.0)],
            background: None,
            show_master_shapes: true,
            color_map: None,
            raw_ext: None,
        };
        let l1 = SlideLayout {
            name: "TitleOnly".into(),
            kind: SlideLayoutKind::TitleOnly,
            shapes: vec![ph(&base, 201, "title", 0, 10.0)],
            background: None,
            show_master_shapes: true,
            color_map: None,
            raw_ext: None,
        };
        pres.master_slides = vec![MasterSlide {
            theme_xml: None,
            tx_styles_xml: None,
            layouts: vec![l0, l1],
            color_map: None,
            raw_ext: None,
            name: "M".into(),
            shapes: vec![],
            background: None,
        }];
        pres.slides[0].master_idx = Some(0);
        pres
    }

    fn ph_types(pres: &Presentation, slide: usize) -> Vec<String> {
        pres.slides[slide].shapes.iter()
            .filter_map(|s| s.placeholder.as_ref().and_then(|p| p.ph_type.clone()))
            .collect()
    }

    #[test]
    fn list_layouts_shows_all_and_current() {
        let mut pres = pres_with_layouts();
        let j = run(&mut pres, "list_layouts(0)");
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "Title+Body");
        assert_eq!(arr[0]["placeholders"], 2);
        assert_eq!(arr[1]["kind"], "TitleOnly");
        assert!(arr.iter().all(|x| x["current"] == false));
        assert_eq!(run(&mut pres, "set_slide_layout(0, 1)"), "ok");
        let v: serde_json::Value = serde_json::from_str(&run(&mut pres, "list_layouts(0)")).unwrap();
        assert_eq!(v[1]["current"], true);
        assert_eq!(v[0]["current"], false);
    }

    #[test]
    fn set_layout_adds_placeholders_and_keeps_plain_shape() {
        let mut pres = pres_with_layouts();
        assert_eq!(run(&mut pres, "set_slide_layout(0, 0)"), "ok");
        assert_eq!(pres.slides[0].layout_idx, Some(0));
        assert_eq!(ph_types(&pres, 0), vec!["title".to_string(), "body".to_string()]);
        // 原有普通文本形状(id=1)还在
        assert!(pres.slides[0].shapes.iter().any(|s| s.id == 1 && s.placeholder.is_none()));
        let ids: std::collections::HashSet<u32> = pres.slides[0].shapes.iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), pres.slides[0].shapes.len());
        // 换到只有 title 的版式:空的 body 占位符被删,title 保留
        assert_eq!(run(&mut pres, "set_slide_layout(0, 1)"), "ok");
        assert_eq!(ph_types(&pres, 0), vec!["title".to_string()]);
        assert_eq!(pres.slides[0].layout_idx, Some(1));
    }

    #[test]
    fn insert_slide_with_layout_works() {
        let mut pres = pres_with_layouts();
        let out = run(&mut pres, "insert_slide_with_layout(0, 0)");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["slide"], 1);
        assert_eq!(pres.slides.len(), 2);
        assert_eq!(pres.slides[1].layout_idx, Some(0));
        assert_eq!(ph_types(&pres, 1), vec!["title".to_string(), "body".to_string()]);
    }

    #[test]
    fn bad_input_reports_err_and_changes_nothing() {
        let mut pres = pres_with_layouts();
        for script in [
            "insert_slide_with_layout(0, 9)",
            "insert_slide_with_layout(0, -1)",
            "insert_slide_with_layout(5, 0)",
            "insert_slide_with_layout(-1, 0)",
            "set_slide_layout(0, 9)",
            "set_slide_layout(0, -1)",
            "set_slide_layout(7, 0)",
            "list_layouts(7)",
        ] {
            let out = run(&mut pres, script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        // 无效版式不能留下空白页,也不能改动原页
        assert_eq!(pres.slides.len(), 1);
        assert_eq!(pres.slides[0].layout_idx, None);
        assert_eq!(pres.slides[0].shapes.len(), 1);
    }

    #[test]
    fn without_presentation_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        for script in ["list_layouts(0)", "set_slide_layout(0, 0)", "insert_slide_with_layout(0, 0)"] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }
}

#[cfg(test)]
mod style_range_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::impress::{Presentation, ShapeContent};

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    fn id_of(s: &str) -> i64 {
        s.trim().parse::<i64>().unwrap_or_else(|_| panic!("不是整数 id: {s}"))
    }

    // 造一个两段文字的文本框:第 0 段 "abcd",第 1 段 "汉字ef"
    fn setup() -> (Presentation, i64) {
        let mut pres = make_pres();
        let id = id_of(&run(&mut pres, r#"insert_textbox(0, 0.0, 0.0, 100.0, 50.0, "abcd\n汉字ef")"#));
        (pres, id)
    }

    // 返回每段里 (文字, 是否加粗) 的列表
    fn dump(pres: &Presentation, id: i64) -> Vec<Vec<(String, bool)>> {
        let sh = pres.slides[0].shapes.iter().find(|s| s.id as i64 == id).unwrap();
        match &sh.content {
            ShapeContent::Text(ps) => ps.iter()
                .map(|p| p.runs.iter().map(|r| (r.text.clone(), r.bold == Some(true))).collect())
                .collect(),
            _ => panic!("not text"),
        }
    }

    #[test]
    fn middle_of_one_paragraph() {
        let (mut pres, id) = setup();
        assert_eq!(run(&mut pres, &format!(r#"set_shape_text_style_range(0, {id}, 0, 1, 0, 3, "bold:1")"#)), "ok");
        let d = dump(&pres, id);
        assert_eq!(d[0], vec![("a".into(), false), ("bc".into(), true), ("d".into(), false)]);
        assert_eq!(d[1], vec![("汉字ef".into(), false)]);
    }

    #[test]
    fn cross_paragraph_and_chinese_bytes() {
        let (mut pres, id) = setup();
        // 第 0 段从偏移 2 到末尾,第 1 段从开头到偏移 3(正好是"汉"字)
        assert_eq!(run(&mut pres, &format!(r#"set_shape_text_style_range(0, {id}, 0, 2, 1, 3, "bold:1")"#)), "ok");
        let d = dump(&pres, id);
        assert_eq!(d[0], vec![("ab".into(), false), ("cd".into(), true)]);
        assert_eq!(d[1], vec![("汉".into(), true), ("字ef".into(), false)]);
    }

    #[test]
    fn reversed_selection_is_swapped() {
        let (mut pres, id) = setup();
        assert_eq!(run(&mut pres, &format!(r#"set_shape_text_style_range(0, {id}, 0, 3, 0, 1, "bold:1")"#)), "ok");
        assert_eq!(dump(&pres, id)[0][1], ("bc".to_string(), true));
    }

    #[test]
    fn bad_input_reports_err_and_changes_nothing() {
        let (mut pres, id) = setup();
        let before = dump(&pres, id);
        for script in [
            format!(r#"set_shape_text_style_range(0, {id}, 0, 2, 0, 2, "bold:1")"#),   // 空选区
            format!(r#"set_shape_text_style_range(0, {id}, 0, 0, 0, 99, "bold:1")"#),  // 偏移越界
            format!(r#"set_shape_text_style_range(0, {id}, 5, 0, 5, 1, "bold:1")"#),   // 段落越界
            format!(r#"set_shape_text_style_range(0, {id}, 1, 1, 1, 3, "bold:1")"#),   // 落在汉字中间
            format!(r#"set_shape_text_style_range(0, {id}, 0, -1, 0, 2, "bold:1")"#),  // 负数
            format!(r#"set_shape_text_style_range(0, {id}, 0, 1, 0, 3, "wobble:1")"#), // 命令非法
            format!(r#"set_shape_text_style_range(0, 9999, 0, 1, 0, 3, "bold:1")"#),   // 形状不存在
            format!(r#"set_shape_text_style_range(9, {id}, 0, 1, 0, 3, "bold:1")"#),   // 页不存在
        ] {
            let out = run(&mut pres, &script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        // 非法命令也不能留下被拆开的 run
        assert_eq!(dump(&pres, id), before);
    }

    #[test]
    fn non_text_shape_errs() {
        let (mut pres, _) = setup();
        let tid = id_of(&run(&mut pres, "insert_slide_table(0, 0.0, 0.0, 100.0, 50.0, 2, 2)"));
        let out = run(&mut pres, &format!(r#"set_shape_text_style_range(0, {tid}, 0, 0, 0, 1, "bold:1")"#));
        assert!(out.starts_with("err:"), "{out}");
    }

    #[test]
    fn without_presentation_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let out = e.eval::<String>(r#"set_shape_text_style_range(0, 1, 0, 0, 0, 1, "bold:1")"#).unwrap();
        assert!(out.starts_with("err:"), "{out}");
    }
}

#[cfg(test)]
mod image_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::common::FillStyle;
    use roe_model::impress::{Presentation, ShapeContent};

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    // 最小的"看起来像 PNG"的字节(只需文件头正确,本函数不解码)
    fn write_tmp(name: &str, bytes: &[u8]) -> String {
        let p = std::env::temp_dir().join(format!("roe_img_test_{}_{}", std::process::id(), name));
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().to_string()
    }

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    const JPG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];

    /// 假 source:/inputs/logo.png 给合法 PNG,/inputs/bad.png 给非图片字节,其余 Err。
    struct FakeImgSource;
    impl crate::media_context::MediaAssetSource for FakeImgSource {
        fn read_media(&self, virtual_path: &str) -> Result<Vec<u8>, String> {
            match virtual_path {
                "/inputs/logo.png" => Ok(PNG.to_vec()),
                "/inputs/bad.png" => Ok(b"not an image at all".to_vec()),
                _ => Err(format!("not found: {virtual_path}")),
            }
        }
    }

    #[test]
    fn virtual_path_insert_reads_via_media_source() {
        let mut pres = make_pres();
        let src = FakeImgSource;
        let mut out = String::new();
        crate::media_context::with_media_source(Some(&src), || {
            out = run(&mut pres, r#"insert_slide_image(0, "/inputs/logo.png", 10.0, 20.0, 100.0, 50.0)"#);
        });
        let id: i64 = out.parse().unwrap();
        assert!(id >= 0, "虚拟路径应能插入,实际返回 {out}");
        let sh = pres.slides[0].shapes.iter().find(|s| s.id == id as u32).unwrap();
        assert!(matches!(&sh.content, ShapeContent::Image(i) if i.mime == "image/png" && i.data == PNG));
    }

    #[test]
    fn virtual_path_background_reads_via_media_source() {
        let mut pres = make_pres();
        let src = FakeImgSource;
        let mut out = String::new();
        crate::media_context::with_media_source(Some(&src), || {
            out = run(&mut pres, r#"set_slide_background_image(0, "/inputs/logo.png")"#);
        });
        assert_eq!(out, "ok", "虚拟路径背景应成功");
        assert!(matches!(&pres.slides[0].background, Some(FillStyle::Picture(i)) if i.data == PNG));
    }

    #[test]
    fn virtual_path_with_non_image_bytes_fails() {
        let mut pres = make_pres();
        let src = FakeImgSource;
        let mut out = String::new();
        crate::media_context::with_media_source(Some(&src), || {
            out = run(&mut pres, r#"insert_slide_image(0, "/inputs/bad.png", 0.0, 0.0, 10.0, 10.0)"#);
        });
        assert_eq!(out, "-1");
        assert_eq!(pres.slides[0].shapes.iter().filter(|s| matches!(s.content, ShapeContent::Image(_))).count(), 0);
    }

    #[test]
    fn virtual_path_without_media_source_still_fails() {
        let mut pres = make_pres();
        let out = run(&mut pres, r#"insert_slide_image(0, "/inputs/logo.png", 0.0, 0.0, 10.0, 10.0)"#);
        assert_eq!(out, "-1");
    }

    #[test]
    fn insert_png_and_jpeg() {
        let mut pres = make_pres();
        let png = write_tmp("a.png", PNG);
        let jpg = write_tmp("b.jpg", JPG);
        let out = run(&mut pres, &format!(r#"insert_slide_image(0, "{png}", 10.0, 20.0, 100.0, 50.0)"#));
        let id = out.parse::<i64>().unwrap() as u32;
        let sh = pres.slides[0].shapes.iter().find(|s| s.id == id).unwrap();
        assert!(matches!(&sh.content, ShapeContent::Image(i) if i.mime == "image/png" && i.data == PNG));
        assert!(sh.style.fill.is_none() && sh.style.stroke.is_none());
        let out = run(&mut pres, &format!(r#"insert_slide_image(0, "{jpg}", 0.0, 0.0, 10.0, 10.0)"#));
        let id2 = out.parse::<i64>().unwrap() as u32;
        assert_ne!(id, id2);
        let sh = pres.slides[0].shapes.iter().find(|s| s.id == id2).unwrap();
        assert!(matches!(&sh.content, ShapeContent::Image(i) if i.mime == "image/jpeg"));
        let _ = std::fs::remove_file(png);
        let _ = std::fs::remove_file(jpg);
    }

    #[test]
    fn background_image_set() {
        let mut pres = make_pres();
        let png = write_tmp("bg.png", PNG);
        assert_eq!(run(&mut pres, &format!(r#"set_slide_background_image(0, "{png}")"#)), "ok");
        assert!(matches!(&pres.slides[0].background, Some(FillStyle::Picture(i)) if i.mime == "image/png"));
        let _ = std::fs::remove_file(png);
    }

    #[test]
    fn bad_input_reports_err_and_changes_nothing() {
        let mut pres = make_pres();
        let txt = write_tmp("c.txt", b"hello world");
        let empty = write_tmp("d.png", b"");
        let png = write_tmp("e.png", PNG);
        let n_shapes = pres.slides[0].shapes.len();
        fn assert_create_fails(pres: &mut Presentation, script: &str) {
            let out = run(pres, &format!("let r = {script}; r.to_string() + \"|\" + get_last_error()"));
            assert!(out.starts_with("-1|err:"), "{script} -> {out}");
        }
        for script in [
            format!(r#"insert_slide_image(0, "{txt}", 0.0, 0.0, 10.0, 10.0)"#),
            format!(r#"insert_slide_image(0, "{empty}", 0.0, 0.0, 10.0, 10.0)"#),
            r#"insert_slide_image(0, "/no/such/file.png", 0.0, 0.0, 10.0, 10.0)"#.to_string(),
            r#"insert_slide_image(0, "", 0.0, 0.0, 10.0, 10.0)"#.to_string(),
            r#"insert_slide_image(0, "/tmp", 0.0, 0.0, 10.0, 10.0)"#.to_string(),
            format!(r#"insert_slide_image(0, "{png}", 0.0, 0.0, -1.0, 10.0)"#),
            format!(r#"insert_slide_image(9, "{png}", 0.0, 0.0, 10.0, 10.0)"#),
        ] {
            assert_create_fails(&mut pres, &script);
        }
        for script in [
            format!(r#"set_slide_background_image(0, "{txt}")"#),
            format!(r#"set_slide_background_image(9, "{png}")"#),
            r#"set_slide_background_image(0, "/no/such/file.png")"#.to_string(),
        ] {
            let out = run(&mut pres, &script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        assert_eq!(pres.slides[0].shapes.len(), n_shapes);
        assert!(pres.slides[0].background.is_none());
        for p in [txt, empty, png] { let _ = std::fs::remove_file(p); }
    }

    #[test]
    fn without_presentation_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let png = write_tmp("f.png", PNG);
        assert_eq!(e.eval::<i64>(&format!(r#"insert_slide_image(0, "{png}", 0.0, 0.0, 10.0, 10.0)"#)).unwrap(), -1);
        for script in [
            format!(r#"set_slide_background_image(0, "{png}")"#),
        ] {
            let out = e.eval::<String>(&script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        let _ = std::fs::remove_file(png);
    }
}


#[cfg(test)]
mod readonly_tests {
    use crate::impress_api::READ_FN_NAMES;
    use crate::impress_api_meta::IMPRESS_API_META;
    use rhai::Engine;
    use std::collections::BTreeSet;

    fn registered(e: &Engine) -> BTreeSet<String> {
        e.gen_fn_signatures(false)
            .into_iter()
            .map(|s| s.split('(').next().unwrap_or("").trim().to_string())
            .collect()
    }

    fn meta_names() -> BTreeSet<String> {
        IMPRESS_API_META.iter().map(|m| m.0.to_string()).collect()
    }

    #[test]
    fn readonly_names_match_const() {
        let want: BTreeSet<String> = READ_FN_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(want.len(), READ_FN_NAMES.len(), "READ_FN_NAMES 有重名");
        assert_eq!(READ_FN_NAMES.len(), 18, "只读白名单数量应为 18");
        let mut e = Engine::new();
        crate::impress_api::register_readonly(&mut e);
        let mut reg = registered(&e);
        assert!(reg.remove("list_functions"), "只读引擎应注册只读版 list_functions");
        assert_eq!(reg, want, "仅注册有: {:?}; 仅白名单有: {:?}",
            reg.difference(&want).collect::<Vec<_>>(), want.difference(&reg).collect::<Vec<_>>());
    }

    #[test]
    fn meta_readonly_flags_match_const() {
        let flagged: BTreeSet<String> = IMPRESS_API_META.iter().filter(|m| m.3).map(|m| m.0.to_string()).collect();
        let want: BTreeSet<String> = READ_FN_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(flagged, want, "meta 只读标记与 READ_FN_NAMES 不一致,请重跑 scripts/gen_impress_api_meta.py");
    }

    #[test]
    fn readonly_names_in_meta() {
        let meta = meta_names();
        let reads: BTreeSet<String> = READ_FN_NAMES.iter().map(|s| s.to_string()).collect();
        assert!(reads.is_subset(&meta), "白名单含不在 meta 里的函数: {:?}",
            reads.difference(&meta).collect::<Vec<_>>());
    }

    #[test]
    fn full_engine_names_unchanged() {
        let mut e = Engine::new();
        crate::impress_api::register(&mut e);
        let reg = registered(&e);
        let meta = meta_names();
        assert_eq!(reg.len(), 54, "全量引擎注册名应为 54 个");
        assert_eq!(reg, meta, "仅注册有: {:?}; 仅 meta 有: {:?}",
            reg.difference(&meta).collect::<Vec<_>>(), meta.difference(&reg).collect::<Vec<_>>());
    }

    #[test]
    fn readonly_engine_rejects_writes_and_allows_reads() {
        let mut e = Engine::new();
        crate::impress_api::register_readonly(&mut e);
        for script in [
            r#"set_shape_text(0, 0, "x")"#,
            r#"insert_shape(0, "rect", 1.0, 1.0, 1.0, 1.0)"#,
            r##"set_shape_fill(0, 1, "#ffffff")"##,
            "delete_slide(0)",
        ] {
            let err = e.eval::<rhai::Dynamic>(script).unwrap_err().to_string();
            assert!(err.contains("Function not found"), "写函数应未注册: {script} => {err}");
        }
        // 未绑定演示文稿时读函数返回默认值,说明函数已注册且可调用
        assert_eq!(e.eval::<i64>("slide_count()").unwrap(), 0);
    }

    #[test]
    fn readonly_list_functions_only_reads() {
        let mut e = Engine::new();
        crate::impress_api::register_readonly(&mut e);
        let all = e.eval::<String>(r#"list_functions("")"#).unwrap();
        assert!(all.contains("get_slide_summary") && all.contains("inspect_presentation"), "应列出只读函数: {all}");
        assert!(all.contains("insert_connector[w]") && all.contains("insert_shape[w]") && all.contains("get_slide_summary[r]"),
            "应列出全部函数并带 [r]/[w]: {all}");
        let one = e.eval::<String>(r#"list_functions("save")"#).unwrap();
        assert!(one.contains("save_pptx[r]("), "按关键词应列出 save_pptx: {one}");
    }
}

#[cfg(test)]
mod slide_chart_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::impress::Presentation;

    const BAR: &str = r#"{\"kind\":\"Bar\",\"title\":null,\"x_labels\":[\"A\",\"B\"],\"series\":[{\"name\":\"S1\",\"data\":[1.0,2.0]}]}"#;
    const LINE: &str = r#"{\"kind\":\"Line\",\"title\":null,\"x_labels\":[\"A\",\"B\",\"C\"],\"series\":[{\"name\":\"S2\",\"data\":[3.0,2.0,1.0]}]}"#;

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    fn id_of(s: &str) -> i64 {
        s.trim().parse::<i64>().unwrap_or_else(|_| panic!("不是整数 id: {s}"))
    }

    #[test]
    fn chart_insert_and_get() {
        let mut pres = make_pres();
        let id = id_of(&run(&mut pres, &format!("insert_slide_chart(0, \"{BAR}\", 10.0, 20.0, 300.0, 200.0)")));
        let got = run(&mut pres, &format!("get_slide_chart_json(0, {id})"));
        let v: serde_json::Value = serde_json::from_str(&got).unwrap_or_else(|_| panic!("不是 JSON: {got}"));
        assert_eq!(v["kind"], "Bar");
        assert_eq!(v["id"].as_i64().unwrap(), id);
        assert_eq!(v["x_pt"].as_f64().unwrap(), 10.0);
        assert_eq!(v["width_pt"].as_f64().unwrap(), 300.0);
    }

    #[test]
    fn chart_set_keeps_id_and_rect() {
        let mut pres = make_pres();
        let id = id_of(&run(&mut pres, &format!("insert_slide_chart(0, \"{BAR}\", 10.0, 20.0, 300.0, 200.0)")));
        assert_eq!(run(&mut pres, &format!("set_slide_chart_json(0, {id}, \"{LINE}\")")), "ok");
        let got = run(&mut pres, &format!("get_slide_chart_json(0, {id})"));
        let v: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["kind"], "Line");
        assert_eq!(v["id"].as_i64().unwrap(), id);
        assert_eq!(v["x_pt"].as_f64().unwrap(), 10.0);
        assert_eq!(v["y_pt"].as_f64().unwrap(), 20.0);
        assert_eq!(v["height_pt"].as_f64().unwrap(), 200.0);
    }

    #[test]
    fn chart_errors_change_nothing() {
        let mut pres = make_pres();
        let tid = id_of(&run(&mut pres, r#"insert_textbox(0, 1.0, 1.0, 10.0, 10.0, "x")"#));
        let cid = id_of(&run(&mut pres, &format!("insert_slide_chart(0, \"{BAR}\", 0.0, 0.0, 100.0, 100.0)")));
        let before = pres.slides[0].shapes.len();
        fn assert_create_fails(pres: &mut Presentation, script: &str) {
            let out = run(pres, &format!("let r = {script}; r.to_string() + \"|\" + get_last_error()"));
            assert!(out.starts_with("-1|err:"), "{script} -> {out}");
        }
        for script in [
            r#"insert_slide_chart(0, "not json", 0.0, 0.0, 100.0, 100.0)"#.to_string(),
            format!("insert_slide_chart(0, \"{BAR}\", 0.0, 0.0, 0.0, 100.0)"),
            format!("insert_slide_chart(99, \"{BAR}\", 0.0, 0.0, 100.0, 100.0)"),
            r#"insert_slide_stat_chart(0, "{}", 0.0, 0.0, 100.0, 100.0)"#.to_string(),
            r#"insert_slide_stat_chart(0, "{}", 0.0, 0.0, 0.0, 100.0)"#.to_string(),
            r#"insert_slide_stat_chart(99, "{}", 0.0, 0.0, 100.0, 100.0)"#.to_string(),
        ] {
            assert_create_fails(&mut pres, &script);
        }
        for script in [
            format!("get_slide_chart_json(0, {tid})"),
            "get_slide_chart_json(0, 99999)".to_string(),
            "get_slide_chart_json(99, 0)".to_string(),
            format!("set_slide_chart_json(0, {tid}, \"{LINE}\")"),
            "set_slide_chart_json(0, 99999, \"{}\")".to_string(),
            format!("set_slide_chart_json(0, {cid}, \"not json\")"),
        ] {
            let out = run(&mut pres, &script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        assert_eq!(pres.slides[0].shapes.len(), before, "失败的调用不应新增形状");
        let v: serde_json::Value = serde_json::from_str(&run(&mut pres, &format!("get_slide_chart_json(0, {cid})"))).unwrap();
        assert_eq!(v["kind"], "Bar", "解析失败的 set 不应改动原图表");
    }

    #[test]
    fn without_presentation_returns_err() {
        let mut e = rhai::Engine::new();
        register(&mut e);
        assert_eq!(e.eval::<i64>("insert_slide_chart(0, \"{}\", 0.0, 0.0, 1.0, 1.0)").unwrap(), -1);
        assert_eq!(e.eval::<i64>("insert_slide_stat_chart(0, \"{}\", 0.0, 0.0, 1.0, 1.0)").unwrap(), -1);
        for script in [
            "get_slide_chart_json(0, 0)",
            "set_slide_chart_json(0, 0, \"{}\")",
        ] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }

    const STAT: &str = r#"{\"kind\":\"Histogram\",\"title_present\":false,\"title\":null,\"show_legend\":false,\"legend_pos\":null,\"show_grid\":true,\"gap_width\":null,\"data\":[{\"id\":0,\"categories\":[[]],\"values\":[1.0,2.0,null,4.0],\"value_kind\":\"val\"}],\"series\":[{\"name\":\"S\",\"layout_id\":\"clusteredColumn\",\"format_idx\":0,\"data_id\":0,\"bin_width\":null,\"bin_count\":null,\"interval_closed_right\":false,\"quartile_exclusive\":false,\"mean_line\":false,\"mean_marker\":false,\"show_outliers\":false,\"show_non_outliers\":false,\"subtotals\":[],\"data_labels\":null}]}"#;

    #[test]
    fn stat_chart_insert_success_and_no_json_access() {
        use roe_model::impress::ShapeContent;
        let mut pres = make_pres();
        let r = run(&mut pres, &format!("insert_slide_stat_chart(0, \"{STAT}\", 5.0, 6.0, 200.0, 120.0)"));
        let id = id_of(&r);
        let sh = pres.slides[0].shapes.iter().find(|s| s.id as i64 == id).expect("应新增形状");
        assert!(matches!(sh.content, ShapeContent::StatChart(_)), "内容应为统计图");
        assert_eq!(sh.position.x(), 5.0);
        assert_eq!(sh.position.w(), 200.0);
        for script in [
            format!("get_slide_chart_json(0, {id})"),
            format!("set_slide_chart_json(0, {id}, \"{{}}\")"),
            format!("set_slide_chart_json(0, {id}, \"{STAT}\")"),
        ] {
            let out = run(&mut pres, &script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        let sh = pres.slides[0].shapes.iter().find(|s| s.id as i64 == id).unwrap();
        assert!(matches!(sh.content, ShapeContent::StatChart(_)), "失败的调用不应改动统计图");
    }

    #[test]
    fn stat_chart_missing_fields_rejected() {
        let mut pres = make_pres();
        let before = pres.slides[0].shapes.len();
        let out = run(&mut pres, r#"insert_slide_stat_chart(0, "{\"kind\":\"Histogram\"}", 0.0, 0.0, 100.0, 100.0)"#);
        assert_eq!(out, "-1", "{out}");
        assert_eq!(pres.slides[0].shapes.len(), before);
    }
}

#[cfg(test)]
mod style_read_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::impress::Presentation;

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    fn id_of(s: &str) -> i64 {
        s.trim().parse::<i64>().unwrap_or_else(|_| panic!("不是整数 id: {s}"))
    }

    fn jv(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap_or_else(|_| panic!("不是 JSON: {s}"))
    }

    #[test]
    fn shape_style_reflects_setters() {
        let mut pres = make_pres();
        let id = id_of(&run(&mut pres, r#"insert_textbox(0, 1.0, 2.0, 100.0, 50.0, "x")"#));
        let v = jv(&run(&mut pres, &format!("get_shape_style(0, {id})")));
        assert_eq!(v["fill"], "none");
        assert_eq!(v["stroke"], "none");
        assert_eq!(v["stroke_w"].as_f64().unwrap(), 0.0);
        assert_eq!(v["shadow"], false);
        assert_eq!(run(&mut pres, &format!(r##"set_shape_fill(0, {id}, "#ff0000")"##)), "ok");
        assert_eq!(run(&mut pres, &format!(r##"set_shape_stroke(0, {id}, "#00ff00", 2.0)"##)), "ok");
        assert_eq!(run(&mut pres, &format!("set_shape_rotation(0, {id}, 45.0)")), "ok");
        let v = jv(&run(&mut pres, &format!("get_shape_style(0, {id})")));
        assert_eq!(v["fill"], "#ff0000");
        assert_eq!(v["fill_detail"]["type"], "solid");
        assert_eq!(v["fill_detail"]["color"]["r"].as_u64().unwrap(), 255);
        assert_eq!(v["stroke"], "#00ff00");
        assert_eq!(v["stroke_w"].as_f64().unwrap(), 2.0);
        assert_eq!(v["rotation"].as_f64().unwrap(), 45.0);
        let pos = pres.slides[0].shapes.iter().position(|s| s.id as i64 == id).unwrap();
        assert_eq!(v["z_index"].as_u64().unwrap() as usize, pos);
    }

    #[test]
    fn shape_style_errors() {
        let mut pres = make_pres();
        for script in ["get_shape_style(0, 99999)", "get_shape_style(99, 0)", "get_shape_style(-1, 0)", "get_shape_style(0, -1)"] {
            let out = run(&mut pres, script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }

    #[test]
    fn background_roundtrip() {
        let mut pres = make_pres();
        assert_eq!(run(&mut pres, r#"set_slide_background(0, `{"type":"none"}`)"#), "");
        assert_eq!(jv(&run(&mut pres, "get_slide_background(0)"))["type"], "none");

        let solid = r#"{"type":"solid","color":{"r":10,"g":20,"b":30}}"#;
        assert_eq!(run(&mut pres, &format!("set_slide_background(0, `{solid}`)")), "");
        let v = jv(&run(&mut pres, "get_slide_background(0)"));
        assert_eq!(v["type"], "solid");
        assert_eq!(v["color"]["r"].as_u64().unwrap(), 10);
        assert_eq!(v["color"]["a"].as_u64().unwrap(), 255);

        let grad = r#"{"type":"gradient","stops":[{"pos":0.0,"color":{"r":255,"g":0,"b":0}},{"pos":1.0,"color":{"r":0,"g":0,"b":255}}],"kind":"linear","angle":90.0}"#;
        assert_eq!(run(&mut pres, &format!("set_slide_background(0, `{grad}`)")), "");
        let got = run(&mut pres, "get_slide_background(0)");
        let v = jv(&got);
        assert_eq!(v["type"], "gradient");
        assert_eq!(v["stops"].as_array().unwrap().len(), 2);
        assert_eq!(v["kind"], "linear");
        assert_eq!(v["angle"].as_f64().unwrap(), 90.0);
        // 读出来的 JSON 可以原样喂回去
        assert_eq!(run(&mut pres, &format!("set_slide_background(0, `{got}`)")), "");
        assert_eq!(jv(&run(&mut pres, "get_slide_background(0)")), v);
    }

    #[test]
    fn background_errors_and_no_presentation() {
        let mut pres = make_pres();
        for script in ["get_slide_background(99)", "get_slide_background(-1)"] {
            let out = run(&mut pres, script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        let mut e = rhai::Engine::new();
        register(&mut e);
        for script in ["get_slide_background(0)", "get_shape_style(0, 0)"] {
            let out = e.eval::<String>(script).unwrap();
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
    }
}

#[cfg(test)]
mod char_read_tests {
    use crate::impress_api::tests::make_pres;
    use crate::impress_api::{register, with_presentation};
    use roe_model::impress::Presentation;

    fn run(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || { out = e.eval::<rhai::Dynamic>(script).unwrap().to_string(); });
        out
    }

    fn id_of(s: &str) -> i64 {
        s.trim().parse::<i64>().unwrap_or_else(|_| panic!("不是整数 id: {s}"))
    }

    fn jv(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap_or_else(|_| panic!("不是 JSON: {s}"))
    }

    #[test]
    fn char_format_by_id_reflects_style() {
        let mut pres = make_pres();
        let id = id_of(&run(&mut pres, r#"insert_textbox(0, 1.0, 1.0, 100.0, 50.0, "hi")"#));
        let v = jv(&run(&mut pres, &format!("get_shape_char_format_by_id(0, {id}, 0, 0)")));
        assert!(v["color"].is_null());
        assert!(v["background"].is_null());
        assert!(v["script"].is_null());
        assert_eq!(v["bold"], false);
        assert_eq!(run(&mut pres, &format!(r##"set_shape_text_style(0, {id}, "color:#ff0000")"##)), "ok");
        assert_eq!(run(&mut pres, &format!(r##"set_shape_text_style(0, {id}, "background:#00ff00")"##)), "ok");
        assert_eq!(run(&mut pres, &format!(r#"set_shape_text_style(0, {id}, "bold:1")"#)), "ok");
        assert_eq!(run(&mut pres, &format!(r#"set_shape_text_style(0, {id}, "size:30")"#)), "ok");
        let v = jv(&run(&mut pres, &format!("get_shape_char_format_by_id(0, {id}, 0, 0)")));
        assert_eq!(v["color"], "#ff0000");
        assert_eq!(v["background"], "#00ff00");
        assert_eq!(v["bold"], true);
        assert_eq!(v["font_size"].as_f64().unwrap(), 30.0);
    }

    #[test]
    fn char_format_by_id_errors() {
        let mut pres = make_pres();
        let tid = id_of(&run(&mut pres, r#"insert_textbox(0, 1.0, 1.0, 100.0, 50.0, "hi")"#));
        let eid = id_of(&run(&mut pres, r#"insert_textbox(0, 1.0, 1.0, 100.0, 50.0, "")"#));
        let tbl = id_of(&run(&mut pres, "insert_slide_table(0, 0.0, 0.0, 100.0, 50.0, 1, 1)"));
        for script in [
            format!("get_shape_char_format_by_id(0, {tid}, 5, 0)"),
            format!("get_shape_char_format_by_id(0, {tid}, 0, 5)"),
            format!("get_shape_char_format_by_id(0, {tid}, -1, 0)"),
            format!("get_shape_char_format_by_id(0, {tid}, 0, -1)"),
            format!("get_shape_char_format_by_id(0, {eid}, 0, 0)"),
            format!("get_shape_char_format_by_id(0, {tbl}, 0, 0)"),
            "get_shape_char_format_by_id(0, 99999, 0, 0)".to_string(),
            "get_shape_char_format_by_id(99, 0, 0, 0)".to_string(),
        ] {
            let out = run(&mut pres, &script);
            assert!(out.starts_with("err:"), "{script} -> {out}");
        }
        let mut e = rhai::Engine::new();
        register(&mut e);
        let out = e.eval::<String>("get_shape_char_format_by_id(0, 0, 0, 0)").unwrap();
        assert!(out.starts_with("err:"), "{out}");
    }
}

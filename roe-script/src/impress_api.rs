//! Impress API：get_shape_text / set_shape_text / slide_count / shape_count

use rhai::Engine;
use base64::Engine as _;
use roe_model::common::StyleRef;
use roe_model::impress::{Presentation, ShapeContent, Slide, AnimationNode, TextVert, Shape, TextBodyProps, TextAutofit, TextAnchor};
use roe_model::common::{ShapeGeometry, ShapeStyle, Transform, FillStyle, GradientKind, GradientStop, Color, ImageData};
use roe_model::style::Alignment;
use roe_model::writer::{Paragraph, Run};
use std::cell::RefCell;

#[path = "impress_api_ext.rs"]
mod ext;

thread_local! {
    static PRESENTATION: RefCell<Option<*mut Presentation>> = const { RefCell::new(None) };
    // 最近一次创建类函数(返回 id、失败 -1)的失败原因;每次绑定演示文稿(一次脚本)时清空。
    static LAST_ERR: RefCell<String> = const { RefCell::new(String::new()) };
}

pub(crate) fn set_last_err(m: impl Into<String>) {
    LAST_ERR.with(|c| *c.borrow_mut() = m.into());
}

pub fn with_presentation<F>(pres: &mut Presentation, f: F)
where F: FnOnce() {
    LAST_ERR.with(|c| c.borrow_mut().clear());
    PRESENTATION.with(|cell| { *cell.borrow_mut() = Some(pres as *mut Presentation); });
    f();
    PRESENTATION.with(|cell| { *cell.borrow_mut() = None; });
}

fn with_pres<R, F: FnOnce(&mut Presentation) -> R>(f: F) -> Option<R> {
    PRESENTATION.with(|cell| {
        let ptr = (*cell.borrow())?;
        Some(f(unsafe { &mut *ptr }))
    })
}

/// 当前线程是否绑定了演示文稿(run_impress 执行期间为 true)。供 list_functions 按文档类型分流。
#[allow(dead_code)]
pub(crate) fn has_presentation() -> bool {
    PRESENTATION.with(|cell| cell.borrow().is_some())
}

/// 只读函数集(纯查询,不修改演示文稿)。register 与 register_readonly 共用。
fn register_read_fns(engine: &mut Engine) {
    // get_last_error:最近一次创建类函数失败(返回 -1)的原因;没有失败返回空串。
    engine.register_fn("get_last_error", || -> String { LAST_ERR.with(|c| c.borrow().clone()) });
    engine.register_fn("slide_count", || -> i64 {
        with_pres(|p| p.slides.len() as i64).unwrap_or(0)
    });

    engine.register_fn("shape_count", |slide: i64| -> i64 {
        with_pres(|p| p.slides.get(slide as usize).map(|s| s.shapes.len() as i64))
            .flatten()
            .unwrap_or(0)
    });

    // get_shape_text：把 Vec<Paragraph> 的所有 run 文本拼接返回
    engine.register_fn("get_shape_text", |slide: i64, shape: i64| -> String {
        with_pres(|p| {
            let s = p.slides.get(slide as usize)?;
            let shape = s.shapes.get(shape as usize)?;
            match &shape.content {
                ShapeContent::Text(paras) => {
                    let text = paras.iter()
                        .flat_map(|p| p.runs.iter())
                        .map(|r| r.text.as_str())
                        .collect::<Vec<_>>()
                        .join("");
                    Some(text)
                }
                _ => None,
            }
        })
        .flatten()
        .unwrap_or_default()
    });

    // list_masters：列出当前文档所有母版，返回JSON数组字符串
    // `[{"index":0,"name":"...","shape_count":N},...]`。index 即
    // master_slides 数组下标，与 Slide.master_idx / insert_slide 的
    // master_idx 参数是同一个坐标系，可直接互通。无母版时返回"[]"。
    // 仅基础查看用途（gap analysis 3.4节"不要求完整母版编辑器"），
    // 不返回 shapes 明细，需要形状级细节应另行查询。
    engine.register_fn("list_masters", || -> String {
        with_pres(|p| {
            let items: Vec<String> = p
                .master_slides
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    format!(
                        "{{\"index\":{},\"name\":{},\"shape_count\":{}}}",
                        i,
                        serde_json::to_string(&m.name).unwrap_or_else(|_| "\"\"".to_string()),
                        m.shapes.len()
                    )
                })
                .collect();
            format!("[{}]", items.join(","))
        })
        .unwrap_or_else(|| "[]".to_string())
    });

    // get_shape_char_format(slide, shape, para_idx, run_idx) -> JSON
    engine.register_fn("get_shape_char_format", |slide: i64, shape: i64, para_idx: i64, run_idx: i64| -> String {
        with_pres(|p| {
            let s = p.slides.get(slide as usize)?;
            let sh = s.shapes.get(shape as usize)?;
            if let ShapeContent::Text(paras) = &sh.content {
                if let Some(para) = paras.get(para_idx as usize) {
                    if let Some(run) = para.runs.get(run_idx as usize) {
                        return Some(serde_json::json!({
                            "bold":        run.bold.unwrap_or(false),
                            "italic":      run.italic.unwrap_or(false),
                            "underline":   run.underline.unwrap_or(false),
                            "font_size":   run.font_size,
                            "font_family": run.font_family,
                            "char_style":  run.char_style.as_ref().map(|s| &s.0),
                        }).to_string());
                    }
                }
            }
            None
        })
        .flatten()
        .unwrap_or_else(|| "{}".to_string())
    });

    // get_shape_paragraph_format(slide, shape, para_idx) -> JSON
    engine.register_fn("get_shape_paragraph_format", |slide: i64, shape: i64, para_idx: i64| -> String {
        with_pres(|p| {
            let s = p.slides.get(slide as usize)?;
            let sh = s.shapes.get(shape as usize)?;
            if let ShapeContent::Text(paras) = &sh.content {
                if let Some(para) = paras.get(para_idx as usize) {
                    let align_str = match para.alignment {
                        Some(Alignment::Left)    => "left",
                        Some(Alignment::Center)  => "center",
                        Some(Alignment::Right)   => "right",
                        Some(Alignment::Justify) => "justify",
                        None                     => "",
                    };
                    return Some(serde_json::json!({
                        "style":        para.style.0,
                        "alignment":    align_str,
                        "indent_left":  para.indent_left,
                        "indent_first": para.indent_first,
                        "space_before": para.space_before,
                        "space_after":  para.space_after,
                    }).to_string());
                }
            }
            None
        })
        .flatten()
        .unwrap_or_else(|| "{}".to_string())
    });

    // save_pptx() -> String
    // 把当前 Presentation 序列化为 pptx 字节流，返回 base64 编码字符串。
    // 与 roe_save_document_to_ooxml_bytes（C-ABI）走同一条 PptxWriter。
    // 写出范围对齐 pptx_writer.rs 文件头注释（slide_size+shape位置/
    // geometry/fill/stroke宽度/文字或图片，不含动画/转场/备注/母版真实
    // 内容，详见该文件头注释）。失败返回空字符串。
    engine.register_fn("save_pptx", || -> String {
        with_pres(|pres| {
            match roe_parser::ooxml::pptx_writer::PptxWriter::write(pres) {
                Ok(bytes) => base64::engine::general_purpose::STANDARD.encode(bytes),
                Err(_) => String::new(),
            }
        })
        .unwrap_or_default()
    });

    // get_slide_summary(slide_idx) -> JSON String
    // 只读摘要接口：列出该 slide 上所有形状的类型/位置/内容概览/占位符身份/
    // 是否有填充，供 AI Agent 快速了解页面结构，不做逐字段全量导出（细节
    // 需要专门查询接口，如 get_shape_text/roe_impress_query_shape_rect）。
    // 设计要点（本次会话与用户逐项讨论确认）：
    // - geometry：ShapeGeometry 枚举名小写字符串
    // - content.type：text/image/table/chart/group/empty；text 类型把所有
    //   run 文本拼接返回（不截断，不做 run 级格式展开）；image 只给 mime，
    //   不带二进制；chart 给 kind/title/series_count，不展开数据点；
    //   group 只给 children_ids 列表，不递归展开子形状全部字段
    // - style：只给 has_fill/fill_kind，不展开具体颜色值/渐变色标
    // - 不含动画信息（动画留给专门的播放功能相关接口）
    // - slide_idx 越界返回 "null" 字符串（与本文件其余查询函数的"越界返回
    //   空/默认值不报错"风格一致，不 panic）
    engine.register_fn("get_slide_summary", |slide_idx: i64| -> String {
        with_pres(|p| {
            let s = p.slides.get(slide_idx as usize)?;
            let shapes: Vec<serde_json::Value> = s.shapes.iter().map(|shape| {
                let geometry = match &shape.geometry {
                    ShapeGeometry::Rect => "rect".to_string(),
                    ShapeGeometry::RoundRect { .. } => "round_rect".to_string(),
                    ShapeGeometry::Ellipse => "ellipse".to_string(),
                    ShapeGeometry::Triangle => "triangle".to_string(),
                    ShapeGeometry::Line => "line".to_string(),
                    ShapeGeometry::PresetPath(_) => "preset_path".to_string(),
                    ShapeGeometry::Custom(_) => "custom".to_string(),
                };

                let content = match &shape.content {
                    ShapeContent::Text(paras) => {
                        let text = paras.iter()
                            .flat_map(|para| para.runs.iter())
                            .map(|r| r.text.as_str())
                            .collect::<Vec<_>>()
                            .join("");
                        serde_json::json!({ "type": "text", "text": text })
                    }
                    ShapeContent::Image(img) => {
                        serde_json::json!({ "type": "image", "mime": img.mime })
                    }
                    ShapeContent::Table(table) => {
                        serde_json::json!({
                            "type": "table",
                            "rows": table.rows.len(),
                            "cols": table.rows.first().map(|r| r.cells.len()).unwrap_or(0),
                        })
                    }
                    ShapeContent::Chart(chart) => {
                        serde_json::json!({
                            "type": "chart",
                            "kind": format!("{:?}", chart.kind),
                            "title": chart.title,
                            "series_count": chart.series.len(),
                        })
                    }
                    ShapeContent::StatChart(sc) => {
                        serde_json::json!({
                            "type": "stat_chart",
                            "kind": format!("{:?}", sc.kind),
                            "title": sc.title,
                            "series_count": sc.series.len(),
                        })
                    }
                    ShapeContent::Group { children, .. } => {
                        serde_json::json!({
                            "type": "group",
                            "children_ids": children.iter().map(|c| c.id).collect::<Vec<_>>(),
                        })
                    }
                    ShapeContent::Empty => serde_json::json!({ "type": "empty" }),
                };

                let fill_kind = shape.style.fill.as_ref().map(|f| match f {
                    FillStyle::Solid(_) => "solid",
                    FillStyle::Gradient { .. } => "gradient",
                    FillStyle::Picture(_) => "picture",
                    FillStyle::PictureTiled { .. } => "picture_tiled",
                    FillStyle::None => "none",
                });

                serde_json::json!({
                    "id": shape.id,
                    "geometry": geometry,
                    "position": {
                        "x": shape.position.x(),
                        "y": shape.position.y(),
                        "w": shape.position.w(),
                        "h": shape.position.h(),
                        "rotation": shape.position.rotation.unwrap_or(0.0),
                    },
                    "placeholder": shape.placeholder.as_ref().map(|ph| serde_json::json!({
                        "ph_type": ph.ph_type,
                        "index": ph.index,
                    })),
                    "content": content,
                    "style": {
                        "has_fill": !matches!(shape.style.fill, None | Some(FillStyle::None)),
                        "fill_kind": fill_kind,
                    },
                })
            }).collect();

            Some(serde_json::json!({
                "slide_idx": slide_idx,
                "shape_count": s.shapes.len(),
                "shapes": shapes,
            }).to_string())
        })
        .flatten()
        .unwrap_or_else(|| "null".to_string())
    });

}

/// 只读函数白名单(register_read_fns + ext::register_read 的全部函数,有测试守着;不含 list_functions)。
pub const READ_FN_NAMES: &[&str] = &[
    "get_last_error",
    "slide_count",
    "shape_count",
    "get_shape_text",
    "list_masters",
    "get_shape_char_format",
    "get_shape_paragraph_format",
    "save_pptx",
    "get_slide_summary",
    "inspect_presentation",
    "list_layouts",
    "get_shape_rect",
    "get_shape_text_by_id",
    "get_shape_table_cell",
    "get_slide_chart_json",
    "get_shape_style",
    "get_slide_background",
    "get_shape_char_format_by_id",
];

/// 只读引擎用:只注册纯查询函数,不注册任何写函数。
pub fn register_readonly(engine: &mut Engine) {
    register_read_fns(engine);
    ext::register_read(engine);
    crate::calc_api_list::register_impress_readonly_list(engine);
}

/// 写函数统一回报:Some(Ok)->"ok";Some(Err(m))->m(已带 err: 前缀);None(未绑定文档)->err。
fn write_ret(r: Option<Result<(), String>>) -> String {
    match r {
        Some(Ok(())) => "ok".to_string(),
        Some(Err(m)) => m,
        None => "err:当前没有绑定演示文稿".to_string(),
    }
}

pub fn register(engine: &mut Engine) {
    ext::register(engine);
    register_read_fns(engine);



    // set_shape_text：写入单段落单 run
        engine.register_fn("set_shape_text", |slide: i64, shape: i64, text: String| -> String {
        write_ret(with_pres(|p| -> Result<(), String> {
            let n = p.slides.len();
            let s = p.slides.get_mut(slide as usize)
                .ok_or_else(|| format!("err:slide={} 越界(共 {} 页)", slide, n))?;
            let m = s.shapes.len();
            let sh = s.shapes.get_mut(shape as usize)
                .ok_or_else(|| format!("err:shape 下标={} 越界(该页共 {} 个形状;shape 是页内下标,不是 shape_id)", shape, m))?;
            let para = Paragraph {
                style: StyleRef("Default".into()),
                runs: vec![Run { text: text.clone(), char_style: None, ..Default::default() }],
                list_info: None,
                ..Default::default()
            };
            sh.content = ShapeContent::Text(vec![para]);
            Ok(())
        }))
    });

    // insert_slide：在 index 处插入一张空白 slide（插入后新页下标 = index,原第 index 页后移;
    // 注意与 insert_slide_with_layout 的 after 不同,后者是插在第 after 页之后）。
    // master_idx < 0 表示不挂 master（None）;>= 0 须落在 master_slides 范围内,否则失败。
    // v304 起返回 i64:成功为新页下标(clamp 之后),失败 -1,原因记入 get_last_error()。
    // index 越界（负数或 > len）时 clamp 到合法范围插入，不 panic、不报错，与其余函数的
    // 越界容错风格保持一致（slide_count/shape_count 越界返回默认值而非报错）。
    engine.register_fn("insert_slide", |at: i64, master_idx: i64| -> i64 {
        match with_pres(|p| -> Result<i64, String> {
            let len = p.slides.len();
            let idx = at.max(0).min(len as i64) as usize;
            let mn = p.master_slides.len();
            if master_idx >= 0 && master_idx as usize >= mn {
                return Err(format!("err:master_idx={} 越界(共 {} 个母版,先用 list_masters 查看;-1 表示不挂母版)", master_idx, mn));
            }
            let m_idx = if master_idx < 0 { None } else { Some(master_idx as usize) };
            let slide = Slide { layout_idx: None,
                shapes: vec![],
                master_idx: m_idx,
                animation_tree: AnimationNode::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            };
            p.slides.insert(idx, slide);
            Ok(idx as i64)
        }) {
            Some(Ok(i)) => i,
            Some(Err(e)) => { set_last_err(e); -1 }
            None => { set_last_err("err:无打开的演示文稿"); -1 }
        }
    });

    // delete_slide：删除 index 处的 slide。成功 "ok",越界返回 "err:..."(不 panic)。
    engine.register_fn("delete_slide", |index: i64| -> String {
        write_ret(with_pres(|p| -> Result<(), String> {
            let n = p.slides.len();
            if index < 0 || index as usize >= n {
                return Err(format!("err:slide={} 越界(共 {} 页)", index, n));
            }
            p.slides.remove(index as usize);
            Ok(())
        }))
    });

    // duplicate_slide：复制 index 处的 slide 到其后一位。成功 "ok",越界 "err:..."。
    // 整体 clone（含 shapes 和 animation_tree）：shape id 只需在单个 slide 内唯一，
    // 不需要跨 slide 全局唯一，因此复制后 animation_tree 里按 id 引用 shape 的逻辑
    // （如 InteractiveSequence.trigger_shape_id）在新 slide 内仍然自洽，不需要重新分配 id。
    engine.register_fn("duplicate_slide", |index: i64| -> String {
        write_ret(with_pres(|p| -> Result<(), String> {
            let n = p.slides.len();
            if index < 0 || index as usize >= n {
                return Err(format!("err:slide={} 越界(共 {} 页)", index, n));
            }
            let idx = index as usize;
            let cloned = p.slides[idx].clone();
            p.slides.insert(idx + 1, cloned);
            Ok(())
        }))
    });


    // set_slide_master：切换 index 处 slide 的挂载母版。master_idx < 0 表示卸载母版。
    // master_idx 必须落在 master_slides 范围内,否则返回 "err:..."(v303 起校验;
    // 此前静默不挂载,AI 写错下标无从发现)。slide 越界同样报错。成功 "ok"。
    engine.register_fn("set_slide_master", |index: i64, master_idx: i64| -> String {
        write_ret(with_pres(|p| -> Result<(), String> {
            let n = p.slides.len();
            if index < 0 || index as usize >= n {
                return Err(format!("err:slide={} 越界(共 {} 页)", index, n));
            }
            let mn = p.master_slides.len();
            if master_idx >= 0 && master_idx as usize >= mn {
                return Err(format!("err:master_idx={} 越界(共 {} 个母版,先用 list_masters 查看;-1 表示卸载母版)", master_idx, mn));
            }
            p.slides[index as usize].master_idx = if master_idx < 0 { None } else { Some(master_idx as usize) };
            Ok(())
        }))
    });

    // insert_shape：在指定slide插入一个空白基础形状，返回新形状id（失败返回-1）。
    // geometry_type 原生变体接受 "rect"/"ellipse"/"line"（不查预设表，
    // 直接对应 ShapeGeometry 的原生枚举分支，与解析层pptx.rs对Rect的
    // 处理一致——Rect本身虽也在prst命名体系里，但作为最常用形状+最终
    // fallback，保留原生分支不查表，避免每次插入矩形都走一遍HashMap
    // 构造+公式求值的开销）。其余字符串查 find_preset_shape（187种
    // 预设形状公式表，roe-model/src/preset_geometry.rs，与pptx.rs解析
    // 已有文件时复用同一张表，见其"通用预设形状"分支注释），查到则用
    // eval_preset_shape 求值出 PresetPath（新建场景用全部默认adj值，
    // 不支持插入时就指定adj覆盖——这是"从无到有"与"精确调整"的分阶段，
    // 覆盖值可后续通过其余形状编辑接口补）；查不到（未覆盖的形状或
    // 拼写错误）fallback到Rect，与此前"不认识的字符串统一Rect"的
    // 容错风格保持一致，不 panic。
    // id 分配沿用 roe-api/src/clipboard.rs 已有先例：slide 内最大 id + 1，
    // 保证 slide 内唯一（跨 slide 不要求全局唯一，见 duplicate_slide 的设计说明）。
    engine.register_fn("insert_shape", |slide: i64, geometry_type: String, x: f64, y: f64, width: f64, height: f64| -> i64 {
        with_pres(|p| {
            let n = p.slides.len();
            let Some(s) = p.slides.get_mut(slide as usize) else {
                set_last_err(format!("err:slide={} 越界(共 {} 页)", slide, n));
                return None;
            };
            let geometry = match geometry_type.as_str() {
                "rect" => ShapeGeometry::Rect,
                "ellipse" => ShapeGeometry::Ellipse,
                "line" => ShapeGeometry::Line,
                other => {
                    match roe_model::preset_geometry::find_preset_shape(other) {
                        Some(formula) => {
                            let empty_overrides: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
                            let path = roe_model::preset_geometry::eval_preset_shape(formula, width, height, &empty_overrides);
                            // v181: other 是真实 prst 名字（来自 geometry_type
                            // 参数），adj_overrides 恒为空表（新建场景不支持
                            // 插入时指定adj，见上方"新建场景用全部默认adj值"
                            // 注释），如实记录为空 Vec。
                            ShapeGeometry::PresetPath(roe_model::common::PresetPathData {
                                commands: path,
                                preset_name: other.to_string(),
                                adj_overrides: Vec::new(),
                            })
                        }
                        None => ShapeGeometry::Rect,
                    }
                }
            };
            let next_id = s.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
            let shape = Shape {
                id: next_id,
                geometry,
                position: Transform::from_rect(x as f32, y as f32, width as f32, height as f32),
                style: ShapeStyle::default(),
                content: ShapeContent::Text(vec![]),
                text_body: TextBodyProps {
                    autofit: TextAutofit::NoAutofit,
                    wrap: true,
                    anchor: TextAnchor::Top,
                    ins_l: None, ins_r: None, ins_t: None, ins_b: None,
                    vert: TextVert::default(),
                },
                placeholder: None,
                st_cxn: None,
                end_cxn: None,
            };
            s.shapes.push(shape);
            Some(next_id as i64)
        })
        .flatten()
        .unwrap_or_else(|| {
            if !has_presentation() { set_last_err("err:当前没有绑定演示文稿"); }
            -1
        })
    });

    // delete_shape：按 shape_id（不是index，id更稳定，与clipboard粘贴逻辑一致）
    // 删除指定slide内的形状。成功 "ok",slide 越界或 id 不存在返回 "err:..."。
    engine.register_fn("delete_shape", |slide: i64, shape_id: i64| -> String {
        write_ret(with_pres(|p| -> Result<(), String> {
            let n = p.slides.len();
            if slide < 0 || slide as usize >= n {
                return Err(format!("err:slide={} 越界(共 {} 页)", slide, n));
            }
            let s = &mut p.slides[slide as usize];
            let before = s.shapes.len();
            s.shapes.retain(|sh| sh.id as i64 != shape_id);
            if s.shapes.len() == before {
                return Err(format!("err:第 {} 页没有 shape_id={} 的形状(shape_id 不是页内下标)", slide, shape_id));
            }
            Ok(())
        }))
    });

    // ---- 连接线（Connector）：PROOT-002 遗留缺口的收尾。折线/曲线的渲染路径
    // 早已由187种预设形状公式表（find_preset_shape/eval_preset_shape）覆盖，
    // 真正缺的只是"创建接口"，见 roe-ext_progress_v178 第三节调研记录。
    // line_type 三档粗粒度设计：先只暴露 straight/bent/curved，内部各自固定
    // 映射到一个代表性 prst（bent/curved 各有多个变体如 bentConnector2-5，
    // 全部默认adj居中效果最接近直觉预期，精细选择留待后续需求明确后再加参数）。
    // 未知字符串 fallback 到 straight，与 insert_shape 对未知 geometry_type 的
    // 容错风格一致，不 panic。

    // insert_connector：创建一条连接线 Shape，可选绑定两端到已有 shape 的连接点
    // (from_shape_id/from_idx, to_shape_id/to_idx)。shape_id 传负数表示该端为
    // 自由端点（不绑定），对应 st_cxn/end_cxn 写 None。
    engine.register_fn("insert_connector",
        |slide: i64, line_type: String, x: f64, y: f64, width: f64, height: f64,
         from_shape_id: i64, from_idx: i64, to_shape_id: i64, to_idx: i64| -> i64 {
        with_pres(|p| {
            let n = p.slides.len();
            let Some(s) = p.slides.get_mut(slide as usize) else {
                set_last_err(format!("err:slide={} 越界(共 {} 页)", slide, n));
                return None;
            };
            let prst = match line_type.as_str() {
                "bent" => "bentConnector3",
                "curved" => "curvedConnector3",
                _ => "straightConnector1",
            };
            let formula = roe_model::preset_geometry::find_preset_shape(prst)?;
            let empty_overrides: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
            let path = roe_model::preset_geometry::eval_preset_shape(formula, width, height, &empty_overrides);
            let next_id = s.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
            // v181: prst 是真实 prst 名字（来自 line_type 映射出的静态字符串），
            // adj_overrides 恒为空表（同 insert_shape，新建场景不支持指定adj）。
            let shape = Shape {
                id: next_id,
                geometry: ShapeGeometry::PresetPath(roe_model::common::PresetPathData {
                    commands: path,
                    preset_name: prst.to_string(),
                    adj_overrides: Vec::new(),
                }),
                position: Transform::from_rect(x as f32, y as f32, width as f32, height as f32),
                style: ShapeStyle::default(),
                content: ShapeContent::Text(vec![]),
                text_body: TextBodyProps {
                    autofit: TextAutofit::NoAutofit,
                    wrap: true,
                    anchor: TextAnchor::Top,
                    ins_l: None, ins_r: None, ins_t: None, ins_b: None,
                    vert: TextVert::default(),
                },
                placeholder: None,
                st_cxn: if from_shape_id < 0 { None } else { Some((from_shape_id as u32, from_idx as u32)) },
                end_cxn: if to_shape_id < 0 { None } else { Some((to_shape_id as u32, to_idx as u32)) },
            };
            s.shapes.push(shape);
            Some(next_id as i64)
        })
        .flatten()
        .unwrap_or_else(|| {
            if !has_presentation() { set_last_err("err:当前没有绑定演示文稿"); }
            -1
        })
    });

    // set_connector_arrows：设置指定 shape（按id查找，与 delete_shape 的id语义
    // 一致，不是index）的两端箭头样式。head_arrow/tail_arrow 目前完全没有编辑
    // 接口（只在解析层读取用），本函数补上，否则 insert_connector 创建的连接线
    // 永远没有箭头。字符串到 ArrowType 的映射："arrow"/"triangle"/"oval"/"none"，
    // 未知字符串 fallback 到 ArrowType::None（与 ArrowType 现有注释的容错风格
    // 一致：不认识的类型不画任何装饰，不panic）。
    engine.register_fn("set_connector_arrows",
        |slide: i64, shape_id: i64, head_arrow: String, tail_arrow: String| -> String {
        write_ret(with_pres(|p| -> Result<(), String> {
            let n = p.slides.len();
            if slide < 0 || slide as usize >= n {
                return Err(format!("err:slide={} 越界(共 {} 页)", slide, n));
            }
            let s = &mut p.slides[slide as usize];
            let sh = s.shapes.iter_mut().find(|sh| sh.id as i64 == shape_id)
                .ok_or_else(|| format!("err:第 {} 页没有 shape_id={} 的形状(shape_id 不是页内下标)", slide, shape_id))?;
            let to_arrow_type = |v: &str| -> roe_model::common::ArrowType {
                match v {
                    "arrow" => roe_model::common::ArrowType::Arrow,
                    "triangle" => roe_model::common::ArrowType::Triangle,
                    "oval" => roe_model::common::ArrowType::Oval,
                    _ => roe_model::common::ArrowType::None,
                }
            };
            sh.style.head_arrow = Some(to_arrow_type(&head_arrow));
            sh.style.tail_arrow = Some(to_arrow_type(&tail_arrow));
            Ok(())
        }))
    });

    // ---- 文本框内段落/字符格式：与 Writer 侧 set_char_format/set_paragraph_format
    // 同一套字段设计（复用 roe_model::writer::{Paragraph, Run} 数据结构），但不能
    // 直接复用 Writer 的函数实现——Writer 用独立的 WRITER_DOC thread_local 绑定
    // WriterDocument.body，Impress 用 PRESENTATION thread_local 绑定 Presentation，
    // 两者是互不相关的全局状态，Rhai register_fn 也不支持同名重载，因此在此重新
    // 实现一遍相同逻辑，函数名加 shape_ 前缀避免与 Writer 侧同名冲突。
    // 定位路径比 Writer 多两层：slide -> shape -> (仅当 content 是 Text 时) paragraph -> run。

    // set_shape_char_format(slide, shape, para_idx, run_idx, bold, italic, underline, font_size)
        engine.register_fn("set_shape_char_format",
        |slide: i64, shape: i64, para_idx: i64, run_idx: i64, bold: bool, italic: bool, underline: bool, font_size: f64| -> String {
        write_ret(with_pres(|p| -> Result<(), String> {
            let n = p.slides.len();
            let s = p.slides.get_mut(slide as usize)
                .ok_or_else(|| format!("err:slide={} 越界(共 {} 页)", slide, n))?;
            let m = s.shapes.len();
            let sh = s.shapes.get_mut(shape as usize)
                .ok_or_else(|| format!("err:shape 下标={} 越界(该页共 {} 个形状;shape 是页内下标,不是 shape_id)", shape, m))?;
            let ShapeContent::Text(paras) = &mut sh.content else {
                return Err("err:该形状不是文本形状".to_string());
            };
            let np = paras.len();
            let para = paras.get_mut(para_idx as usize)
                .ok_or_else(|| format!("err:para_idx={} 越界(该形状共 {} 段)", para_idx, np))?;
            let nr = para.runs.len();
            let run = para.runs.get_mut(run_idx as usize)
                .ok_or_else(|| format!("err:run_idx={} 越界(该段共 {} 个 run;空占位符请先 set_shape_text 写入文字)", run_idx, nr))?;
            run.bold      = Some(bold);
            run.italic    = Some(italic);
            run.underline = Some(underline);
            if font_size > 0.0 {
                run.font_size = Some(font_size as f32);
            }
            Ok(())
        }))
    });


    // set_shape_paragraph_format(slide, shape, para_idx, align, indent_left, indent_first, spacing_before, spacing_after)
        engine.register_fn("set_shape_paragraph_format",
        |slide: i64, shape: i64, para_idx: i64, align: String,
         indent_left: f64, indent_first: f64,
         spacing_before: f64, spacing_after: f64| -> String {
        write_ret(with_pres(|p| -> Result<(), String> {
            let n = p.slides.len();
            let s = p.slides.get_mut(slide as usize)
                .ok_or_else(|| format!("err:slide={} 越界(共 {} 页)", slide, n))?;
            let m = s.shapes.len();
            let sh = s.shapes.get_mut(shape as usize)
                .ok_or_else(|| format!("err:shape 下标={} 越界(该页共 {} 个形状;shape 是页内下标,不是 shape_id)", shape, m))?;
            let ShapeContent::Text(paras) = &mut sh.content else {
                return Err("err:该形状不是文本形状".to_string());
            };
            let np = paras.len();
            let para = paras.get_mut(para_idx as usize)
                .ok_or_else(|| format!("err:para_idx={} 越界(该形状共 {} 段)", para_idx, np))?;
            // 先校验对齐值再改,非法值不再清掉原对齐
            let new_align = if align.is_empty() {
                None
            } else {
                Some(match align.to_lowercase().as_str() {
                    "left"    => Alignment::Left,
                    "center"  => Alignment::Center,
                    "right"   => Alignment::Right,
                    "justify" => Alignment::Justify,
                    _ => return Err(format!("err:align 非法值 \"{}\"(可选 left/center/right/justify;空串表示不改)", align)),
                })
            };
            if let Some(a) = new_align { para.alignment = Some(a); }
            if indent_left    >= 0.0 { para.indent_left  = Some(indent_left as f32); }
            if indent_first   >= 0.0 { para.indent_first = Some(indent_first as f32); }
            if spacing_before >= 0.0 { para.space_before = Some(spacing_before as f32); }
            if spacing_after  >= 0.0 { para.space_after  = Some(spacing_after as f32); }
            Ok(())
        }))
    });




    // set_slide_background(slide_idx, background_json) -> String
    // 空字符串表示成功，非空字符串为错误信息。
    // 设计要点（本次会话与用户逐项讨论确认）：
    // - 支持 solid/gradient/none 三种（本次完整实现）；picture/picture_tiled
    //   两种走占位错误分支，等 Agent 工作区机制（图片路径读取+安全隔离）
    //   补齐后再实现真正的文件读取逻辑，不是本次遗漏。
    // - slide_idx 越界 / JSON 解析失败 / type 字段非法均返回非空错误信息，
    //   不修改 background 字段（全有或全无，与 set_range_json 同一哲学）。
    engine.register_fn("set_slide_background", |slide_idx: i64, background_json: String| -> String {
        match parse_background_json(&background_json) {
            Ok(fill_style) => {
                let result = with_pres(|p| {
                    match p.slides.get_mut(slide_idx as usize) {
                        Some(slide) => {
                            slide.background = if matches!(fill_style, FillStyle::None) {
                                None
                            } else {
                                Some(fill_style)
                            };
                            String::new()
                        }
                        None => format!("set_slide_background: slide_idx {slide_idx} 越界"),
                    }
                });
                result.unwrap_or_else(|| "set_slide_background: 未绑定 Presentation（内部错误）".to_string())
            }
            Err(e) => format!("set_slide_background: {e}"),
        }
    });
}

/// 解析 set_slide_background 的输入 JSON，产出 FillStyle。
/// 独立成普通函数（而非内联在 register_fn 闭包里）是因为闭包返回类型是
/// String（Rhai 要求），无法在闭包体内直接用 `?` 操作符做 Result 传播；
/// 抽成这个函数后内部可以自由用 `?`，调用处再统一转换成 String 错误信息。
fn parse_background_json(background_json: &str) -> Result<FillStyle, String> {
    let parsed: serde_json::Value = serde_json::from_str(background_json)
        .map_err(|e| format!("JSON 解析失败: {e}"))?;

    let bg_type = parsed.get("type").and_then(|v| v.as_str())
        .ok_or("缺少 type 字段")?;

    match bg_type {
        "none" => Ok(FillStyle::None),
        "solid" => {
            let color = parsed.get("color").ok_or("solid 类型缺少 color 字段")?;
            let r = color.get("r").and_then(|v| v.as_u64()).ok_or("color.r 缺失或非法")? as u8;
            let g = color.get("g").and_then(|v| v.as_u64()).ok_or("color.g 缺失或非法")? as u8;
            let b = color.get("b").and_then(|v| v.as_u64()).ok_or("color.b 缺失或非法")? as u8;
            let a = color.get("a").and_then(|v| v.as_u64()).unwrap_or(255) as u8;
            Ok(FillStyle::Solid(Color { r, g, b, a }))
        }
        "gradient" => {
            let stops_json = parsed.get("stops").and_then(|v| v.as_array())
                .ok_or("gradient 类型缺少 stops 数组")?;
            if stops_json.is_empty() {
                return Err("gradient stops 不能为空".to_string());
            }
            let mut stops = Vec::with_capacity(stops_json.len());
            for stop in stops_json {
                let pos = stop.get("pos").and_then(|v| v.as_f64()).ok_or("stop.pos 缺失或非法")? as f32;
                let color = stop.get("color").ok_or("stop.color 缺失")?;
                let r = color.get("r").and_then(|v| v.as_u64()).ok_or("stop.color.r 缺失或非法")? as u8;
                let g = color.get("g").and_then(|v| v.as_u64()).ok_or("stop.color.g 缺失或非法")? as u8;
                let b = color.get("b").and_then(|v| v.as_u64()).ok_or("stop.color.b 缺失或非法")? as u8;
                let a = color.get("a").and_then(|v| v.as_u64()).unwrap_or(255) as u8;
                stops.push(GradientStop { pos, color: Color { r, g, b, a } });
            }
            let kind_str = parsed.get("kind").and_then(|v| v.as_str()).unwrap_or("linear");
            let kind = match kind_str {
                "linear" => {
                    let angle = parsed.get("angle").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    GradientKind::Linear { angle }
                }
                "radial" => GradientKind::Radial,
                "rect" => GradientKind::Rect,
                other => return Err(format!("未知 gradient kind: {other}")),
            };
            let rotate_with_shape = parsed.get("rotate_with_shape").and_then(|v| v.as_bool()).unwrap_or(true);
            Ok(FillStyle::Gradient { stops, kind, rotate_with_shape })
        }
        "picture" => {
            let virtual_path = parsed.get("path").and_then(|v| v.as_str())
                .ok_or("picture 类型缺少 path 字段")?;
            let mime = parsed.get("mime").and_then(|v| v.as_str())
                .ok_or("picture 类型缺少 mime 字段（建议直接传 ingest_media_asset 返回的 mime_type）")?
                .to_string();
            let data = crate::media_context::with_media(|source| source.read_media(virtual_path))
                .ok_or("picture 类型需要 Agent 工作区支持，但当前会话未提供媒体资产来源")?
                .map_err(|e| format!("读取媒体文件失败: {e}"))?;
            Ok(FillStyle::Picture(ImageData {
                data,
                mime,
                crop_l: 0.0,
                crop_t: 0.0,
                crop_r: 0.0,
                crop_b: 0.0,
            }))
        }
        "picture_tiled" => {
            let virtual_path = parsed.get("path").and_then(|v| v.as_str())
                .ok_or("picture_tiled 类型缺少 path 字段")?;
            let mime = parsed.get("mime").and_then(|v| v.as_str())
                .ok_or("picture_tiled 类型缺少 mime 字段（建议直接传 ingest_media_asset 返回的 mime_type）")?
                .to_string();
            let data = crate::media_context::with_media(|source| source.read_media(virtual_path))
                .ok_or("picture_tiled 类型需要 Agent 工作区支持，但当前会话未提供媒体资产来源")?
                .map_err(|e| format!("读取媒体文件失败: {e}"))?;
            let tile_offset_x = parsed.get("tile_offset_x").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let tile_offset_y = parsed.get("tile_offset_y").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let tile_scale_x = parsed.get("tile_scale_x").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            let tile_scale_y = parsed.get("tile_scale_y").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
            let tile_align = parsed.get("tile_align").and_then(|v| v.as_str()).unwrap_or("tl").to_string();
            Ok(FillStyle::PictureTiled {
                image: ImageData {
                    data,
                    mime,
                    crop_l: 0.0,
                    crop_t: 0.0,
                    crop_r: 0.0,
                    crop_b: 0.0,
                },
                tile_offset_x,
                tile_offset_y,
                tile_scale_x,
                tile_scale_y,
                tile_align,
            })
        }
        other => Err(format!("未知背景类型: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::impress::*;
    use roe_model::common::Transform;

    pub(super) fn make_pres() -> Presentation {
        let para = Paragraph {
            style: StyleRef("Default".into()),
            runs: vec![Run { text: "Hello Slide".into(), char_style: None, ..Default::default() }],
            list_info: None,
            ..Default::default()
        };
        Presentation {
            styles: roe_model::style::StyleRegistry::default(),
            theme_colors: std::collections::HashMap::new(),
            default_text_style: Default::default(),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![Shape {
                    id: 1,
                    geometry: ShapeGeometry::Rect,
                    position: Transform::from_rect(0.0, 0.0, 100.0, 50.0),
                    style: ShapeStyle::default(),
                    content: ShapeContent::Text(vec![para]),
                    text_body: TextBodyProps {
                        autofit: TextAutofit::NoAutofit,
                        wrap: true,
                        anchor: TextAnchor::Top,
                        ins_l: None, ins_r: None, ins_t: None, ins_b: None,
                        vert: TextVert::default(),
                    },
            placeholder: None,
            end_cxn: None,
            st_cxn: None,
        }],
                master_idx: None,
                animation_tree: AnimationNode::TimingRoot { children: vec![] },
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            slide_size: (960.0, 540.0),
        }
    }

    #[test]
    fn test_get_shape_text() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let text: String = e.eval("get_shape_text(0, 0)").unwrap();
            assert_eq!(text, "Hello Slide");
        });
    }

    fn eval_str(pres: &mut Presentation, script: &str) -> String {
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut out = String::new();
        with_presentation(pres, || {
            out = e.eval::<String>(script).unwrap();
        });
        out
    }

    #[test]
    fn write_fns_return_ok_on_success() {
        let mut pres = make_pres();
        assert_eq!(eval_str(&mut pres, r#"set_shape_text(0, 0, "A")"#), "ok");
        assert_eq!(eval_str(&mut pres, "set_shape_char_format(0, 0, 0, 0, true, false, false, 12.0)"), "ok");
        assert_eq!(eval_str(&mut pres, r#"set_shape_paragraph_format(0, 0, 0, "center", -1.0, -1.0, -1.0, -1.0)"#), "ok");
    }

    #[test]
    fn write_fns_report_out_of_range() {
        let mut pres = make_pres();
        let r = eval_str(&mut pres, r#"set_shape_text(99, 0, "x")"#);
        assert!(r.starts_with("err:") && r.contains("slide=99"), "{r}");
        let r = eval_str(&mut pres, r#"set_shape_text(0, 99, "x")"#);
        assert!(r.starts_with("err:") && r.contains("shape"), "{r}");
        let r = eval_str(&mut pres, "set_shape_char_format(0, 0, 0, 99, true, false, false, 12.0)");
        assert!(r.starts_with("err:") && r.contains("run_idx=99"), "{r}");
        let r = eval_str(&mut pres, "set_shape_char_format(0, 0, 99, 0, true, false, false, 12.0)");
        assert!(r.starts_with("err:") && r.contains("para_idx=99"), "{r}");
    }

    #[test]
    fn paragraph_format_bad_align_errs_and_keeps_old() {
        let mut pres = make_pres();
        assert_eq!(eval_str(&mut pres, r#"set_shape_paragraph_format(0, 0, 0, "right", -1.0, -1.0, -1.0, -1.0)"#), "ok");
        let r = eval_str(&mut pres, r#"set_shape_paragraph_format(0, 0, 0, "centre", -1.0, -1.0, -1.0, -1.0)"#);
        assert!(r.starts_with("err:") && r.contains("align"), "{r}");
        match &pres.slides[0].shapes[0].content {
            ShapeContent::Text(paras) => assert!(matches!(paras[0].alignment, Some(Alignment::Right)), "{:?}", paras[0].alignment),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn test_set_shape_text() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            e.run("set_shape_text(0, 0, \"Updated\")").unwrap();
        });
        match &pres.slides[0].shapes[0].content {
            ShapeContent::Text(paras) => assert_eq!(paras[0].runs[0].text, "Updated"),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn test_slide_count() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let n: i64 = e.eval("slide_count()").unwrap();
            assert_eq!(n, 1);
        });
    }

    #[test]
    fn test_insert_slide_appends_and_shifts() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            // 插入到末尾（index=1，原本只有1张slide，长度为1）
            e.run("insert_slide(1, -1)").unwrap();
        });
        assert_eq!(pres.slides.len(), 2);
        assert!(pres.slides[1].shapes.is_empty());
        assert_eq!(pres.slides[1].master_idx, None);
    }

    #[test]
    fn test_insert_slide_with_master_idx() {
        let mut pres = make_pres();
        pres.master_slides = vec![make_master("A"), make_master("B"), make_master("C"), make_master("D")];
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            e.run("insert_slide(0, 3)").unwrap();
        });
        assert_eq!(pres.slides.len(), 2);
        assert_eq!(pres.slides[0].master_idx, Some(3));
        // 原本的第0张被挤到第1位，文字内容应还在
        match &pres.slides[1].shapes[0].content {
            ShapeContent::Text(paras) => assert_eq!(paras[0].runs[0].text, "Hello Slide"),
            _ => panic!("expected Text content"),
        }
    }

    fn make_master(name: &str) -> MasterSlide {
        MasterSlide { theme_xml: None, tx_styles_xml: None, layouts: Vec::new(), color_map: None, raw_ext: None, name: name.to_string(), shapes: vec![], background: None }
    }

    #[test]
    fn test_list_masters_empty() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let json: String = e.eval("list_masters()").unwrap();
            assert_eq!(json, "[]");
        });
    }

    #[test]
    fn test_list_masters_returns_index_name_shape_count() {
        let mut pres = make_pres();
        pres.master_slides = vec![make_master("Title Slide"), make_master("Content")];
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let json: String = e.eval("list_masters()").unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.as_array().unwrap().len(), 2);
            assert_eq!(parsed[0]["index"], 0);
            assert_eq!(parsed[0]["name"], "Title Slide");
            assert_eq!(parsed[0]["shape_count"], 0);
            assert_eq!(parsed[1]["index"], 1);
            assert_eq!(parsed[1]["name"], "Content");
        });
    }

    #[test]
    fn test_set_slide_master_switches_to_valid_index() {
        let mut pres = make_pres();
        pres.master_slides = vec![make_master("A"), make_master("B")];
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let r: String = e.eval("set_slide_master(0, 1)").unwrap();
            assert_eq!(r, "ok");
        });
        assert_eq!(pres.slides[0].master_idx, Some(1));
    }

    #[test]
    fn test_set_slide_master_negative_unmounts() {
        let mut pres = make_pres();
        pres.master_slides = vec![make_master("A")];
        pres.slides[0].master_idx = Some(0);
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let r: String = e.eval("set_slide_master(0, -1)").unwrap();
            assert_eq!(r, "ok");
        });
        assert_eq!(pres.slides[0].master_idx, None);
    }

    #[test]
    fn test_set_slide_master_out_of_bounds_slide_index_returns_false() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let r: String = e.eval("set_slide_master(99, 0)").unwrap();
            assert!(r.starts_with("err:"), "{r}");
        });
    }

    #[test]
    fn test_set_slide_master_validates_master_idx_range() {
        // v303 起校验 master_idx:越界报错且不改动原挂载。
        let mut pres = make_pres();
        let before = pres.slides[0].master_idx;
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let r: String = e.eval("set_slide_master(0, 999)").unwrap();
            assert!(r.starts_with("err:"), "{r}");
        });
        assert_eq!(pres.slides[0].master_idx, before);
    }

    #[test]
    fn test_get_last_error_records_creation_failure_and_resets() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            assert_eq!(e.eval::<String>("get_last_error()").unwrap(), "");
            let id: i64 = e.eval(r#"insert_shape(99, "rect", 0.0, 0.0, 10.0, 10.0)"#).unwrap();
            assert_eq!(id, -1);
            let m: String = e.eval("get_last_error()").unwrap();
            assert!(m.starts_with("err:") && m.contains("99"), "{m}");
            let id2: i64 = e.eval(r#"insert_connector(-1, "straight", 0.0, 0.0, 10.0, 10.0, -1, 0, -1, 0)"#).unwrap();
            assert_eq!(id2, -1);
        });
        // 新一次绑定(新一次脚本)应清空
        with_presentation(&mut pres, || {
            assert_eq!(e.eval::<String>("get_last_error()").unwrap(), "");
        });
    }

    #[test]
    fn test_insert_slide_index_out_of_bounds_clamped() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            // index 远超长度，应 clamp 到末尾，不 panic
            e.run("insert_slide(999, -1)").unwrap();
        });
        assert_eq!(pres.slides.len(), 2);
    }

    #[test]
    fn test_insert_slide_validates_master_idx() {
        let mut pres = make_pres();
        pres.master_slides = vec![make_master("A")];
        let n = pres.slides.len();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            let r: String = e.eval(r#"insert_slide(0, 5).to_string() + "|" + get_last_error()"#).unwrap();
            assert!(r.starts_with("-1|err:master_idx=5"), "{r}");
            let r: i64 = e.eval("insert_slide(0, 0)").unwrap();
            assert_eq!(r, 0);
            let r: i64 = e.eval("insert_slide(999, -1)").unwrap();
            assert_eq!(r, 2, "追加时返回收敛后的下标");
        });
        assert_eq!(pres.slides.len(), n + 2, "err 不得插入页");
    }

    #[test]
    fn test_delete_slide_success() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut r = String::new();
        with_presentation(&mut pres, || {
            r = e.eval("delete_slide(0)").unwrap();
        });
        assert_eq!(r, "ok");
        assert_eq!(pres.slides.len(), 0);
    }

    #[test]
    fn test_delete_slide_out_of_bounds_returns_false() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut r = String::new();
        with_presentation(&mut pres, || {
            r = e.eval("delete_slide(5)").unwrap();
        });
        assert!(r.starts_with("err:"), "{r}");
        assert_eq!(pres.slides.len(), 1);
    }

    #[test]
    fn test_duplicate_slide_success() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut r = String::new();
        with_presentation(&mut pres, || {
            r = e.eval("duplicate_slide(0)").unwrap();
        });
        assert_eq!(r, "ok");
        assert_eq!(pres.slides.len(), 2);
        // 复制出的 slide 内容应与原 slide 一致（文字、shape id 均相同）
        match (&pres.slides[0].shapes[0].content, &pres.slides[1].shapes[0].content) {
            (ShapeContent::Text(p1), ShapeContent::Text(p2)) => {
                assert_eq!(p1[0].runs[0].text, p2[0].runs[0].text);
            }
            _ => panic!("expected Text content on both"),
        }
        assert_eq!(pres.slides[0].shapes[0].id, pres.slides[1].shapes[0].id);
    }

    #[test]
    fn test_duplicate_slide_out_of_bounds_returns_false() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut r = String::new();
        with_presentation(&mut pres, || {
            r = e.eval("duplicate_slide(9)").unwrap();
        });
        assert!(r.starts_with("err:"), "{r}");
        assert_eq!(pres.slides.len(), 1);
    }

    #[test]
    fn test_insert_shape_rect_returns_new_id() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        // make_pres() 里已有一个 id=1 的 shape，新插入应分配 id=2
        let mut new_id: i64 = -1;
        with_presentation(&mut pres, || {
            new_id = e.eval(r#"insert_shape(0, "rect", 10.0, 20.0, 100.0, 50.0)"#).unwrap();
        });
        assert_eq!(new_id, 2);
        assert_eq!(pres.slides[0].shapes.len(), 2);
        let shape = &pres.slides[0].shapes[1];
        assert_eq!(shape.id, 2);
        assert!(matches!(shape.geometry, ShapeGeometry::Rect));
        assert_eq!(shape.position.offset, (10.0, 20.0));
        assert_eq!(shape.position.extent, (100.0, 50.0));
    }

    #[test]
    fn test_insert_shape_ellipse_and_line() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            e.run(r#"insert_shape(0, "ellipse", 0.0, 0.0, 10.0, 10.0)"#).unwrap();
            e.run(r#"insert_shape(0, "line", 0.0, 0.0, 10.0, 10.0)"#).unwrap();
        });
        assert!(matches!(pres.slides[0].shapes[1].geometry, ShapeGeometry::Ellipse));
        assert!(matches!(pres.slides[0].shapes[2].geometry, ShapeGeometry::Line));
    }

    #[test]
    fn test_insert_shape_unknown_type_falls_back_to_rect() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            e.run(r#"insert_shape(0, "star", 0.0, 0.0, 10.0, 10.0)"#).unwrap();
        });
        assert!(matches!(pres.slides[0].shapes[1].geometry, ShapeGeometry::Rect));
    }

    #[test]
    fn test_insert_shape_preset_name_produces_preset_path() {
        // A2: 验证187种预设形状表中的任意名字（非rect/ellipse/line原生
        // 变体）能被 insert_shape 正确路由到 find_preset_shape 查表 +
        // eval_preset_shape 求值，产出 PresetPath，而不是像未知字符串
        // 那样 fallback 到 Rect。star5（五角星）是表里确认存在的形状名
        // （roe-model/src/preset_geometry.rs），与pptx.rs解析已有文件时
        // 复用同一张表、同一套求值逻辑。
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut new_id: i64 = -1;
        with_presentation(&mut pres, || {
            new_id = e.eval(r#"insert_shape(0, "star5", 0.0, 0.0, 100.0, 100.0)"#).unwrap();
        });
        assert!(new_id > 0);
        let shape = pres.slides[0]
            .shapes
            .iter()
            .find(|s| s.id == new_id as u32)
            .unwrap();
        match &shape.geometry {
            ShapeGeometry::PresetPath(cmds) => {
                assert!(!cmds.is_empty(), "star5求值后的路径指令不应为空");
            }
            other => panic!("expected PresetPath for star5, got {:?}", other),
        }
    }

    #[test]
    fn test_insert_shape_slide_out_of_bounds_returns_negative_one() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut new_id: i64 = 0;
        with_presentation(&mut pres, || {
            new_id = e.eval(r#"insert_shape(9, "rect", 0.0, 0.0, 10.0, 10.0)"#).unwrap();
        });
        assert_eq!(new_id, -1);
    }

    #[test]
    fn test_delete_shape_success() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut r = String::new();
        with_presentation(&mut pres, || {
            // make_pres() 里的 shape id 是 1
            r = e.eval("delete_shape(0, 1)").unwrap();
        });
        assert_eq!(r, "ok");
        assert!(pres.slides[0].shapes.is_empty());
    }

    #[test]
    fn test_delete_shape_nonexistent_id_returns_false() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut r = String::new();
        with_presentation(&mut pres, || {
            r = e.eval("delete_shape(0, 999)").unwrap();
        });
        assert!(r.starts_with("err:"), "{r}");
        assert_eq!(pres.slides[0].shapes.len(), 1);
    }

    #[test]
    fn test_delete_shape_slide_out_of_bounds_returns_false() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut r = String::new();
        with_presentation(&mut pres, || {
            r = e.eval("delete_shape(9, 1)").unwrap();
        });
        assert!(r.starts_with("err:"), "{r}");
    }

    #[test]
    fn test_set_get_shape_char_format() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            e.run("set_shape_char_format(0, 0, 0, 0, true, true, false, 18.0)").unwrap();
        });
        let run = &pres.slides[0].shapes[0].content;
        match run {
            ShapeContent::Text(paras) => {
                let r = &paras[0].runs[0];
                assert_eq!(r.bold, Some(true));
                assert_eq!(r.italic, Some(true));
                assert_eq!(r.underline, Some(false));
                assert_eq!(r.font_size, Some(18.0));
            }
            _ => panic!("expected Text content"),
        }

        let mut json = String::new();
        with_presentation(&mut pres, || {
            json = e.eval("get_shape_char_format(0, 0, 0, 0)").unwrap();
        });
        assert!(json.contains("\"bold\":true"));
        assert!(json.contains("\"font_size\":18.0"));
    }

    #[test]
    fn test_set_get_shape_char_format_out_of_bounds_returns_empty_json() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut json = String::new();
        with_presentation(&mut pres, || {
            json = e.eval("get_shape_char_format(0, 0, 9, 9)").unwrap();
        });
        assert_eq!(json, "{}");
    }

    #[test]
    fn test_set_get_shape_paragraph_format() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            e.run(r#"set_shape_paragraph_format(0, 0, 0, "center", 10.0, 5.0, 12.0, 6.0)"#).unwrap();
        });
        match &pres.slides[0].shapes[0].content {
            ShapeContent::Text(paras) => {
                let para = &paras[0];
                assert!(matches!(para.alignment, Some(Alignment::Center)));
                assert_eq!(para.indent_left, Some(10.0));
                assert_eq!(para.indent_first, Some(5.0));
                assert_eq!(para.space_before, Some(12.0));
                assert_eq!(para.space_after, Some(6.0));
            }
            _ => panic!("expected Text content"),
        }

        let mut json = String::new();
        with_presentation(&mut pres, || {
            json = e.eval("get_shape_paragraph_format(0, 0, 0)").unwrap();
        });
        assert!(json.contains("\"alignment\":\"center\""));
    }

    #[test]
    fn test_shape_paragraph_format_empty_align_leaves_unchanged() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            // align 传空字符串，alignment 字段不应被改动（仍是 None，初始值）
            e.run(r#"set_shape_paragraph_format(0, 0, 0, "", 1.0, 1.0, 1.0, 1.0)"#).unwrap();
        });
        match &pres.slides[0].shapes[0].content {
            ShapeContent::Text(paras) => assert!(paras[0].alignment.is_none()),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn test_insert_connector_straight() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut new_id: i64 = -1;
        with_presentation(&mut pres, || {
            new_id = e.eval(r#"insert_connector(0, "straight", 0.0, 0.0, 100.0, 50.0, -1, 0, -1, 0)"#).unwrap();
        });
        assert_eq!(new_id, 2);
        let shape = &pres.slides[0].shapes[1];
        assert!(matches!(shape.geometry, ShapeGeometry::PresetPath(_)));
        assert_eq!(shape.st_cxn, None);
        assert_eq!(shape.end_cxn, None);
    }

    #[test]
    fn test_insert_connector_bent_and_curved() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            e.run(r#"insert_connector(0, "bent", 0.0, 0.0, 100.0, 50.0, -1, 0, -1, 0)"#).unwrap();
            e.run(r#"insert_connector(0, "curved", 0.0, 0.0, 100.0, 50.0, -1, 0, -1, 0)"#).unwrap();
        });
        assert!(matches!(pres.slides[0].shapes[1].geometry, ShapeGeometry::PresetPath(_)));
        assert!(matches!(pres.slides[0].shapes[2].geometry, ShapeGeometry::PresetPath(_)));
    }

    #[test]
    fn test_insert_connector_unknown_type_falls_back_to_straight() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        with_presentation(&mut pres, || {
            e.run(r#"insert_connector(0, "zigzag", 0.0, 0.0, 100.0, 50.0, -1, 0, -1, 0)"#).unwrap();
        });
        // fallback到straight同样产出PresetPath（straightConnector1也在187表里），
        // 用path非空 + 不panic来验证fallback生效，而非退化到Rect（这是connector
        // 专属的fallback目标，与insert_shape的Rect fallback不同，因为straight
        // 本身就是查表结果，没有"原生不查表"分支）
        assert!(matches!(pres.slides[0].shapes[1].geometry, ShapeGeometry::PresetPath(_)));
    }

    #[test]
    fn test_insert_connector_invalid_slide_returns_negative_one() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut new_id: i64 = -1;
        with_presentation(&mut pres, || {
            new_id = e.eval(r#"insert_connector(9, "straight", 0.0, 0.0, 100.0, 50.0, -1, 0, -1, 0)"#).unwrap();
        });
        assert_eq!(new_id, -1);
    }

    #[test]
    fn test_insert_connector_binds_endpoints() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut new_id: i64 = -1;
        with_presentation(&mut pres, || {
            // 绑定到shape id=1（make_pres()里已有的shape）的连接点idx=2，
            // 终点绑定到shape id=1的idx=4（自我连接，纯粹测试数值写入正确，
            // 不代表真实场景connector会连自己）
            new_id = e.eval(r#"insert_connector(0, "straight", 0.0, 0.0, 100.0, 50.0, 1, 2, 1, 4)"#).unwrap();
        });
        let shape = pres.slides[0].shapes.iter().find(|s| s.id == new_id as u32).unwrap();
        assert_eq!(shape.st_cxn, Some((1, 2)));
        assert_eq!(shape.end_cxn, Some((1, 4)));
    }

    #[test]
    fn test_set_connector_arrows_independent_of_geometry() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut new_id: i64 = -1;
        let mut r = String::new();
        with_presentation(&mut pres, || {
            new_id = e.eval(r#"insert_connector(0, "straight", 0.0, 0.0, 100.0, 50.0, -1, 0, -1, 0)"#).unwrap();
        });
        with_presentation(&mut pres, || {
            r = e.eval(&format!(r#"set_connector_arrows(0, {}, "arrow", "triangle")"#, new_id)).unwrap();
        });
        assert_eq!(r, "ok");
        let shape = pres.slides[0].shapes.iter().find(|s| s.id == new_id as u32).unwrap();
        assert_eq!(shape.style.head_arrow, Some(roe_model::common::ArrowType::Arrow));
        assert_eq!(shape.style.tail_arrow, Some(roe_model::common::ArrowType::Triangle));
        // geometry不受影响，仍是插入时产出的PresetPath
        assert!(matches!(shape.geometry, ShapeGeometry::PresetPath(_)));
    }

    #[test]
    fn test_set_connector_arrows_unknown_falls_back_to_none() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut new_id: i64 = -1;
        with_presentation(&mut pres, || {
            new_id = e.eval(r#"insert_connector(0, "straight", 0.0, 0.0, 100.0, 50.0, -1, 0, -1, 0)"#).unwrap();
        });
        with_presentation(&mut pres, || {
            e.run(&format!(r#"set_connector_arrows(0, {}, "unknownstyle", "oval")"#, new_id)).unwrap();
        });
        let shape = pres.slides[0].shapes.iter().find(|s| s.id == new_id as u32).unwrap();
        assert_eq!(shape.style.head_arrow, Some(roe_model::common::ArrowType::None));
        assert_eq!(shape.style.tail_arrow, Some(roe_model::common::ArrowType::Oval));
    }

    #[test]
    fn test_set_connector_arrows_invalid_shape_id_returns_false() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut r = String::new();
        with_presentation(&mut pres, || {
            r = e.eval(r#"set_connector_arrows(0, 999, "arrow", "arrow")"#).unwrap();
        });
        assert!(r.starts_with("err:"), "{r}");
    }


    #[test]
    fn test_save_pptx_produces_valid_bytes() {
        // 验证 save_pptx() 脚本接口：base64 解码后应能被 PptxParser 正常
        // 读回，slide 数量与 make_pres() 构造的原始 presentation 一致。
        let mut pres = make_pres();
        let expected_slide_count = pres.slides.len();
        with_presentation(&mut pres, || {
            let mut e = rhai::Engine::new();
            register(&mut e);
            let b64: String = e.eval("save_pptx()").unwrap();
            assert!(!b64.is_empty(), "save_pptx() 不应返回空字符串");

            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .expect("base64 解码失败");

            let parsed = roe_parser::ooxml::pptx::PptxParser::parse(&bytes)
                .expect("PptxParser 读回失败，说明 save_pptx 产出的字节流不是合法 pptx");

            assert_eq!(parsed.slides.len(), expected_slide_count);
        });
    }

    #[test]
    fn test_get_slide_summary_basic() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut json_str = String::new();
        with_presentation(&mut pres, || {
            json_str = e.eval(r#"get_slide_summary(0)"#).unwrap();
        });
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["slide_idx"], 0);
        assert_eq!(v["shape_count"], 1);
        assert_eq!(v["shapes"][0]["id"], 1);
        assert_eq!(v["shapes"][0]["geometry"], "rect");
        assert_eq!(v["shapes"][0]["content"]["type"], "text");
        assert_eq!(v["shapes"][0]["content"]["text"], "Hello Slide");
        assert_eq!(v["shapes"][0]["style"]["has_fill"], false);
    }

    #[test]
    fn test_get_slide_summary_out_of_range_returns_null() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut json_str = String::new();
        with_presentation(&mut pres, || {
            json_str = e.eval(r#"get_slide_summary(999)"#).unwrap();
        });
        assert_eq!(json_str, "null");
    }

    #[test]
    fn test_set_slide_background_solid() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut scope = rhai::Scope::new();
        scope.push("bg_json", r#"{"type":"solid","color":{"r":255,"g":0,"b":0,"a":255}}"#.to_string());
        let mut result = String::new();
        with_presentation(&mut pres, || {
            result = e.eval_with_scope::<String>(&mut scope, "set_slide_background(0, bg_json)").unwrap();
        });
        assert_eq!(result, "", "设置成功应返回空字符串，实际: {result}");
        match &pres.slides[0].background {
            Some(FillStyle::Solid(c)) => {
                assert_eq!(c.r, 255);
                assert_eq!(c.g, 0);
                assert_eq!(c.b, 0);
            }
            other => panic!("期望 Solid 背景，实际: {other:?}"),
        }
    }

    #[test]
    fn test_set_slide_background_gradient() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut scope = rhai::Scope::new();
        scope.push("bg_json", r#"{"type":"gradient","stops":[{"pos":0.0,"color":{"r":255,"g":0,"b":0,"a":255}},{"pos":1.0,"color":{"r":0,"g":0,"b":255,"a":255}}],"kind":"linear","angle":45.0}"#.to_string());
        let mut result = String::new();
        with_presentation(&mut pres, || {
            result = e.eval_with_scope::<String>(&mut scope, "set_slide_background(0, bg_json)").unwrap();
        });
        assert_eq!(result, "", "设置成功应返回空字符串，实际: {result}");
        match &pres.slides[0].background {
            Some(FillStyle::Gradient { stops, kind, .. }) => {
                assert_eq!(stops.len(), 2);
                assert!(matches!(kind, GradientKind::Linear { angle } if (*angle - 45.0).abs() < 0.01));
            }
            other => panic!("期望 Gradient 背景，实际: {other:?}"),
        }
    }

    #[test]
    fn test_set_slide_background_none_clears_background() {
        let mut pres = make_pres();
        pres.slides[0].background = Some(FillStyle::Solid(Color { r: 1, g: 2, b: 3, a: 255 }));
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut scope = rhai::Scope::new();
        scope.push("bg_json", r#"{"type":"none"}"#.to_string());
        let mut result = String::new();
        with_presentation(&mut pres, || {
            result = e.eval_with_scope::<String>(&mut scope, "set_slide_background(0, bg_json)").unwrap();
        });
        assert_eq!(result, "");
        assert!(pres.slides[0].background.is_none());
    }

    /// 测试用的假 MediaAssetSource：只认一个固定虚拟路径，其余一律
    /// 返回 Err，模拟"文件不存在"这类真实场景。
    struct FakeMediaSource;
    impl crate::media_context::MediaAssetSource for FakeMediaSource {
        fn read_media(&self, virtual_path: &str) -> Result<Vec<u8>, String> {
            if virtual_path == "/media/deadbeef.png" {
                Ok(b"fake png bytes".to_vec())
            } else {
                Err(format!("no such file in fake workspace: {virtual_path}"))
            }
        }
    }

    #[test]
    fn test_set_slide_background_picture_without_media_source_returns_clear_error() {
        // 没有通过 with_media_source 绑定任何 MediaAssetSource 时
        // （比如只读查询路径、或调用方本来就没有 workspace 可用），
        // picture 分支应该返回明确的"未提供媒体资产来源"错误，而不是
        // panic 或静默成功。
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut scope = rhai::Scope::new();
        scope.push("bg_json", r#"{"type":"picture","path":"/media/deadbeef.png","mime":"image/png"}"#.to_string());
        let mut result = String::new();
        with_presentation(&mut pres, || {
            result = e.eval_with_scope::<String>(&mut scope, "set_slide_background(0, bg_json)").unwrap();
        });
        assert!(!result.is_empty(), "未绑定媒体源时应返回错误，不应静默成功");
        assert!(result.contains("未提供媒体资产来源"), "错误信息应说明缺少 workspace 支持，实际: {result}");
        assert!(pres.slides[0].background.is_none(), "失败时不应修改 background 字段");
    }

    #[test]
    fn test_set_slide_background_picture_succeeds_with_media_source() {
        // 端到端验证（roe-script 这一层能做到的最贴近版本，见
        // ROE_Progress_v199.md 四、4.2 第2点提到的验证要求）：
        // 绑定一个假的 MediaAssetSource → set_slide_background 走
        // picture 分支 → background 字段确实变成预期的 FillStyle::Picture，
        // 且字节内容、mime 都与 FakeMediaSource 返回的一致。
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut scope = rhai::Scope::new();
        scope.push("bg_json", r#"{"type":"picture","path":"/media/deadbeef.png","mime":"image/png"}"#.to_string());
        let mut result = String::new();
        let source = FakeMediaSource;
        crate::media_context::with_media_source(Some(&source), || {
            with_presentation(&mut pres, || {
                result = e.eval_with_scope::<String>(&mut scope, "set_slide_background(0, bg_json)").unwrap();
            });
        });
        assert_eq!(result, "", "picture 类型应成功设置背景，不应返回错误: {result}");
        match &pres.slides[0].background {
            Some(FillStyle::Picture(image)) => {
                assert_eq!(image.data, b"fake png bytes");
                assert_eq!(image.mime, "image/png");
                assert_eq!(image.crop_l, 0.0);
            }
            other => panic!("期望 Some(FillStyle::Picture(..))，实际: {other:?}"),
        }
    }

    #[test]
    fn test_set_slide_background_picture_missing_mime_returns_error() {
        // mime 字段缺失应该明确报错，而不是猜测或留空——本次 MVP 不
        // 引入魔术字节探测（见 ROE_Progress_v199.md 二、2.3），调用方
        // 必须显式提供 mime（自然来源：ingest_media_asset 的返回值）。
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut scope = rhai::Scope::new();
        scope.push("bg_json", r#"{"type":"picture","path":"/media/deadbeef.png"}"#.to_string());
        let mut result = String::new();
        let source = FakeMediaSource;
        crate::media_context::with_media_source(Some(&source), || {
            with_presentation(&mut pres, || {
                result = e.eval_with_scope::<String>(&mut scope, "set_slide_background(0, bg_json)").unwrap();
            });
        });
        assert!(!result.is_empty(), "缺少 mime 字段应返回错误");
        assert!(result.contains("mime"), "错误信息应提到 mime 字段，实际: {result}");
        assert!(pres.slides[0].background.is_none());
    }

    #[test]
    fn test_set_slide_background_invalid_json_returns_error() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut scope = rhai::Scope::new();
        scope.push("bg_json", "not valid json".to_string());
        let mut result = String::new();
        with_presentation(&mut pres, || {
            result = e.eval_with_scope::<String>(&mut scope, "set_slide_background(0, bg_json)").unwrap();
        });
        assert!(!result.is_empty());
        assert!(pres.slides[0].background.is_none());
    }

    #[test]
    fn test_set_slide_background_out_of_range_slide() {
        let mut pres = make_pres();
        let mut e = rhai::Engine::new();
        register(&mut e);
        let mut scope = rhai::Scope::new();
        scope.push("bg_json", r#"{"type":"none"}"#.to_string());
        let mut result = String::new();
        with_presentation(&mut pres, || {
            result = e.eval_with_scope::<String>(&mut scope, "set_slide_background(999, bg_json)").unwrap();
        });
        assert!(!result.is_empty());
        assert!(result.contains("越界"));
    }
}

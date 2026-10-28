//! Impress 形状几何查询/修改 C-ABI 接口（roe-interface-implementation-plan.md
//! 阶段A 第3步：position/size 读写）。
//!
//! 定位方式统一为 (slide_idx, shape_id)，不使用数组下标，理由与
//! chart_query.rs 一致：避免删除/并发编辑导致下标错位。注意 Rhai 侧
//! （roe-script/src/impress_api.rs）现有的 get_shape_text/set_shape_text/
//! insert_shape 实际是按*下标*访问 shapes（shape_id 只在 insert_shape 的
//! 返回值和 delete_shape 的参数里用到），C-ABI 侧从设计第一天就统一按
//! shape_id 定位，不跟随 Rhai 侧这个下标用法，避免把同样的坑带到新代码里。
//!
//! 与 chart_query.rs 是姊妹接口（同一套 with_xxx/with_xxx_mut 辅助函数
//! 模式），但服务对象是 Slide.shapes 而非 Sheet.floating_charts，因此
//! 使用独立的 roe_impress_* 前缀，符合既有范式：每种文档类型各自一套
//! 同名前缀函数，不共享一个函数内部分派（tile.rs 里 roe_hit_test 的
//! 文档注释已明确写死这一点，Calc 有 roe_calc_hit_test，Impress 同理
//! 需要 roe_impress_ 前缀，不是往 Writer/Calc 的函数里插分支）。

use std::collections::HashMap;
use std::os::raw::c_char;

use roe_model::common::{ConnectionPoint, ShapeGeometry};
use roe_model::impress::Shape;
use roe_model::preset_geometry::{find_preset_shape, get_connection_points, map_connection_point_to_global};
use roe_parser::RoeDocument;

use crate::error::set_last_error;
use crate::handle;

fn with_shape<R>(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    fn_name: &str,
    f: impl FnOnce(&Shape) -> Result<R, String>,
) -> Option<R> {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let shape = find_shape(slide, shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        f(shape)
    });

    match result {
        None => {
            set_last_error(format!("{fn_name}: 无效句柄"));
            None
        }
        Some(Err(msg)) => {
            set_last_error(format!("{fn_name}: {msg}"));
            None
        }
        Some(Ok(v)) => Some(v),
    }
}

/// 形状尺寸变化后，让已求值的路径跟上新尺寸（PresetPathData.commands 是按旧宽高
/// 求值的本地坐标，渲染层与图片裁剪都直接使用它，不会按 extent 再缩放）。
/// 有公式名：用保留的 adj_overrides 按新宽高重新求值；
/// 无公式名（custGeom）：按新旧比例缩放坐标。
fn reflow_path_data(data: &mut roe_model::common::PresetPathData, old_w: f32, old_h: f32, new_w: f32, new_h: f32) {
    use roe_model::common::PathCommand as P;
    if (old_w - new_w).abs() < 1e-4 && (old_h - new_h).abs() < 1e-4 {
        return;
    }
    if !data.preset_name.is_empty() {
        if let Some(formula) = find_preset_shape(&data.preset_name) {
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

pub(crate) fn reflow_geometry(shape: &mut Shape, old_w: f32, old_h: f32) {
    let (nw, nh) = shape.position.extent;
    // v318v: 表格缩放时同步缩放列宽和行高（渲染层按 col_widths/row_height 画）
    if let roe_model::impress::ShapeContent::Table(t) = &mut shape.content {
        if old_w > 0.5 && old_h > 0.5 && nw > 0.0 && nh > 0.0 {
            let (kx, ky) = (nw / old_w, nh / old_h);
            for w in t.col_widths.iter_mut() { *w *= kx; }
            for r in t.rows.iter_mut() {
                if let Some(h) = r.row_height.as_mut() { *h *= ky; }
            }
        }
    }
    if let roe_model::common::ShapeGeometry::PresetPath(data) = &mut shape.geometry {
        reflow_path_data(data, old_w, old_h, nw, nh);
    }
}

fn with_shape_mut<R>(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    fn_name: &str,
    f: impl FnOnce(&mut Shape) -> Result<R, String>,
) -> Option<R> {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let shape = find_shape_mut(slide, shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        f(shape)
    });

    match result {
        None => {
            set_last_error(format!("{fn_name}: 无效句柄"));
            None
        }
        Some(Err(msg)) => {
            set_last_error(format!("{fn_name}: {msg}"));
            None
        }
        Some(Ok(v)) => Some(v),
    }
}

fn find_shape(slide: &roe_model::impress::Slide, shape_id: u32) -> Option<&Shape> {
    slide.shapes.iter().find(|s| s.id == shape_id)
}

fn find_shape_mut(slide: &mut roe_model::impress::Slide, shape_id: u32) -> Option<&mut Shape> {
    slide.shapes.iter_mut().find(|s| s.id == shape_id)
}

fn json_to_cstring(json: String, fn_name: &str) -> *mut c_char {
    match std::ffi::CString::new(json) {
        Ok(cs) => cs.into_raw(),
        Err(e) => {
            set_last_error(format!("{fn_name}: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// 是否为"两点直线"：Line，或预设名为 line / straightConnector1 的 PresetPath。
/// 弯头/曲线连接线与 lineInv 不算（前台仍按普通矩形选择框处理）。
pub(crate) fn shape_is_line(shape: &Shape) -> bool {
    match &shape.geometry {
        roe_model::common::ShapeGeometry::Line => true,
        roe_model::common::ShapeGeometry::PresetPath(d) => {
            d.preset_name == "line" || d.preset_name == "straightConnector1"
        }
        _ => false,
    }
}

/// 按两个端点（slide 全局坐标，单位 pt）设置直线：x1,y1 为起点，x2,y2 为终点。
/// 一次写入 offset/extent，旋转清零，flip_h/flip_v 按走向设置绝对值，并重算路径。
/// 非直线形状返回 false。成功返回 true。
#[no_mangle]
pub extern "C" fn roe_impress_set_line_endpoints(handle: u64, slide_idx: u32, shape_id: u32, x1: f32, y1: f32, x2: f32, y2: f32) -> bool {
    let result = with_shape_mut(handle, slide_idx, shape_id, "roe_impress_set_line_endpoints", |shape| {
        if !shape_is_line(shape) {
            return Err("形状不是直线".to_string());
        }
        let (old_w, old_h) = (shape.position.w(), shape.position.h());
        shape.position.offset = (x1.min(x2), y1.min(y2));
        shape.position.extent = ((x2 - x1).abs(), (y2 - y1).abs());
        shape.position.rotation = Some(0.0);
        shape.position.flip_h = Some(x1 > x2);
        shape.position.flip_v = Some(y1 > y2);
        reflow_geometry(shape, old_w, old_h);
        Ok(())
    });
    result.is_some()
}

/// 查询形状当前几何信息。返回 JSON：
/// x_pt/y_pt/width_pt/height_pt/rotation/flip_h/flip_v，
/// rotation/flip_h/flip_v 为 null 表示该维度未设置（见 Transform 字段注释，
/// 调用方应按默认值处理：rotation=0.0, flip_h=false, flip_v=false）。
/// 失败（无效句柄/非 Impress 文档/slide_idx 越界/shape_id 不存在）返回 null，
/// 详情见 roe_last_error_message。调用方需用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_impress_query_shape_rect(handle: u64, slide_idx: u32, shape_id: u32) -> *mut c_char {
    let json = with_shape(handle, slide_idx, shape_id, "roe_impress_query_shape_rect", |shape| {
        let t = &shape.position;
        let rotation = t.rotation.map(|v| v.to_string()).unwrap_or_else(|| "null".to_string());
        let flip_h = t.flip_h.map(|v| v.to_string()).unwrap_or_else(|| "null".to_string());
        let flip_v = t.flip_v.map(|v| v.to_string()).unwrap_or_else(|| "null".to_string());
        let shear_x = t.shear_x.map(|v| v.to_string()).unwrap_or_else(|| "null".to_string());
        let shear_y = t.shear_y.map(|v| v.to_string()).unwrap_or_else(|| "null".to_string());
        Ok(format!(
            "{{\"x_pt\":{:.2},\"y_pt\":{:.2},\"width_pt\":{:.2},\"height_pt\":{:.2},\"rotation\":{},\"flip_h\":{},\"flip_v\":{},\"shear_x\":{},\"shear_y\":{},\"is_line\":{}}}",
            t.x(), t.y(), t.w(), t.h(), rotation, flip_h, flip_v, shear_x, shear_y, shape_is_line(shape)
        ))
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_impress_query_shape_rect"),
        None => std::ptr::null_mut(),
    }
}

/// 修改形状位置（不改变尺寸/旋转/翻转）。成功返回 true，失败返回 false。
#[no_mangle]
pub extern "C" fn roe_impress_move_shape(handle: u64, slide_idx: u32, shape_id: u32, x_pt: f32, y_pt: f32) -> bool {
    let result = with_shape_mut(handle, slide_idx, shape_id, "roe_impress_move_shape", |shape| {
        shape.position.offset = (x_pt, y_pt);
        Ok(())
    });
    result.is_some()
}

/// 修改形状尺寸（不改变位置/旋转/翻转）。成功返回 true，失败返回 false。
#[no_mangle]
pub extern "C" fn roe_impress_resize_shape(handle: u64, slide_idx: u32, shape_id: u32, width_pt: f32, height_pt: f32) -> bool {
    let result = with_shape_mut(handle, slide_idx, shape_id, "roe_impress_resize_shape", |shape| {
        let (ow, oh) = shape.position.extent;
        shape.position.extent = (width_pt, height_pt);
        reflow_geometry(shape, ow, oh);
        Ok(())
    });
    result.is_some()
}

/// 原子设置形状完整矩形（位置+尺寸一次提交，不改变旋转/翻转）。
/// 成功返回 true，失败返回 false。
/// 用于前端8方向缩放句柄（nw/n/ne/w/e/sw/s/se），避免 resize+move 两次调用
/// 之间的中间不一致窗口，调用方直接传入拖拽结束后的最终矩形即可。
/// 设计对照 roe_set_chart_rect（chart_query.rs），语义与测试场景一致。
#[no_mangle]
pub extern "C" fn roe_impress_set_shape_rect(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    x_pt: f32,
    y_pt: f32,
    width_pt: f32,
    height_pt: f32,
) -> bool {
    let result = with_shape_mut(handle, slide_idx, shape_id, "roe_impress_set_shape_rect", |shape| {
        shape.position.offset = (x_pt, y_pt);
        let (ow, oh) = shape.position.extent;
        shape.position.extent = (width_pt, height_pt);
        reflow_geometry(shape, ow, oh);
        Ok(())
    });
    result.is_some()
}

/// 查询形状的连接点（connection points）全局坐标，用于连接线（Connector）
/// 起止端点的吸附/对齐。仅对预设几何形状（`ShapeGeometry::PresetPath`且
/// `preset_name`非空）有意义，参见 roe-ext_progress_v180/v181 的设计文档：
///
/// - 本地坐标求值：`preset_geometry::get_connection_points`（消费
///   `PresetPathData.preset_name` 查表 + `adj_overrides` 求值，与渲染
///   路径共享同一套具名guide语义，故必须用同一份 adj_overrides，不能
///   假设默认值）
/// - 全局坐标映射：`preset_geometry::map_connection_point_to_global`
///   （消费 Transform 的 offset/extent/rotation/flip_h/flip_v，不消费
///   shear_x/shear_y，理由见该函数文档注释）
///
/// 返回 JSON 数组，每个元素：`{"idx":u, "angle_60000th_deg":f, "x":f,
/// "y":f}`（x/y 为文档全局坐标，pt单位）。以下情况返回**空数组**而非
/// 错误（均为合法情况，不是异常）：
/// - 该预设形状本身没有定义连接点（如13个connector家族自身，语义上
///   "连接别人的"不需要自身被连接，见 roe-ext_progress_v180 §1.1）
///
/// 以下情况返回 **null**（失败，详情见 roe_last_error_message）：
/// - 无效句柄/非 Impress 文档/slide_idx 越界/shape_id 不存在
///   （与 with_shape 其余用法一致）
/// - 形状几何不是 `PresetPath`（Rect/Ellipse/Triangle/Line/Custom
///   均无连接点概念）
/// - `PresetPath` 但 `preset_name` 为空（custGeom 场景，非真预设形状，
///   见 `PresetPathData` 文档注释）
/// - `preset_name` 非空但查不到对应公式（理论不应发生的防御分支，
///   `preset_name` 只应来自 `find_preset_shape` 曾经命中过的名字，
///   若真的出现，多半是数据被绕过正常构造路径手工篡改）
///
/// 调用方需用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_impress_get_connection_points(handle: u64, slide_idx: u32, shape_id: u32) -> *mut c_char {
    let json = with_shape(handle, slide_idx, shape_id, "roe_impress_get_connection_points", |shape| {
        // 阶段C分支1：非 PresetPath 几何（Rect/RoundRect/Ellipse/Triangle/
        // Line/Custom 均无连接点概念）—— 返回错误，不是空数组，因为这是
        // "问了一个没有意义的问题"，与"预设形状但确实没有连接点"性质不同。
        let ShapeGeometry::PresetPath(path_data) = &shape.geometry else {
            return Err("形状几何不是 PresetPath，不支持查询连接点".to_string());
        };

        // 阶段C分支2：custGeom 场景（preset_name 为空，非真预设形状）。
        if path_data.preset_name.is_empty() {
            return Err("形状为 custGeom（自定义路径），不是预设形状，无连接点".to_string());
        }

        // 阶段C分支3：preset_name 非空但查不到公式（理论不应发生的防御分支）。
        let formula = find_preset_shape(&path_data.preset_name).ok_or_else(|| {
            format!(
                "preset_name={} 查不到对应的 PresetShapeFormula（数据可能被绕过正常构造路径篡改）",
                path_data.preset_name
            )
        })?;

        // adj_overrides: Vec<(String, f64)> -> HashMap<&str, f64>，
        // get_connection_points 要求 &HashMap<&str, f64>。
        let adj_map: HashMap<&str, f64> = path_data
            .adj_overrides
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();

        let (w, h) = shape.position.extent;
        let local_points = get_connection_points(formula, w as f64, h as f64, &adj_map);

        // 阶段C分支4：cxn_lst 为空是合法情况（如13个connector家族自身），
        // 返回空数组而非错误。
        let global_points: Vec<(ConnectionPoint, (f32, f32))> = local_points
            .into_iter()
            .map(|cp| {
                let global = map_connection_point_to_global(&cp, &shape.position);
                (cp, global)
            })
            .collect();

        // 阶段D：手写 JSON 拼接，风格对齐 roe_impress_query_shape_rect
        // （不引入 serde）。
        let items: Vec<String> = global_points
            .iter()
            .map(|(cp, (gx, gy))| {
                format!(
                    "{{\"idx\":{},\"angle_60000th_deg\":{:.2},\"x\":{:.2},\"y\":{:.2}}}",
                    cp.idx, cp.angle_60000th_deg, gx, gy
                )
            })
            .collect();

        Ok(format!("[{}]", items.join(",")))
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_impress_get_connection_points"),
        None => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use roe_model::common::{ShapeStyle, Transform};
    use roe_model::impress::{AnimationNode, Slide, ShapeContent, ShapeGeometry, TextAnchor, TextAutofit, TextBodyProps, TextVert, Presentation};
    use roe_model::style::StyleRegistry;
    use crate::handle::{self, DocumentEntry, WriterOptions};

    fn make_shape(id: u32, x: f32, y: f32, w: f32, h: f32) -> Shape {
        Shape {
            id,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(x, y, w, h),
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
        }
    }

    fn make_impress_handle_with_shape() -> (u64, u32) {
        let shape_id = 1u32;
        let pres = Presentation {
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![make_shape(shape_id, 10.0, 10.0, 100.0, 80.0)],
                master_idx: None,
                animation_tree: AnimationNode::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            styles: StyleRegistry::default(),
            theme_colors: HashMap::new(),
            default_text_style: Default::default(),
        };

        let h = handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        });
        (h, shape_id)
    }

    #[test]
    fn test_set_shape_rect_single_call_is_accurate() {
        let (h, shape_id) = make_impress_handle_with_shape();
        let ok = roe_impress_set_shape_rect(h, 0, shape_id, 5.0, 6.0, 50.0, 40.0);
        assert!(ok, "set_shape_rect should succeed");

        let got = with_shape(h, 0, shape_id, "test", |s| {
            Ok((s.position.x(), s.position.y(), s.position.w(), s.position.h()))
        }).unwrap();
        assert_eq!(got, (5.0, 6.0, 50.0, 40.0));
    }

    #[test]
    fn test_set_shape_rect_repeated_nw_no_accumulated_drift() {
        let (h, shape_id) = make_impress_handle_with_shape();

        let rects = [
            (20.0_f32, 15.0_f32, 90.0_f32, 75.0_f32),
            (30.0, 25.0, 80.0, 65.0),
            (15.0, 40.0, 95.0, 50.0),
            (25.0, 30.0, 85.0, 60.0),
        ];

        for (x, y, w, hh) in rects.iter() {
            let ok = roe_impress_set_shape_rect(h, 0, shape_id, *x, *y, *w, *hh);
            assert!(ok);
            let got = with_shape(h, 0, shape_id, "test", |s| {
                Ok((s.position.x(), s.position.y(), s.position.w(), s.position.h()))
            }).unwrap();
            assert_eq!(got, (*x, *y, *w, *hh));
            let (gx, gy, gw, gh) = got;
            assert!((gx + gw - 110.0).abs() < 1e-4, "右下角x应保持110, got {}", gx + gw);
            assert!((gy + gh - 90.0).abs() < 1e-4, "右下角y应保持90, got {}", gy + gh);
        }
    }

    #[test]
    fn test_move_and_resize_are_independent() {
        let (h, shape_id) = make_impress_handle_with_shape();

        assert!(roe_impress_move_shape(h, 0, shape_id, 50.0, 60.0));
        let got = with_shape(h, 0, shape_id, "test", |s| {
            Ok((s.position.x(), s.position.y(), s.position.w(), s.position.h()))
        }).unwrap();
        assert_eq!(got, (50.0, 60.0, 100.0, 80.0), "move 不应改变尺寸");

        assert!(roe_impress_resize_shape(h, 0, shape_id, 200.0, 150.0));
        let got = with_shape(h, 0, shape_id, "test", |s| {
            Ok((s.position.x(), s.position.y(), s.position.w(), s.position.h()))
        }).unwrap();
        assert_eq!(got, (50.0, 60.0, 200.0, 150.0), "resize 不应改变位置");
    }

    #[test]
    fn test_invalid_shape_id_returns_false_or_null() {
        let (h, _shape_id) = make_impress_handle_with_shape();
        assert!(!roe_impress_move_shape(h, 0, 9999, 1.0, 2.0));
        assert!(!roe_impress_resize_shape(h, 0, 9999, 1.0, 2.0));
        assert!(!roe_impress_set_shape_rect(h, 0, 9999, 1.0, 2.0, 3.0, 4.0));
        assert!(roe_impress_query_shape_rect(h, 0, 9999).is_null());
    }

    #[test]
    fn test_invalid_slide_idx_returns_false_or_null() {
        let (h, shape_id) = make_impress_handle_with_shape();
        assert!(!roe_impress_move_shape(h, 99, shape_id, 1.0, 2.0));
        assert!(roe_impress_query_shape_rect(h, 99, shape_id).is_null());
    }

    #[test]
    fn test_invalid_handle_returns_false_or_null() {
        assert!(!roe_impress_move_shape(999_999, 0, 1, 1.0, 2.0));
        assert!(roe_impress_query_shape_rect(999_999, 0, 1).is_null());
    }

    #[test]
    fn test_query_shape_rect_rotation_flip_are_null_when_unset() {
        let (h, shape_id) = make_impress_handle_with_shape();
        let ptr = roe_impress_query_shape_rect(h, 0, shape_id);
        assert!(!ptr.is_null());
        let s = unsafe { std::ffi::CString::from_raw(ptr) }.into_string().unwrap();
        assert!(s.contains("\"rotation\":null"), "got: {s}");
        assert!(s.contains("\"flip_h\":null"), "got: {s}");
        assert!(s.contains("\"flip_v\":null"), "got: {s}");
    }
}

/// 在 slide.shapes 中查找 shape_id 对应的下标（不是 shape 本身），
/// 供层级操作（前移/后移/置顶/置底）使用——这类操作本质是对 Vec 做
/// remove+insert，需要下标而非引用。
fn find_shape_index(slide: &roe_model::impress::Slide, shape_id: u32) -> Option<usize> {
    slide.shapes.iter().position(|s| s.id == shape_id)
}

/// 取出指定句柄、指定 slide_idx 的 Slide 可写引用，供层级操作使用
/// （层级操作直接操作 Vec 本身的顺序，不是操作单个 Shape 字段，因此
/// 复用粒度是 Slide 而非 Shape，与 with_shape_mut 不同）。
fn with_slide_mut<R>(
    handle: u64,
    slide_idx: u32,
    fn_name: &str,
    f: impl FnOnce(&mut roe_model::impress::Slide) -> Result<R, String>,
) -> Option<R> {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        f(slide)
    });

    match result {
        None => {
            set_last_error(format!("{fn_name}: 无效句柄"));
            None
        }
        Some(Err(msg)) => {
            set_last_error(format!("{fn_name}: {msg}"));
            None
        }
        Some(Ok(v)) => Some(v),
    }
}

/// 形状前移一层（在 shapes 数组中与后一个元素交换位置，index+1，
/// 视觉上向上层移动一层）。已在最顶层（index 已是末尾）时视为
/// no-op，仍返回 true（与 UNO MoveForward 在已到顶时的静默行为一致，
/// 不是错误）。shape_id 不存在/slide_idx 越界/无效句柄返回 false。
#[no_mangle]
pub extern "C" fn roe_impress_shape_move_forward(handle: u64, slide_idx: u32, shape_id: u32) -> bool {
    let result = with_slide_mut(handle, slide_idx, "roe_impress_shape_move_forward", |slide| {
        let idx = find_shape_index(slide, shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        if idx + 1 < slide.shapes.len() {
            slide.shapes.swap(idx, idx + 1);
        }
        Ok(())
    });
    result.is_some()
}

/// 形状后移一层（与前一个元素交换位置，index-1）。已在最底层时
/// no-op，仍返回 true。语义同 roe_impress_shape_move_forward。
#[no_mangle]
pub extern "C" fn roe_impress_shape_move_backward(handle: u64, slide_idx: u32, shape_id: u32) -> bool {
    let result = with_slide_mut(handle, slide_idx, "roe_impress_shape_move_backward", |slide| {
        let idx = find_shape_index(slide, shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        if idx > 0 {
            slide.shapes.swap(idx, idx - 1);
        }
        Ok(())
    });
    result.is_some()
}

/// 形状置顶（移到 shapes 数组末尾，渲染顺序最靠后=视觉最顶层，
/// 见 slide_renderer.rs render_slide 的 "按原始顺序渲染保持z-order"
/// 注释）。已在顶层时 no-op，仍返回 true。
#[no_mangle]
pub extern "C" fn roe_impress_shape_bring_to_front(handle: u64, slide_idx: u32, shape_id: u32) -> bool {
    let result = with_slide_mut(handle, slide_idx, "roe_impress_shape_bring_to_front", |slide| {
        let idx = find_shape_index(slide, shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        let last = slide.shapes.len() - 1;
        if idx != last {
            let shape = slide.shapes.remove(idx);
            slide.shapes.push(shape);
        }
        Ok(())
    });
    result.is_some()
}

/// 形状置底（移到 shapes 数组开头）。已在底层时 no-op，仍返回 true。
#[no_mangle]
pub extern "C" fn roe_impress_shape_send_to_back(handle: u64, slide_idx: u32, shape_id: u32) -> bool {
    let result = with_slide_mut(handle, slide_idx, "roe_impress_shape_send_to_back", |slide| {
        let idx = find_shape_index(slide, shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        if idx != 0 {
            let shape = slide.shapes.remove(idx);
            slide.shapes.insert(0, shape);
        }
        Ok(())
    });
    result.is_some()
}

#[cfg(test)]
mod tests_zorder {
    use super::*;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use roe_model::common::{ShapeStyle, Transform};
    use roe_model::impress::{AnimationNode, Slide, ShapeContent, ShapeGeometry, TextAnchor, TextAutofit, TextBodyProps, TextVert, Presentation};
    use roe_model::style::StyleRegistry;
    use crate::handle::{self, DocumentEntry, WriterOptions};

    fn make_shape(id: u32) -> Shape {
        Shape {
            id,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(0.0, 0.0, 10.0, 10.0),
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
        }
    }

    /// 构造一个含 4 个 shape 的 slide，id 依次为 1/2/3/4，
    /// 初始数组顺序即 [1,2,3,4]（4 在最顶层，见 render_slide 注释）。
    fn make_handle_with_shapes() -> u64 {
        let pres = Presentation {
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![make_shape(1), make_shape(2), make_shape(3), make_shape(4)],
                master_idx: None,
                animation_tree: AnimationNode::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            styles: StyleRegistry::default(),
            theme_colors: HashMap::new(),
            default_text_style: Default::default(),
        };

        handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn shape_order(h: u64) -> Vec<u32> {
        with_slide_mut(h, 0, "test", |slide| {
            Ok(slide.shapes.iter().map(|s| s.id).collect::<Vec<_>>())
        }).unwrap()
    }

    #[test]
    fn test_move_forward_swaps_with_next() {
        let h = make_handle_with_shapes();
        assert!(roe_impress_shape_move_forward(h, 0, 2));
        assert_eq!(shape_order(h), vec![1, 3, 2, 4]);
    }

    #[test]
    fn test_move_backward_swaps_with_prev() {
        let h = make_handle_with_shapes();
        assert!(roe_impress_shape_move_backward(h, 0, 3));
        assert_eq!(shape_order(h), vec![1, 3, 2, 4]);
    }

    #[test]
    fn test_move_forward_at_top_is_noop_returns_true() {
        let h = make_handle_with_shapes();
        assert!(roe_impress_shape_move_forward(h, 0, 4));
        assert_eq!(shape_order(h), vec![1, 2, 3, 4], "已在顶层，顺序不应改变");
    }

    #[test]
    fn test_move_backward_at_bottom_is_noop_returns_true() {
        let h = make_handle_with_shapes();
        assert!(roe_impress_shape_move_backward(h, 0, 1));
        assert_eq!(shape_order(h), vec![1, 2, 3, 4], "已在底层，顺序不应改变");
    }

    #[test]
    fn test_bring_to_front_moves_to_end() {
        let h = make_handle_with_shapes();
        assert!(roe_impress_shape_bring_to_front(h, 0, 1));
        assert_eq!(shape_order(h), vec![2, 3, 4, 1]);
    }

    #[test]
    fn test_bring_to_front_already_at_front_is_noop() {
        let h = make_handle_with_shapes();
        assert!(roe_impress_shape_bring_to_front(h, 0, 4));
        assert_eq!(shape_order(h), vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_send_to_back_moves_to_start() {
        let h = make_handle_with_shapes();
        assert!(roe_impress_shape_send_to_back(h, 0, 4));
        assert_eq!(shape_order(h), vec![4, 1, 2, 3]);
    }

    #[test]
    fn test_send_to_back_already_at_back_is_noop() {
        let h = make_handle_with_shapes();
        assert!(roe_impress_shape_send_to_back(h, 0, 1));
        assert_eq!(shape_order(h), vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_zorder_invalid_shape_id_returns_false() {
        let h = make_handle_with_shapes();
        assert!(!roe_impress_shape_move_forward(h, 0, 9999));
        assert!(!roe_impress_shape_move_backward(h, 0, 9999));
        assert!(!roe_impress_shape_bring_to_front(h, 0, 9999));
        assert!(!roe_impress_shape_send_to_back(h, 0, 9999));
    }

    #[test]
    fn test_zorder_invalid_slide_idx_returns_false() {
        let h = make_handle_with_shapes();
        assert!(!roe_impress_shape_move_forward(h, 99, 1));
    }

    #[test]
    fn test_zorder_invalid_handle_returns_false() {
        assert!(!roe_impress_shape_move_forward(999_999, 0, 1));
    }
}

/// 形状水平镜像（左右翻转），切换 flip_h 当前状态（None 视为 false，
/// 切换后变 Some(true)；Some(v) 切换为 Some(!v)）。对应 UNO Mirror/
/// MirrorHorz 的切换语义，比"设为true/false"更贴近用户实际点击一个
/// 镜像按钮的心智模型，且切换操作幂等安全（点两次等于没点）。
/// 成功返回 true，shape_id 不存在/slide_idx 越界/无效句柄返回 false。
#[no_mangle]
pub extern "C" fn roe_impress_flip_shape_h(handle: u64, slide_idx: u32, shape_id: u32) -> bool {
    let result = with_shape_mut(handle, slide_idx, shape_id, "roe_impress_flip_shape_h", |shape| {
        shape.position.flip_h = Some(!shape.position.flip_h.unwrap_or(false));
        Ok(())
    });
    result.is_some()
}

/// 形状垂直镜像（上下翻转），切换 flip_v。语义同 roe_impress_flip_shape_h。
#[no_mangle]
pub extern "C" fn roe_impress_flip_shape_v(handle: u64, slide_idx: u32, shape_id: u32) -> bool {
    let result = with_shape_mut(handle, slide_idx, shape_id, "roe_impress_flip_shape_v", |shape| {
        shape.position.flip_v = Some(!shape.position.flip_v.unwrap_or(false));
        Ok(())
    });
    result.is_some()
}

/// 设置形状绝对旋转角度（度，非增量旋转），避免多次调用累积误差
/// （与 roe_impress_set_shape_rect 的"原子设置优于增量修改"设计哲学
/// 一致）。调用方需要"旋转90度"这种相对语义时，应先查询当前角度
/// （roe_impress_query_shape_rect 返回的 rotation，null 视为0.0）
/// 再加上増量后调用本函数一次性设置。
/// 成功返回 true，shape_id 不存在/slide_idx 越界/无效句柄返回 false。
#[no_mangle]
pub extern "C" fn roe_impress_set_shape_rotation(handle: u64, slide_idx: u32, shape_id: u32, degrees: f32) -> bool {
    let result = with_shape_mut(handle, slide_idx, shape_id, "roe_impress_set_shape_rotation", |shape| {
        shape.position.rotation = Some(degrees);
        Ok(())
    });
    result.is_some()
}

/// 设置形状的拉伸变形（shear）角度，单位度。绝对值替换，不是累加，
/// 与 roe_impress_set_shape_rotation 的语义一致。shear_x 为水平方向
/// 剪切角度，shear_y 为垂直方向剪切角度，两者独立设置，互不影响。
/// 已知限制：写入后命中测试（roe_impress_hit_test）的包围盒判定
/// 暂不消费此字段，仍按未拉伸的矩形/椭圆几何判定，见
/// point_hits_shape 函数文档注释。成功返回 true，失败返回 false。
#[no_mangle]
pub extern "C" fn roe_impress_set_shape_shear(handle: u64, slide_idx: u32, shape_id: u32, shear_x_deg: f32, shear_y_deg: f32) -> bool {
    let result = with_shape_mut(handle, slide_idx, shape_id, "roe_impress_set_shape_shear", |shape| {
        shape.position.shear_x = Some(shear_x_deg);
        shape.position.shear_y = Some(shear_y_deg);
        Ok(())
    });
    result.is_some()
}

#[cfg(test)]
mod tests_transform {
    use super::*;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use roe_model::common::{ShapeStyle, Transform};
    use roe_model::impress::{AnimationNode, Slide, ShapeContent, ShapeGeometry, TextAnchor, TextAutofit, TextBodyProps, TextVert, Presentation};
    use roe_model::style::StyleRegistry;
    use crate::handle::{self, DocumentEntry, WriterOptions};

    fn make_shape(id: u32) -> Shape {
        Shape {
            id,
            geometry: ShapeGeometry::Rect,
            position: Transform::from_rect(0.0, 0.0, 10.0, 10.0),
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
        }
    }

    fn make_handle_with_shape() -> (u64, u32) {
        let shape_id = 1u32;
        let pres = Presentation {
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![make_shape(shape_id)],
                master_idx: None,
                animation_tree: AnimationNode::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            styles: StyleRegistry::default(),
            theme_colors: HashMap::new(),
            default_text_style: Default::default(),
        };

        let h = handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        });
        (h, shape_id)
    }

    fn get_flip_h(h: u64, shape_id: u32) -> Option<bool> {
        with_shape(h, 0, shape_id, "test", |s| Ok(s.position.flip_h)).unwrap()
    }

    fn get_flip_v(h: u64, shape_id: u32) -> Option<bool> {
        with_shape(h, 0, shape_id, "test", |s| Ok(s.position.flip_v)).unwrap()
    }

    fn get_rotation(h: u64, shape_id: u32) -> Option<f32> {
        with_shape(h, 0, shape_id, "test", |s| Ok(s.position.rotation)).unwrap()
    }

    fn get_shear(h: u64, shape_id: u32) -> (Option<f32>, Option<f32>) {
        with_shape(h, 0, shape_id, "test", |s| Ok((s.position.shear_x, s.position.shear_y))).unwrap()
    }

    #[test]
    fn test_flip_h_toggles_from_none_to_true() {
        let (h, id) = make_handle_with_shape();
        assert_eq!(get_flip_h(h, id), None);
        assert!(roe_impress_flip_shape_h(h, 0, id));
        assert_eq!(get_flip_h(h, id), Some(true));
    }

    #[test]
    fn test_flip_h_toggles_back_to_false() {
        let (h, id) = make_handle_with_shape();
        assert!(roe_impress_flip_shape_h(h, 0, id));
        assert!(roe_impress_flip_shape_h(h, 0, id));
        assert_eq!(get_flip_h(h, id), Some(false));
    }

    #[test]
    fn test_flip_v_toggles_independently_of_flip_h() {
        let (h, id) = make_handle_with_shape();
        assert!(roe_impress_flip_shape_v(h, 0, id));
        assert_eq!(get_flip_v(h, id), Some(true));
        assert_eq!(get_flip_h(h, id), None, "flip_v不应影响flip_h");
    }

    #[test]
    fn test_set_rotation_sets_absolute_value() {
        let (h, id) = make_handle_with_shape();
        assert!(roe_impress_set_shape_rotation(h, 0, id, 45.0));
        assert_eq!(get_rotation(h, id), Some(45.0));
        // 再次设置应是绝对值替换，不是累加
        assert!(roe_impress_set_shape_rotation(h, 0, id, 90.0));
        assert_eq!(get_rotation(h, id), Some(90.0));
    }

    #[test]
    fn test_set_shear_sets_independent_xy_values() {
        let (h, id) = make_handle_with_shape();
        assert_eq!(get_shear(h, id), (None, None));
        assert!(roe_impress_set_shape_shear(h, 0, id, 15.0, -8.0));
        assert_eq!(get_shear(h, id), (Some(15.0), Some(-8.0)));
        // 再次设置应是绝对值替换，不是累加，且不影响rotation/flip等其他维度
        assert!(roe_impress_set_shape_shear(h, 0, id, 0.0, 20.0));
        assert_eq!(get_shear(h, id), (Some(0.0), Some(20.0)));
        assert_eq!(get_rotation(h, id), None, "设置shear不应影响rotation");
    }

    #[test]
    fn test_transform_invalid_shape_id_returns_false() {
        let (h, _id) = make_handle_with_shape();
        assert!(!roe_impress_flip_shape_h(h, 0, 9999));
        assert!(!roe_impress_flip_shape_v(h, 0, 9999));
        assert!(!roe_impress_set_shape_rotation(h, 0, 9999, 10.0));
        assert!(!roe_impress_set_shape_shear(h, 0, 9999, 10.0, 5.0));
    }

    #[test]
    fn test_transform_invalid_handle_returns_false() {
        assert!(!roe_impress_flip_shape_h(999_999, 0, 1));
        assert!(!roe_impress_set_shape_rotation(999_999, 0, 1, 10.0));
        assert!(!roe_impress_set_shape_shear(999_999, 0, 1, 10.0, 5.0));
    }
}

/// 判断点 (px, py)（slide 全局坐标系，单位 pt）是否落在形状 shape 内。
/// 先把点变换到形状本地坐标系（撤销旋转，以形状中心为原点），再按
/// 几何类型判定：Rect/RoundRect/Line 及其余未特殊处理的类型（Custom/
/// PresetPath 等）统一按外接矩形判定；Ellipse 按椭圆方程判定。
/// flip_h/flip_v 不影响命中范围（镜像不改变外接矩形，只影响内容
/// 绘制），此处忽略。
/// 已知局限：Custom/PresetPath 复杂路径未做精确多边形命中，退化为
/// 矩形判定——足够满足"点击选中形状"场景，不追求逐像素精确。
pub(crate) fn point_hits_shape(px: f32, py: f32, shape: &Shape) -> bool {
    let t = &shape.position;
    let (w, h) = (t.w(), t.h());
    // 横线/竖线一维为 0：只拒绝负尺寸和宽高皆为 0 的点，零维方向给最小厚度。
    if w < 0.0 || h < 0.0 || (w == 0.0 && h == 0.0) {
        return false;
    }
    let cx = t.x() + w / 2.0;
    let cy = t.y() + h / 2.0;
    let dx = px - cx;
    let dy = py - cy;

    // 逆向旋转：把点从全局坐标系转回形状未旋转时的本地坐标系。
    let (ldx, ldy) = match t.rotation {
        Some(deg) if deg != 0.0 => {
            let rad = -deg.to_radians(); // 逆向，所以取负角度
            let (sin, cos) = rad.sin_cos();
            (dx * cos - dy * sin, dx * sin + dy * cos)
        }
        _ => (dx, dy),
    };

    // 最小半厚度 4pt：只对零维方向生效，方便点中横线/竖线。
    const MIN_HALF: f32 = 4.0;
    let hw = if w == 0.0 { MIN_HALF } else { w / 2.0 };
    let hh = if h == 0.0 { MIN_HALF } else { h / 2.0 };

    match shape.geometry {
        roe_model::common::ShapeGeometry::Ellipse => {
            // 椭圆方程：(ldx/hw)^2 + (ldy/hh)^2 <= 1
            (ldx / hw).powi(2) + (ldy / hh).powi(2) <= 1.0
        }
        // Rect/RoundRect/Line/PresetPath/Custom 等统一按外接矩形判定
        // （已知局限，见函数文档注释）。
        _ => ldx.abs() <= hw && ldy.abs() <= hh,
    }
}

/// 给定 slide 内一点坐标，反查命中的最顶层形状 id。
/// 参数：x_pt/y_pt 为 slide 全局坐标系下的坐标（单位 pt，与
/// roe_impress_query_shape_rect 的坐标系一致）。
/// 返回 JSON `{"shape_id":N}`，未命中任何形状时返回 `{"shape_id":null}`
/// （未命中不是错误，是合法结果，与 roe_hit_test 对越界坐标"钳制到
/// 合理落点"的容错精神一致，调用方不需要因为点在空白处而特殊处理
/// 错误分支）。只在 slide_idx 越界/无效句柄/非 Impress 文档时返回
/// null 指针，详情见 roe_last_error_message。
/// 只检索 slide.shapes（可编辑内容层），不含 master/layout 背景装饰
/// 形状——与 insert_shape/delete_shape（roe-script/impress_api.rs）
/// 的操作范围一致，母版/版式形状本身不是用户可直接点击编辑的对象。
/// 多个形状重叠命中时，返回 z-order 最顶层的（shapes 数组下标最大者，
/// 见 slide_renderer.rs render_slide "按原始顺序渲染保持z-order" 注释）。
/// 调用方需用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_impress_hit_test(handle: u64, slide_idx: u32, x_pt: f32, y_pt: f32) -> *mut c_char {
    let json = with_slide_ref(handle, slide_idx, "roe_impress_hit_test", |slide| {
        let hit_id = slide
            .shapes
            .iter()
            .rev() // 从末尾（最顶层）往前找，第一个命中的就是最顶层
            .find(|s| point_hits_shape(x_pt, y_pt, s))
            .map(|s| s.id);
        Ok(match hit_id {
            Some(id) => format!("{{\"shape_id\":{id}}}"),
            None => "{\"shape_id\":null}".to_string(),
        })
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_impress_hit_test"),
        None => std::ptr::null_mut(),
    }
}

/// 取出指定句柄、指定 slide_idx 的 Slide 只读引用，供 hit_test 使用
/// （与 with_slide_mut 对称的只读版本；层级操作用 mut，hit_test 只读）。
fn with_slide_ref<R>(
    handle: u64,
    slide_idx: u32,
    fn_name: &str,
    f: impl FnOnce(&roe_model::impress::Slide) -> Result<R, String>,
) -> Option<R> {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        f(slide)
    });

    match result {
        None => {
            set_last_error(format!("{fn_name}: 无效句柄"));
            None
        }
        Some(Err(msg)) => {
            set_last_error(format!("{fn_name}: {msg}"));
            None
        }
        Some(Ok(v)) => Some(v),
    }
}

#[cfg(test)]
mod tests_hit_test {
    use super::*;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use roe_model::common::{ShapeStyle, Transform, ShapeGeometry};
    use roe_model::impress::{AnimationNode, Slide, ShapeContent, TextAnchor, TextAutofit, TextBodyProps, TextVert, Presentation};
    use roe_model::style::StyleRegistry;
    use crate::handle::{self, DocumentEntry, WriterOptions};


    // ---- roe_impress_get_connection_points 测试（v181 阶段E）----

    fn make_triangle_shape(id: u32, x: f32, y: f32, w: f32, h: f32, rotation: Option<f32>, flip_h: Option<bool>, flip_v: Option<bool>) -> Shape {
        use roe_model::common::PresetPathData;
        let mut position = Transform::from_rect(x, y, w, h);
        position.rotation = rotation;
        position.flip_h = flip_h;
        position.flip_v = flip_v;
        Shape {
            id,
            geometry: ShapeGeometry::PresetPath(PresetPathData {
                commands: vec![],
                preset_name: "triangle".to_string(),
                adj_overrides: vec![],
            }),
            position,
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
        }
    }

    fn make_impress_handle_with_custom_shape(shape: Shape) -> (u64, u32) {
        let shape_id = shape.id;
        let pres = Presentation {
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![shape],
                master_idx: None,
                animation_tree: AnimationNode::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            styles: StyleRegistry::default(),
            theme_colors: HashMap::new(),
            default_text_style: Default::default(),
        };

        let h = handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        });
        (h, shape_id)
    }

    // ---- 端到端测试：真实 pptx 样本（v181 §5.1 遗留任务，用
    // scan_connection_point_candidates 探测脚本筛选得到的候选文件）----

    fn find_shape_by_id<'a>(shapes: &'a [Shape], target_id: u32) -> Option<&'a Shape> {
        for shape in shapes {
            if shape.id == target_id {
                return Some(shape);
            }
            if let roe_model::impress::ShapeContent::Group { children, .. } = &shape.content {
                if let Some(found) = find_shape_by_id(children, target_id) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// 解析真实 pptx 文件，把指定 slide/shape 的 Presentation 塞进一个新
    /// handle 里（roe_impress_get_connection_points 只认 handle 内的文档，
    /// 不能直接对裸 Presentation 调用，所以要经过 handle::insert 这一步，
    /// 与内存构造 Shape 的测试路径一致，只是 Presentation 来自真实解析而
    /// 非手工构造）。
    fn load_real_pptx_handle(rel_path: &str) -> roe_model::impress::Presentation {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let full_path = std::path::Path::new(manifest_dir).join("..").join(rel_path);
        let data = std::fs::read(&full_path)
            .unwrap_or_else(|e| panic!("读取测试样本失败 {full_path:?}: {e}"));
        let doc = roe_parser::ooxml::pptx::PptxParser::parse(&data)
            .unwrap_or_else(|e| panic!("解析测试样本失败 {rel_path}: {e:?}"));
        doc
    }

    fn insert_presentation_handle(pres: roe_model::impress::Presentation) -> u64 {
        handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    #[test]
    fn test_get_connection_points_real_pptx_bent_connector3_empty() {
        // bentConnector3 的 cxn_lst 是 &[]（见 preset_geometry.rs），真实
        // connector 家族形状应返回空数组 "[]" 而非报错——验证 gap-analysis
        // §1.1 "connector家族自身无连接点，属合法情况" 在真实文件上成立。
        let pres = load_real_pptx_handle("test_samples/pptx/oox_unit_data__elbowConnectors.pptx");
        let shape = find_shape_by_id(&pres.slides[0].shapes, 7)
            .unwrap_or_else(|| panic!("shape_id=7 not found in elbowConnectors.pptx slide 0"));
        assert!(matches!(&shape.geometry, ShapeGeometry::PresetPath(pd) if pd.preset_name == "bentConnector3"));

        let h = insert_presentation_handle(pres);
        let raw = roe_impress_get_connection_points(h, 0, 7);
        assert!(!raw.is_null());
        let json = unsafe { std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned() };
        crate::roe_free_string(raw);
        assert_eq!(json, "[]", "bentConnector3 应返回空数组，实际: {json}");
    }

    #[test]
    fn test_get_connection_points_real_pptx_down_arrow_with_transform() {
        // downArrow 带 rotation/flip（tdf91999_rotateShape.pptx 文件名本身
        // 即LOK官方旋转回归测试用例），4个连接点非空，交叉验证纯函数链路
        // 与 C-ABI 输出一致，覆盖"真实文件 adj_overrides + Transform 一起
        // 生效"的完整路径（内存构造测试只验证了其中一个维度）。
        let pres = load_real_pptx_handle("test_samples/pptx/sd_data_pptx__tdf91999_rotateShape.pptx");
        let shape = find_shape_by_id(&pres.slides[0].shapes, 39)
            .unwrap_or_else(|| panic!("shape_id=39 not found in tdf91999_rotateShape.pptx slide 0"));
        let ShapeGeometry::PresetPath(path_data) = &shape.geometry else {
            panic!("shape_id=39 不是 PresetPath 几何");
        };
        assert_eq!(path_data.preset_name, "downArrow");
        assert!(
            shape.position.rotation.is_some()
                || shape.position.flip_h.unwrap_or(false)
                || shape.position.flip_v.unwrap_or(false),
            "该样本应带有旋转/翻转变换，探测脚本已确认 transform=true，实际 position={:?}",
            shape.position
        );

        // 独立走一遍纯函数链路算期望值。
        let formula = find_preset_shape("downArrow").unwrap();
        let adj_map: HashMap<&str, f64> = path_data
            .adj_overrides
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        let (w, h_) = shape.position.extent;
        let local_points = get_connection_points(formula, w as f64, h_ as f64, &adj_map);
        assert_eq!(local_points.len(), 4, "downArrow 应有4个连接点");
        let expected: Vec<(f32, f32)> = local_points
            .iter()
            .map(|cp| map_connection_point_to_global(cp, &shape.position))
            .collect();

        let handle_id = insert_presentation_handle(pres);
        let raw = roe_impress_get_connection_points(handle_id, 0, 39);
        assert!(!raw.is_null());
        let json = unsafe { std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned() };
        crate::roe_free_string(raw);

        for (gx, gy) in &expected {
            let x_marker = format!("\"x\":{:.2}", gx);
            let y_marker = format!("\"y\":{:.2}", gy);
            assert!(json.contains(&x_marker), "expected x {gx} not found in {json}");
            assert!(json.contains(&y_marker), "expected y {gy} not found in {json}");
        }
    }

    #[test]
    fn test_get_connection_points_triangle_no_transform() {
        // w=100,h=80，offset=(10,10)，无rotation/flip，adj用默认50000。
        // 本地坐标手算：(50,0) (25,40) (0,80) (50,80) (100,80) (75,40)
        // 无变换时全局坐标 = 本地坐标 + offset(10,10)。
        let shape = make_triangle_shape(1, 10.0, 10.0, 100.0, 80.0, None, None, None);
        let (h, shape_id) = make_impress_handle_with_custom_shape(shape);

        let raw = roe_impress_get_connection_points(h, 0, shape_id);
        assert!(!raw.is_null());
        let json = unsafe { std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned() };
        crate::roe_free_string(raw);

        let expected = [(60.0, 10.0), (35.0, 50.0), (10.0, 90.0), (60.0, 90.0), (110.0, 90.0), (85.0, 50.0)];
        for (idx, (ex, ey)) in expected.iter().enumerate() {
            let needle_x = format!("\"idx\":{},\"angle_60000th_deg\"", idx);
            assert!(json.contains(&needle_x), "missing idx {} in {}", idx, json);
            let x_marker = format!("\"x\":{:.2}", ex);
            let y_marker = format!("\"y\":{:.2}", ey);
            assert!(json.contains(&x_marker), "expected x {} not found in {}", ex, json);
            assert!(json.contains(&y_marker), "expected y {} not found in {}", ey, json);
        }
    }

    #[test]
    fn test_get_connection_points_triangle_with_rotation_and_flip() {
        // 与 v180/v181 一致，用 rotation+flip 组合验证 map_connection_point_to_global
        // 被正确调用（不重复推导数值，只验证与直接调用两个纯函数手算的一致性）。
        let shape = make_triangle_shape(1, 10.0, 10.0, 100.0, 80.0, Some(90.0), Some(true), None);
        let (h, shape_id) = make_impress_handle_with_custom_shape(shape.clone());

        let raw = roe_impress_get_connection_points(h, 0, shape_id);
        assert!(!raw.is_null());
        let json = unsafe { std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned() };
        crate::roe_free_string(raw);

        // 独立走一遍纯函数链路算期望值，交叉验证 C-ABI 包装没有引入偏差。
        let formula = find_preset_shape("triangle").unwrap();
        let adj_map: HashMap<&str, f64> = HashMap::new();
        let local_points = get_connection_points(formula, 100.0, 80.0, &adj_map);
        assert_eq!(local_points.len(), 6);
        for cp in &local_points {
            let (gx, gy) = map_connection_point_to_global(cp, &shape.position);
            let x_marker = format!("\"x\":{:.2}", gx);
            let y_marker = format!("\"y\":{:.2}", gy);
            assert!(json.contains(&x_marker), "expected x {} not found in {}", gx, json);
            assert!(json.contains(&y_marker), "expected y {} not found in {}", gy, json);
        }
    }

    #[test]
    fn test_get_connection_points_non_preset_path_errors() {
        // Rect 几何无连接点概念，应返回 null（失败），不是空数组。
        // 用本模块内的 make_impress_handle_with_custom_shape 而非 mod tests
        // 的 make_impress_handle_with_shape：test 子模块各自独立作用域
        // （v181 §4.4 已记录这一点），不能跨模块直接引用。
        let position = Transform::from_rect(0.0, 0.0, 50.0, 50.0);
        let shape = Shape {
            id: 1,
            geometry: ShapeGeometry::Rect,
            position,
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
        let (h, shape_id) = make_impress_handle_with_custom_shape(shape);
        let raw = roe_impress_get_connection_points(h, 0, shape_id);
        assert!(raw.is_null());
    }

    #[test]
    fn test_get_connection_points_custgeom_errors() {
        use roe_model::common::PresetPathData;
        let mut position = Transform::from_rect(0.0, 0.0, 50.0, 50.0);
        position.rotation = None;
        let shape = Shape {
            id: 1,
            geometry: ShapeGeometry::PresetPath(PresetPathData {
                commands: vec![],
                preset_name: String::new(), // custGeom场景：空字符串
                adj_overrides: vec![],
            }),
            position,
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
        let (h, shape_id) = make_impress_handle_with_custom_shape(shape);
        let raw = roe_impress_get_connection_points(h, 0, shape_id);
        assert!(raw.is_null());
    }

    fn make_shape_geo(id: u32, x: f32, y: f32, w: f32, h: f32, geometry: ShapeGeometry, rotation: Option<f32>) -> Shape {
        let mut position = Transform::from_rect(x, y, w, h);
        position.rotation = rotation;
        Shape {
            id,
            geometry,
            position,
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
        }
    }

    fn make_handle_with_shapes(shapes: Vec<Shape>) -> u64 {
        let pres = Presentation {
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes,
                master_idx: None,
                animation_tree: AnimationNode::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            styles: StyleRegistry::default(),
            theme_colors: HashMap::new(),
            default_text_style: Default::default(),
        };

        handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: HashMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn hit_shape_id(h: u64, slide_idx: u32, x: f32, y: f32) -> Option<u32> {
        let ptr = roe_impress_hit_test(h, slide_idx, x, y);
        assert!(!ptr.is_null());
        let s = unsafe { std::ffi::CString::from_raw(ptr) }.into_string().unwrap();
        if s.contains("null") { None } else {
            s.split(':').nth(1).unwrap().trim_end_matches('}').parse().ok()
        }
    }

    #[test]
    fn test_hit_test_center_of_rect_hits() {
        let h = make_handle_with_shapes(vec![
            make_shape_geo(1, 0.0, 0.0, 100.0, 100.0, ShapeGeometry::Rect, None),
        ]);
        assert_eq!(hit_shape_id(h, 0, 50.0, 50.0), Some(1));
    }

    #[test]
    fn test_hit_test_outside_rect_misses() {
        let h = make_handle_with_shapes(vec![
            make_shape_geo(1, 0.0, 0.0, 100.0, 100.0, ShapeGeometry::Rect, None),
        ]);
        assert_eq!(hit_shape_id(h, 0, 150.0, 150.0), None);
    }

    #[test]
    fn test_hit_test_overlapping_shapes_returns_topmost() {
        // shape 1 和 2 完全重叠，2 在数组末尾（更靠上层），应命中 2。
        let h = make_handle_with_shapes(vec![
            make_shape_geo(1, 0.0, 0.0, 100.0, 100.0, ShapeGeometry::Rect, None),
            make_shape_geo(2, 0.0, 0.0, 100.0, 100.0, ShapeGeometry::Rect, None),
        ]);
        assert_eq!(hit_shape_id(h, 0, 50.0, 50.0), Some(2));
    }

    #[test]
    fn test_hit_test_ellipse_corner_misses_center_hits() {
        // 100x100 外接矩形的椭圆，角落 (5,5) 在矩形内但在椭圆外
        let h = make_handle_with_shapes(vec![
            make_shape_geo(1, 0.0, 0.0, 100.0, 100.0, ShapeGeometry::Ellipse, None),
        ]);
        assert_eq!(hit_shape_id(h, 0, 50.0, 50.0), Some(1), "椭圆中心应命中");
        assert_eq!(hit_shape_id(h, 0, 2.0, 2.0), None, "椭圆角落应不命中（矩形内椭圆外）");
    }

    #[test]
    fn test_hit_test_rotated_rect_precise() {
        // 100x100 矩形，中心在(50,50)，旋转45度。旋转前角落(0,0)在矩形内，
        // 旋转45度后该点应该落在形状外接矩形之外的某个位置——
        // 验证旋转判定确实生效（不是简单退化为未旋转AABB判定）。
        // 用中心点验证肯定命中（旋转不影响中心）：
        let h = make_handle_with_shapes(vec![
            make_shape_geo(1, 0.0, 0.0, 100.0, 100.0, ShapeGeometry::Rect, Some(45.0)),
        ]);
        assert_eq!(hit_shape_id(h, 0, 50.0, 50.0), Some(1), "旋转形状中心应始终命中");
        // 未旋转时角落(5,5)应该命中矩形；旋转45度后，(5,5)相对中心的
        // 偏移逆向旋转后会落在更远的对角线方向，超出半边长，应不命中。
        assert_eq!(hit_shape_id(h, 0, 5.0, 5.0), None, "旋转45度后原角落位置应不再命中");
    }

    #[test]
    fn test_hit_test_no_shapes_returns_none() {
        let h = make_handle_with_shapes(vec![]);
        assert_eq!(hit_shape_id(h, 0, 50.0, 50.0), None);
    }

    #[test]
    fn test_hit_test_invalid_slide_idx_returns_null() {
        let h = make_handle_with_shapes(vec![
            make_shape_geo(1, 0.0, 0.0, 100.0, 100.0, ShapeGeometry::Rect, None),
        ]);
        assert!(roe_impress_hit_test(h, 99, 50.0, 50.0).is_null());
    }

    #[test]
    fn test_hit_test_invalid_handle_returns_null() {
        assert!(roe_impress_hit_test(999_999, 0, 50.0, 50.0).is_null());
    }

    #[test]
    fn test_hit_test_custom_geometry_falls_back_to_bbox() {
        // Custom 几何应退化为外接矩形判定（已知局限，见函数文档）。
        let h = make_handle_with_shapes(vec![
            make_shape_geo(1, 0.0, 0.0, 100.0, 100.0, ShapeGeometry::Custom("star".into()), None),
        ]);
        assert_eq!(hit_shape_id(h, 0, 2.0, 2.0), Some(1), "Custom几何角落应按矩形判定命中");
    }
}


#[cfg(test)]
mod tests_reflow {
    use super::*;
    use roe_model::common::{PathCommand, PresetPathData};

    fn max_xy(cmds: &[PathCommand]) -> (f32, f32) {
        let (mut mx, mut my) = (0.0f32, 0.0f32);
        for c in cmds {
            match *c {
                PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => { mx = mx.max(x); my = my.max(y); }
                _ => {}
            }
        }
        (mx, my)
    }

    #[test]
    fn preset_rect_reevaluated_to_new_size() {
        let f = find_preset_shape("rect").unwrap();
        let cmds = roe_model::preset_geometry::eval_preset_shape(f, 100.0, 50.0, &std::collections::HashMap::new());
        let mut d = PresetPathData { commands: cmds, preset_name: "rect".to_string(), adj_overrides: Vec::new() };
        reflow_path_data(&mut d, 100.0, 50.0, 200.0, 80.0);
        let (mx, my) = max_xy(&d.commands);
        assert!((mx - 200.0).abs() < 0.01 && (my - 80.0).abs() < 0.01, "got {mx},{my}");
    }

    #[test]
    fn custom_path_scaled_by_ratio() {
        let cmds = vec![
            PathCommand::MoveTo { x: 0.0, y: 0.0 },
            PathCommand::LineTo { x: 100.0, y: 0.0 },
            PathCommand::LineTo { x: 100.0, y: 50.0 },
            PathCommand::LineTo { x: 0.0, y: 50.0 },
            PathCommand::Close,
        ];
        let mut d = PresetPathData { commands: cmds, preset_name: String::new(), adj_overrides: Vec::new() };
        reflow_path_data(&mut d, 100.0, 50.0, 200.0, 80.0);
        let (mx, my) = max_xy(&d.commands);
        assert!((mx - 200.0).abs() < 0.01 && (my - 80.0).abs() < 0.01, "got {mx},{my}");
    }
}


// ───────────────────────────────────────────────────────────────
// 插入 / 删除形状（Impress「插入」功能后端）
// ───────────────────────────────────────────────────────────────

/// 在指定幻灯片末尾（最上层）插入一个形状，返回新形状 id；失败返回 -1。
/// geometry_name: "rect" / "ellipse" / "line"，其余按 OOXML 预设名（如 "star5"、
/// "roundRect"、"rightArrow"、"straightConnector1"）查表生成路径，查不到退回矩形。
/// 单位 pt。普通形状写主题蓝填充 + 1pt 深蓝描边；线类（line / *Connector*）无填充，
/// 只写 1.5pt 深蓝描边（渲染器对无描边形状不画线，所以必须显式写）。
/// 线的两端点请随后用 roe_impress_set_line_endpoints 写入。
#[no_mangle]
pub extern "C" fn roe_impress_insert_shape(
    handle: u64,
    slide_idx: u32,
    geometry_name: *const c_char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> i32 {
    if geometry_name.is_null() {
        set_last_error("roe_impress_insert_shape: geometry 为空指针".to_string());
        return -1;
    }
    let name = match unsafe { std::ffi::CStr::from_ptr(geometry_name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => {
            set_last_error("roe_impress_insert_shape: geometry 不是合法 UTF-8".to_string());
            return -1;
        }
    };
    // OnlyOffice 菜单里的"带箭头线"名：映射成基础形状 + 箭头
    let (name, want_head, want_tail) = match name.as_str() {
        "lineWithArrow" => ("line".to_string(), false, true),
        "lineWithTwoArrows" => ("line".to_string(), true, true),
        "bentConnector5WithArrow" => ("bentConnector5".to_string(), false, true),
        "bentConnector5WithTwoArrows" => ("bentConnector5".to_string(), true, true),
        "curvedConnector3WithArrow" => ("curvedConnector3".to_string(), false, true),
        "curvedConnector3WithTwoArrows" => ("curvedConnector3".to_string(), true, true),
        _ => (name.clone(), false, false),
    };
    let is_line_kind = name == "line"
        || name.starts_with("straightConnector")
        || name.starts_with("bentConnector")
        || name.starts_with("curvedConnector");

    let geometry = match name.as_str() {
        "rect" => ShapeGeometry::Rect,
        "ellipse" => ShapeGeometry::Ellipse,
        "line" => ShapeGeometry::Line,
        other => match find_preset_shape(other) {
            Some(formula) => {
                let empty: HashMap<&str, f64> = HashMap::new();
                let path = roe_model::preset_geometry::eval_preset_shape(formula, w as f64, h as f64, &empty);
                ShapeGeometry::PresetPath(roe_model::common::PresetPathData {
                    commands: path,
                    preset_name: other.to_string(),
                    adj_overrides: Vec::new(),
                })
            }
            None => ShapeGeometry::Rect,
        },
    };

    let stroke_color = roe_model::common::Color { r: 0x38, g: 0x5D, b: 0x8A, a: 255 };
    let style = if is_line_kind {
        roe_model::common::ShapeStyle {
            fill: Some(roe_model::common::FillStyle::None),
            stroke: Some(roe_model::common::StrokeStyle { color: stroke_color, width: 1.5 }),
            head_arrow: if want_head { Some(roe_model::common::ArrowType::Triangle) } else { None },
            tail_arrow: if want_tail { Some(roe_model::common::ArrowType::Triangle) } else { None },
            ..Default::default()
        }
    } else {
        roe_model::common::ShapeStyle {
            fill: Some(roe_model::common::FillStyle::Solid(roe_model::common::Color { r: 0x4F, g: 0x81, b: 0xBD, a: 255 })),
            stroke: Some(roe_model::common::StrokeStyle { color: stroke_color, width: 1.0 }),
            ..Default::default()
        }
    };

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let next_id = slide.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
        slide.shapes.push(Shape {
            id: next_id,
            geometry,
            position: roe_model::common::Transform::from_rect(x, y, w, h),
            style,
            content: roe_model::impress::ShapeContent::Text(vec![]),
            text_body: roe_model::impress::TextBodyProps {
                autofit: roe_model::impress::TextAutofit::NoAutofit,
                wrap: true,
                anchor: roe_model::impress::TextAnchor::Top,
                ins_l: None,
                ins_r: None,
                ins_t: None,
                ins_b: None,
                vert: roe_model::impress::TextVert::default(),
            },
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        Ok(next_id)
    });

    match result {
        None => {
            set_last_error("roe_impress_insert_shape: 无效句柄".to_string());
            -1
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_impress_insert_shape: {msg}"));
            -1
        }
        Some(Ok(id)) => id as i32,
    }
}

/// 按 shape_id 删除指定幻灯片里的形状。成功删除返回 true，找不到或失败返回 false。
#[no_mangle]
pub extern "C" fn roe_impress_delete_shape(handle: u64, slide_idx: u32, shape_id: u32) -> bool {
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let before = slide.shapes.len();
        slide.shapes.retain(|sh| sh.id != shape_id);
        Ok(slide.shapes.len() < before)
    });
    match result {
        None => {
            set_last_error("roe_impress_delete_shape: 无效句柄".to_string());
            false
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_impress_delete_shape: {msg}"));
            false
        }
        Some(Ok(v)) => v,
    }
}


#[cfg(test)]
mod tests_insert_shape {
    use super::*;
    use crate::handle::{DocumentEntry, WriterOptions};
    use roe_model::impress::{Presentation, Slide};
    use std::collections::{HashMap as StdMap, VecDeque};
    use std::ffi::CString;

    fn make_handle() -> u64 {
        let pres = Presentation {
            styles: roe_model::style::StyleRegistry::default(),
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![],
                master_idx: None,
                animation_tree: Default::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        };
        handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: StdMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn count(h: u64) -> usize {
        handle::with_entry(h, |e| match &e.doc {
            RoeDocument::Impress(p) => p.slides[0].shapes.len(),
            _ => 0,
        }).unwrap()
    }

    #[test]
    fn insert_then_delete() {
        let h = make_handle();
        let rect = CString::new("rect").unwrap();
        let star = CString::new("star5").unwrap();
        let line = CString::new("line").unwrap();
        let a = roe_impress_insert_shape(h, 0, rect.as_ptr(), 10.0, 10.0, 100.0, 60.0);
        let b = roe_impress_insert_shape(h, 0, star.as_ptr(), 20.0, 20.0, 80.0, 80.0);
        let c = roe_impress_insert_shape(h, 0, line.as_ptr(), 0.0, 0.0, 50.0, 50.0);
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(count(h), 3);
        assert!(roe_impress_delete_shape(h, 0, 2));
        assert!(!roe_impress_delete_shape(h, 0, 2));
        assert_eq!(count(h), 2);
        assert_eq!(roe_impress_insert_shape(h, 9, rect.as_ptr(), 0.0, 0.0, 1.0, 1.0), -1);
    }
}


/// 形状缩略图路径：按 OOXML 预设名求路径，返回 SVG 路径字符串。
/// 所有点先算包围盒，再等比缩放并居中到 w x h 框内（不同公式的坐标系大小不一，必须归一化）。
/// 圆弧按每 10° 采样成折线（角度单位 1/60000 度）。带箭头的线名取基础形状。
/// 查不到返回 null。调用方需用 roe_free_string 释放返回值。
#[no_mangle]
pub extern "C" fn roe_impress_shape_preview_path(prst: *const c_char, w: f32, h: f32) -> *mut c_char {
    use std::fmt::Write;
    use roe_model::common::PathCommand as PC;
    if prst.is_null() { return std::ptr::null_mut(); }
    let name = match unsafe { std::ffi::CStr::from_ptr(prst) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    let base = match name {
        "lineWithArrow" | "lineWithTwoArrows" => "line",
        "bentConnector5WithArrow" | "bentConnector5WithTwoArrows" => "bentConnector5",
        "curvedConnector3WithArrow" | "curvedConnector3WithTwoArrows" => "curvedConnector3",
        o => o,
    };
    let formula = match find_preset_shape(base) {
        Some(f) => f,
        None => return std::ptr::null_mut(),
    };
    let empty: HashMap<&str, f64> = HashMap::new();
    let cmds = roe_model::preset_geometry::eval_preset_shape(formula, w as f64, h as f64, &empty);

    // 第一遍：展开成 (指令字母, 坐标列表)
    let mut ops: Vec<(char, Vec<f64>)> = Vec::new();
    let mut started = false;
    for c in &cmds {
        match c {
            PC::MoveTo { x, y } => { ops.push(('M', vec![*x as f64, *y as f64])); started = true; }
            PC::LineTo { x, y } => { ops.push(('L', vec![*x as f64, *y as f64])); }
            PC::CubicTo { x1, y1, x2, y2, x, y } => {
                ops.push(('C', vec![*x1 as f64, *y1 as f64, *x2 as f64, *y2 as f64, *x as f64, *y as f64]));
            }
            PC::QuadTo { x1, y1, x, y } => { ops.push(('Q', vec![*x1 as f64, *y1 as f64, *x as f64, *y as f64])); }
            PC::ArcTo { cx, cy, rx, ry, start_deg, sweep_deg } => {
                let s = *start_deg as f64 / 60000.0;
                let sw = *sweep_deg as f64 / 60000.0;
                let n = ((sw.abs() / 10.0).ceil() as usize).max(2);
                for i in 0..=n {
                    let a = (s + sw * i as f64 / n as f64).to_radians();
                    let px = *cx as f64 + *rx as f64 * a.cos();
                    let py = *cy as f64 + *ry as f64 * a.sin();
                    ops.push((if started { 'L' } else { 'M' }, vec![px, py]));
                    started = true;
                }
            }
            PC::Close => ops.push(('Z', vec![])),
        }
    }

    // 包围盒
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (_, v) in &ops {
        for p in v.chunks(2) {
            minx = minx.min(p[0]); maxx = maxx.max(p[0]);
            miny = miny.min(p[1]); maxy = maxy.max(p[1]);
        }
    }
    if minx > maxx || miny > maxy { return std::ptr::null_mut(); }
    let (bw, bh) = (maxx - minx, maxy - miny);
    let (wf, hf) = (w as f64, h as f64);
    let sx = if bw > 1e-6 { wf / bw } else { f64::INFINITY };
    let sy = if bh > 1e-6 { hf / bh } else { f64::INFINITY };
    let mut s = sx.min(sy);
    if !s.is_finite() { s = 1.0; }
    let ox = (wf - bw * s) / 2.0 - minx * s;
    let oy = (hf - bh * s) / 2.0 - miny * s;

    let mut out = String::new();
    for (c, v) in &ops {
        out.push(*c);
        out.push(' ');
        for p in v.chunks(2) {
            let _ = write!(out, "{:.1} {:.1} ", p[0] * s + ox, p[1] * s + oy);
        }
    }
    json_to_cstring(out, "roe_impress_shape_preview_path")
}


/// 读取 PNG/JPEG 图片的原始像素宽高。成功返回 true 并写入 out_w/out_h；
/// 失败返回 false（错误信息见 last_error）。
#[no_mangle]
pub extern "C" fn roe_image_dimensions(
    bytes: *const u8,
    len: usize,
    out_w: *mut u32,
    out_h: *mut u32,
) -> bool {
    if bytes.is_null() || len == 0 || out_w.is_null() || out_h.is_null() {
        set_last_error("roe_image_dimensions: 参数为空".to_string());
        return false;
    }
    let data = unsafe { std::slice::from_raw_parts(bytes, len) };
    let reader = match image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format() {
        Ok(r) => r,
        Err(e) => {
            set_last_error(format!("roe_image_dimensions: 无法识别格式: {e}"));
            return false;
        }
    };
    match reader.into_dimensions() {
        Ok((w, h)) => {
            unsafe {
                *out_w = w;
                *out_h = h;
            }
            true
        }
        Err(e) => {
            set_last_error(format!("roe_image_dimensions: 读取尺寸失败: {e}"));
            false
        }
    }
}

/// 在指定幻灯片插入一张图片（无填充、无描边的 Image 形状）。
/// bytes/len 为图片文件原始字节；mime 如 "image/png"，传空指针则按字节头猜测。
/// 返回新形状 id，失败返回 -1。
#[no_mangle]
pub extern "C" fn roe_impress_insert_image(
    handle: u64,
    slide_idx: u32,
    bytes: *const u8,
    len: usize,
    mime: *const c_char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> i32 {
    if bytes.is_null() || len == 0 {
        set_last_error("roe_impress_insert_image: 图片数据为空".to_string());
        return -1;
    }
    let data: Vec<u8> = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
    let mime_str = if mime.is_null() {
        if data.starts_with(&[0x89, b'P', b'N', b'G']) {
            "image/png".to_string()
        } else if data.starts_with(&[0xFF, 0xD8]) {
            "image/jpeg".to_string()
        } else {
            set_last_error("roe_impress_insert_image: 无法识别图片格式".to_string());
            return -1;
        }
    } else {
        match unsafe { std::ffi::CStr::from_ptr(mime) }.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                set_last_error("roe_impress_insert_image: mime 不是合法 UTF-8".to_string());
                return -1;
            }
        }
    };

    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let next_id = slide.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
        slide.shapes.push(Shape {
            id: next_id,
            geometry: ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x, y, w, h),
            style: roe_model::common::ShapeStyle {
                fill: None,
                stroke: None,
                ..Default::default()
            },
            content: roe_model::impress::ShapeContent::Image(roe_model::common::ImageData {
                data,
                mime: mime_str,
                crop_l: 0.0,
                crop_t: 0.0,
                crop_r: 0.0,
                crop_b: 0.0,
            }),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        Ok(next_id)
    });

    match result {
        None => {
            set_last_error("roe_impress_insert_image: 无效句柄".to_string());
            -1
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_impress_insert_image: {msg}"));
            -1
        }
        Some(Ok(id)) => id as i32,
    }
}


#[cfg(test)]
mod tests_insert_image {
    use super::*;
    use crate::handle::{DocumentEntry, WriterOptions};
    use roe_model::impress::{Presentation, ShapeContent, Slide};
    use std::collections::{HashMap as StdMap, VecDeque};

    fn make_handle() -> u64 {
        let pres = Presentation {
            styles: roe_model::style::StyleRegistry::default(),
            slide_size: (960.0, 540.0),
            slides: vec![Slide { layout_idx: None,
                shapes: vec![],
                master_idx: None,
                animation_tree: Default::default(),
                transition: None,
                notes: None,
                background: None,
                layout_shapes: vec![],
            }],
            master_slides: vec![],
            theme_colors: Default::default(),
            default_text_style: Default::default(),
        };
        handle::insert(DocumentEntry {
            doc: RoeDocument::Impress(pres),
            snapshot: None,
            is_shadow: false,
            events: VecDeque::new(),
            animation: StdMap::new(),
            writer_layout_cache: None,
            writer_font_registry: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            writer_options: WriterOptions::default(),
            impress_font_registry: None,
            writer_parley_font_context: None,
            calc_engine: None,
            pending_comment_hits: Vec::new(),
        })
    }

    fn png_4x2() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 2, image::Rgba([200, 30, 30, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("encode png");
        buf
    }

    #[test]
    fn dimensions_reads_png() {
        let png = png_4x2();
        let (mut w, mut h) = (0u32, 0u32);
        assert!(roe_image_dimensions(png.as_ptr(), png.len(), &mut w, &mut h));
        assert_eq!((w, h), (4, 2));
    }

    #[test]
    fn dimensions_rejects_garbage() {
        let junk = [1u8, 2, 3, 4, 5];
        let (mut w, mut h) = (0u32, 0u32);
        assert!(!roe_image_dimensions(junk.as_ptr(), junk.len(), &mut w, &mut h));
    }

    #[test]
    fn insert_image_adds_image_shape() {
        let h = make_handle();
        let png = png_4x2();
        let id1 = roe_impress_insert_image(h, 0, png.as_ptr(), png.len(), std::ptr::null(), 10.0, 20.0, 200.0, 100.0);
        let id2 = roe_impress_insert_image(h, 0, png.as_ptr(), png.len(), std::ptr::null(), 0.0, 0.0, 50.0, 25.0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        handle::with_entry(h, |e| {
            let RoeDocument::Impress(p) = &e.doc else { panic!("not impress") };
            let sh = &p.slides[0].shapes[0];
            assert!(matches!(&sh.content, ShapeContent::Image(img) if img.mime == "image/png" && !img.data.is_empty()));
            assert!(sh.style.fill.is_none());
            assert!(sh.style.stroke.is_none());
        })
        .expect("entry");
    }

    #[test]
    fn insert_image_rejects_empty_and_bad_slide() {
        let h = make_handle();
        let png = png_4x2();
        assert_eq!(roe_impress_insert_image(h, 0, std::ptr::null(), 0, std::ptr::null(), 0.0, 0.0, 10.0, 10.0), -1);
        assert_eq!(roe_impress_insert_image(h, 9, png.as_ptr(), png.len(), std::ptr::null(), 0.0, 0.0, 10.0, 10.0), -1);
    }
}

// ───────── v318o: 形状样式 / 幻灯片背景 ─────────
pub(crate) fn parse_hex_rgb_v318o(s: &str) -> Option<roe_model::common::Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(roe_model::common::Color { r, g, b, a: 255 })
}

fn cstr_or_empty_v318o(p: *const c_char) -> String {
    if p.is_null() { return String::new(); }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().trim().to_string()
}

/// 改形状样式。fill/stroke: ""不改 "none"清除 "#rrggbb"设置；stroke_w<0 不改。
#[no_mangle]
pub extern "C" fn roe_impress_set_shape_style(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    fill: *const c_char,
    stroke: *const c_char,
    stroke_w: f32,
) -> bool {
    let fill = cstr_or_empty_v318o(fill);
    let stroke = cstr_or_empty_v318o(stroke);
    with_shape_mut(handle, slide_idx, shape_id, "roe_impress_set_shape_style", |shape| {
        if fill == "none" {
            shape.style.fill = None;
        } else if !fill.is_empty() {
            let c = parse_hex_rgb_v318o(&fill).ok_or_else(|| format!("非法填充颜色 {fill}"))?;
            shape.style.fill = Some(roe_model::common::FillStyle::Solid(c));
        }
        if stroke == "none" {
            shape.style.stroke = None;
        } else if !stroke.is_empty() {
            let c = parse_hex_rgb_v318o(&stroke).ok_or_else(|| format!("非法边框颜色 {stroke}"))?;
            let w = shape.style.stroke.as_ref().map(|x| x.width).unwrap_or(1.0);
            shape.style.stroke = Some(roe_model::common::StrokeStyle { color: c, width: w });
        }
        if stroke_w >= 0.0 {
            match shape.style.stroke.as_mut() {
                Some(st) => st.width = stroke_w,
                None => {
                    shape.style.stroke = Some(roe_model::common::StrokeStyle {
                        color: roe_model::common::Color { r: 0x2b, g: 0x57, b: 0x9a, a: 255 },
                        width: stroke_w,
                    });
                }
            }
        }
        Ok(())
    }).is_some()
}

/// v318p: 对一组段落的所有 Run 应用文字样式命令（纯函数，便于单测）。
/// 命令：bold/italic/underline/strike 取 1|0|toggle；size 取 24 或 +2/-2；
/// color 取 #rrggbb；font 取字体名；clear 清除内联格式。
fn apply_text_style_v318p(paras: &mut [roe_model::writer::Paragraph], cmd: &str) -> Result<(), String> {
    let runs: Vec<&mut roe_model::writer::Run> =
        paras.iter_mut().flat_map(|p| p.runs.iter_mut()).collect();
    apply_style_to_runs_v318s(runs, cmd)
}

fn apply_style_to_runs_v318s(mut runs: Vec<&mut roe_model::writer::Run>, cmd: &str) -> Result<(), String> {
    let (k, v) = cmd.split_once(':').ok_or_else(|| format!("非法文字样式命令 {cmd}"))?;
    let v = v.trim();
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
                _ => !runs.iter().all(|r| get(&**r) == Some(true)),
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
                _ => return Err(format!("bad script {v}")),
            };
            let all = runs.iter().all(|r| r.script == Some(want));
            for r in runs.iter_mut() { r.script = if all { None } else { Some(want) }; }
        }
        "size" => {
            let relative = v.starts_with('+') || v.starts_with('-');
            let n: f32 = v.parse().map_err(|_| format!("非法字号 {v}"))?;
            for r in runs.iter_mut() {
                let cur = r.font_size.unwrap_or(18.0);
                let next = if relative { cur + n } else { n };
                r.font_size = Some(next.clamp(1.0, 400.0));
            }
        }
        "color" if v == "auto" => {
            for r in runs.iter_mut() { r.color = None; }
        }
        "background" => {
            if v == "none" {
                for r in runs.iter_mut() { r.background = None; }
            } else {
                let c = parse_hex_rgb_v318o(v).ok_or_else(|| format!("非法高亮颜色 {v}"))?;
                for r in runs.iter_mut() { r.background = Some(c); }
            }
        }
        "color" => {
            let c = parse_hex_rgb_v318o(v).ok_or_else(|| format!("非法文字颜色 {v}"))?;
            for r in runs.iter_mut() { r.color = Some(c); }
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
        _ => return Err(format!("未知文字样式 {k}")),
    }
    Ok(())
}

/// v318p: 改形状内全部文字的样式，cmd 形如 "bold:toggle" "size:+2" "color:#ff0000"。
#[no_mangle]
pub extern "C" fn roe_impress_set_text_style(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    cmd: *const c_char,
) -> bool {
    let cmd = cstr_or_empty_v318o(cmd);
    with_shape_mut(handle, slide_idx, shape_id, "roe_impress_set_text_style", |shape| {
        match &mut shape.content {
            roe_model::impress::ShapeContent::Text(ps) => apply_text_style_v318p(ps, &cmd),
            _ => Err("该形状没有文字".to_string()),
        }
    }).is_some()
}

/// v318s: 在段落内按 UTF-8 字节偏移把 run 拆成两段（offset 落在 run 内部时才拆）。
fn split_run_at_v318s(p: &mut roe_model::writer::Paragraph, offset: usize) {
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

/// v318s: 只改 [s, e) 范围内文字的样式；(段落号, 字节偏移)。
fn apply_text_style_range_v318s(
    paras: &mut [roe_model::writer::Paragraph],
    s: (usize, usize),
    e: (usize, usize),
    cmd: &str,
) -> Result<(), String> {
    let (s, e) = if s <= e { (s, e) } else { (e, s) };
    if s == e { return Err("选区为空".to_string()); }
    if let Some(p) = paras.get_mut(s.0) { split_run_at_v318s(p, s.1); }
    if let Some(p) = paras.get_mut(e.0) { split_run_at_v318s(p, e.1); }
    let mut sel: Vec<&mut roe_model::writer::Run> = Vec::new();
    for (pi, p) in paras.iter_mut().enumerate() {
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
    apply_style_to_runs_v318s(sel, cmd)
}

/// v318s: 只改选区文字样式。选区 (p1,o1)-(p2,o2)，偏移为段内 UTF-8 字节偏移。
#[no_mangle]
pub extern "C" fn roe_impress_set_text_style_range(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    p1: u32,
    o1: u32,
    p2: u32,
    o2: u32,
    cmd: *const c_char,
) -> bool {
    let cmd = cstr_or_empty_v318o(cmd);
    with_shape_mut(handle, slide_idx, shape_id, "roe_impress_set_text_style_range", |shape| {
        match &mut shape.content {
            roe_model::impress::ShapeContent::Text(ps) => apply_text_style_range_v318s(
                ps,
                (p1 as usize, o1 as usize),
                (p2 as usize, o2 as usize),
                &cmd,
            ),
            _ => Err("该形状没有文字".to_string()),
        }
    }).is_some()
}

#[cfg(test)]
mod text_style_tests_v318p {
    use super::*;
    use roe_model::writer::{Paragraph, Run};

    fn paras() -> Vec<Paragraph> {
        let mk = |t: &str| Run { text: t.to_string(), ..Default::default() };
        vec![
            Paragraph { runs: vec![mk("ab"), mk("cd")], ..Default::default() },
            Paragraph { runs: vec![mk("ef")], ..Default::default() },
        ]
    }

    #[test]
    fn bold_toggle_all_then_off() {
        let mut ps = paras();
        apply_text_style_v318p(&mut ps, "bold:toggle").unwrap();
        assert!(ps.iter().flat_map(|p| &p.runs).all(|r| r.bold == Some(true)));
        apply_text_style_v318p(&mut ps, "bold:toggle").unwrap();
        assert!(ps.iter().flat_map(|p| &p.runs).all(|r| r.bold == Some(false)));
    }

    #[test]
    fn bold_mixed_becomes_all_bold() {
        let mut ps = paras();
        ps[0].runs[0].bold = Some(true);
        apply_text_style_v318p(&mut ps, "bold:toggle").unwrap();
        assert!(ps.iter().flat_map(|p| &p.runs).all(|r| r.bold == Some(true)));
    }

    #[test]
    fn size_relative_and_absolute() {
        let mut ps = paras();
        apply_text_style_v318p(&mut ps, "size:+2").unwrap();
        assert_eq!(ps[0].runs[0].font_size, Some(20.0));
        apply_text_style_v318p(&mut ps, "size:30").unwrap();
        assert_eq!(ps[1].runs[0].font_size, Some(30.0));
    }

    #[test]
    fn color_font_clear_and_errors() {
        let mut ps = paras();
        apply_text_style_v318p(&mut ps, "color:#FF8000").unwrap();
        let c = ps[0].runs[1].color.unwrap();
        assert_eq!((c.r, c.g, c.b), (0xFF, 0x80, 0x00));
        apply_text_style_v318p(&mut ps, "font:微软雅黑").unwrap();
        assert_eq!(ps[1].runs[0].font_family.as_deref(), Some("微软雅黑"));
        apply_text_style_v318p(&mut ps, "clear:1").unwrap();
        assert!(ps[0].runs[0].color.is_none() && ps[0].runs[0].font_family.is_none());
        assert!(apply_text_style_v318p(&mut ps, "color:zzz").is_err());
        assert!(apply_text_style_v318p(&mut ps, "nope:1").is_err());
        let mut empty: Vec<Paragraph> = vec![Paragraph::default()];
        assert!(apply_text_style_v318p(&mut empty, "bold:1").is_err());
    }
}

/// 设置本页背景为图片（拉伸铺满）。mime 为空时按文件头识别 PNG/JPEG。
#[no_mangle]
pub extern "C" fn roe_impress_set_slide_background_image(
    handle: u64,
    slide_idx: u32,
    bytes: *const u8,
    len: usize,
    mime: *const c_char,
) -> bool {
    if bytes.is_null() || len == 0 {
        set_last_error("roe_impress_set_slide_background_image: 图片数据为空".to_string());
        return false;
    }
    let data: Vec<u8> = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
    let m = cstr_or_empty_v318o(mime);
    let mime_s = if !m.is_empty() {
        m
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png".to_string()
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg".to_string()
    } else {
        set_last_error("roe_impress_set_slide_background_image: 无法识别图片格式".to_string());
        return false;
    };
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres.slides.get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        slide.background = Some(roe_model::common::FillStyle::Picture(roe_model::common::ImageData {
            data: data.clone(),
            mime: mime_s.clone(),
            crop_l: 0.0, crop_t: 0.0, crop_r: 0.0, crop_b: 0.0,
        }));
        Ok(())
    });
    match result {
        None => { set_last_error("roe_impress_set_slide_background_image: 无效句柄".to_string()); false }
        Some(Err(msg)) => { set_last_error(format!("roe_impress_set_slide_background_image: {msg}")); false }
        Some(Ok(())) => true,
    }
}

/// 设置本页背景。color: "#rrggbb" 纯色；"none" 清除（回退到母版背景或白色）。
#[no_mangle]
pub extern "C" fn roe_impress_set_slide_background(handle: u64, slide_idx: u32, color: *const c_char) -> bool {
    let color = cstr_or_empty_v318o(color);
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres.slides.get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        if color == "none" {
            slide.background = None;
        } else if let Some(spec) = color.strip_prefix("grad:") {
            // v318p: grad:#起色,#止色,角度（0=从左到右，90=从上到下）
            let parts: Vec<&str> = spec.split(',').collect();
            if parts.len() != 3 {
                return Err(format!("非法渐变参数 {color}"));
            }
            let c1 = parse_hex_rgb_v318o(parts[0]).ok_or_else(|| format!("非法渐变起色 {color}"))?;
            let c2 = parse_hex_rgb_v318o(parts[1]).ok_or_else(|| format!("非法渐变止色 {color}"))?;
            let angle: f32 = parts[2].trim().parse().map_err(|_| format!("非法渐变角度 {color}"))?;
            slide.background = Some(roe_model::common::FillStyle::Gradient {
                stops: vec![
                    roe_model::common::GradientStop { pos: 0.0, color: c1 },
                    roe_model::common::GradientStop { pos: 1.0, color: c2 },
                ],
                kind: roe_model::common::GradientKind::Linear { angle },
                rotate_with_shape: true,
            });
        } else {
            let c = parse_hex_rgb_v318o(&color).ok_or_else(|| format!("非法背景颜色 {color}"))?;
            slide.background = Some(roe_model::common::FillStyle::Solid(c));
        }
        Ok(())
    });
    match result {
        None => { set_last_error("roe_impress_set_slide_background: 无效句柄".to_string()); false }
        Some(Err(msg)) => { set_last_error(format!("roe_impress_set_slide_background: {msg}")); false }
        Some(Ok(())) => true,
    }
}

/// v318r：在指定幻灯片插入 rows×cols 空表格（等宽列、等高行，默认表格样式带表头+镶边）。
/// x/y/w/h 单位 pt。返回新形状 id，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_insert_table(
    handle: u64,
    slide_idx: u32,
    rows: u32,
    cols: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> i32 {
    if rows == 0 || cols == 0 || rows > 100 || cols > 50 {
        set_last_error("roe_impress_insert_table: 行列数越界(1..=100 行, 1..=50 列)".to_string());
        return -1;
    }
    let (w, h) = (w.max(10.0), h.max(10.0));
    let result = handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let next_id = slide.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
        let table = build_empty_table_v318r(rows as usize, cols as usize, w, h);
        slide.shapes.push(Shape {
            id: next_id,
            geometry: ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x, y, w, h),
            style: roe_model::common::ShapeStyle {
                fill: None,
                stroke: None,
                ..Default::default()
            },
            content: roe_model::impress::ShapeContent::Table(table),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        Ok(next_id)
    });
    match result {
        None => {
            set_last_error("roe_impress_insert_table: 无效句柄".to_string());
            -1
        }
        Some(Err(msg)) => {
            set_last_error(format!("roe_impress_insert_table: {msg}"));
            -1
        }
        Some(Ok(id)) => id as i32,
    }
}

/// v318r：构造空表格数据（纯函数，便于单测）。
pub fn build_empty_table_v318r(rows: usize, cols: usize, w: f32, h: f32) -> roe_model::common::Table {
    use roe_model::common::{CellContent, Table, TableCell, TableRow};
    let col_w = w / cols as f32;
    let row_h = h / rows as f32;
    let mk_cell = || TableCell {
        content: vec![CellContent::Paragraph(Default::default())],
        col_span: 1,
        row_span: 1,
        style: None,
        hidden: false,
        bg_color: None,
        border_color: None,
        border_width: None,
        pad_left: None,
        pad_right: None,
        pad_top: None,
        pad_bottom: None,
    };
    let mut table = Table {
        rows: (0..rows)
            .map(|_| TableRow {
                cells: (0..cols).map(|_| mk_cell()).collect(),
                row_height: Some(row_h),
            })
            .collect(),
        col_widths: vec![col_w; cols],
        style_id: Some("{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}".to_string()),
        first_row: true,
        first_col: false,
        last_row: false,
        last_col: false,
        band_row: true,
        band_col: false,
        default_border_color: None,
        default_border_width: None,
        inside_h_color: None,
        inside_h_width: None,
        inside_v_color: None,
        inside_v_width: None,
        float_pos: None,
    };
    // 显式配色（不依赖主题色）：表头蓝，奇偶行浅蓝，白色 1pt 边框
    let white = roe_model::common::Color { r: 255, g: 255, b: 255, a: 255 };
    for (ri, row) in table.rows.iter_mut().enumerate() {
        let (r, g, b) = if ri == 0 { (0x5B, 0x9B, 0xD5) }
            else if ri % 2 == 1 { (0xD2, 0xDE, 0xEF) } else { (0xEA, 0xEF, 0xF7) };
        for cell in row.cells.iter_mut() {
            cell.bg_color = Some(roe_model::common::Color { r, g, b, a: 255 });
            cell.border_color = Some(white.clone());
            cell.border_width = Some(1.0);
        }
    }
    table
}

#[cfg(test)]
mod insert_table_tests_v318r {
    use super::*;

    #[test]
    fn empty_table_shape() {
        let t = build_empty_table_v318r(3, 4, 400.0, 90.0);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0].cells.len(), 4);
        assert_eq!(t.col_widths.len(), 4);
        assert!((t.col_widths[0] - 100.0).abs() < 0.01);
        assert!((t.rows[0].row_height.unwrap() - 30.0).abs() < 0.01);
        assert!(t.first_row && t.band_row);
        assert_eq!(t.rows[0].cells[0].bg_color.as_ref().unwrap().b, 0xD5);
        assert_eq!(t.rows[1].cells[0].bg_color.as_ref().unwrap().r, 0xD2);
    }
}

#[cfg(test)]
mod text_style_range_tests_v318s {
    use super::*;
    use roe_model::writer::{Paragraph, Run};

    fn para(t: &str) -> Paragraph {
        Paragraph { runs: vec![Run { text: t.to_string(), ..Default::default() }], ..Default::default() }
    }

    #[test]
    fn range_splits_runs() {
        let mut ps = vec![para("abcd")];
        apply_text_style_range_v318s(&mut ps, (0, 1), (0, 3), "bold:1").unwrap();
        let r = &ps[0].runs;
        assert_eq!(r.len(), 3);
        assert_eq!((r[0].text.as_str(), r[0].bold), ("a", None));
        assert_eq!((r[1].text.as_str(), r[1].bold), ("bc", Some(true)));
        assert_eq!((r[2].text.as_str(), r[2].bold), ("d", None));
    }

    #[test]
    fn range_multi_para_and_chinese() {
        let mut ps = vec![para("你好世界"), para("abc")];
        apply_text_style_range_v318s(&mut ps, (0, 3), (1, 1), "italic:1").unwrap();
        assert_eq!(ps[0].runs.len(), 2);
        assert_eq!(ps[0].runs[0].italic, None);
        assert_eq!(ps[0].runs[1].italic, Some(true));
        assert_eq!(ps[1].runs[0].text, "a");
        assert_eq!(ps[1].runs[0].italic, Some(true));
        assert_eq!(ps[1].runs[1].italic, None);
    }

    #[test]
    fn empty_range_errors() {
        let mut ps = vec![para("ab")];
        assert!(apply_text_style_range_v318s(&mut ps, (0, 1), (0, 1), "bold:1").is_err());
    }
}


#[cfg(test)]
mod table_resize_v318v {
    use super::*;

    #[test]
    fn table_resize_scales_cols_and_rows() {
        let t = build_empty_table_v318r(3, 4, 400.0, 90.0);
        let mut sh = roe_model::impress::Shape {
            id: 1,
            geometry: roe_model::impress::ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(10.0, 20.0, 400.0, 90.0),
            style: Default::default(),
            content: roe_model::impress::ShapeContent::Table(t),
            text_body: Default::default(),
            placeholder: None, st_cxn: None, end_cxn: None,
        };
        sh.position.extent = (200.0, 180.0);
        reflow_geometry(&mut sh, 400.0, 90.0);
        if let roe_model::impress::ShapeContent::Table(t) = &sh.content {
            assert!((t.col_widths[0] - 50.0).abs() < 0.01);
            assert!((t.rows[0].row_height.unwrap() - 60.0).abs() < 0.01);
            let sum: f32 = t.rows.iter().map(|r| r.row_height.unwrap()).sum();
            assert!((sum - 180.0).abs() < 0.01);
        } else { panic!("not table"); }
    }
}


/// v318x: 读取文字样式（用于功能区回显）。p1==u32::MAX 表示整个形状；
/// 起止相同表示光标（取光标前一个字符所在 run，段首取第一个 run）；否则按选区。
fn query_text_style_v318x(ps: &[roe_model::writer::Paragraph], p1: u32, o1: u32, p2: u32, o2: u32) -> String {
    let whole = p1 == u32::MAX;
    let (mut s, mut e) = ((p1 as usize, o1 as usize), (p2 as usize, o2 as usize));
    if !whole && s > e { std::mem::swap(&mut s, &mut e); }
    let caret = !whole && s == e;
    let mut runs: Vec<&roe_model::writer::Run> = Vec::new();
    for (pi, p) in ps.iter().enumerate() {
        if !whole && (pi < s.0 || pi > e.0) { continue; }
        let mut acc = 0usize;
        for r in p.runs.iter() {
            let len = r.text.len();
            let (rs, re) = (acc, acc + len);
            acc = re;
            if len == 0 { continue; }
            if whole { runs.push(r); continue; }
            if caret {
                let off = s.1;
                if (off > rs && off <= re) || (off == 0 && rs == 0) { runs.push(r); }
            } else {
                let from = if pi == s.0 { s.1 } else { 0 };
                let to = if pi == e.0 { e.1 } else { usize::MAX };
                if rs < to && re > from { runs.push(r); }
            }
        }
    }
    let fonts: Vec<Option<String>> = runs.iter()
        .map(|r| r.font_family.clone().or_else(|| r.font_family_ascii.clone()))
        .collect();
    let font_mixed = fonts.iter().any(|f| *f != fonts[0]);
    let font_json = match fonts.first() {
        Some(Some(n)) if !font_mixed => format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")),
        _ => "null".to_string(),
    };
    let sizes: Vec<f32> = runs.iter().map(|r| r.font_size.unwrap_or(18.0)).collect();
    let size0 = sizes.first().copied().unwrap_or(18.0);
    let size_mixed = sizes.iter().any(|v| (*v - size0).abs() > 0.01);
    format!("{{\"font\":{},\"font_mixed\":{},\"size\":{},\"size_mixed\":{}}}", font_json, font_mixed, size0, size_mixed)
}

/// v318x: 查询选区/光标/整个形状的字体名与字号，返回 JSON。
#[no_mangle]
pub extern "C" fn roe_impress_text_style_query(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    p1: u32,
    o1: u32,
    p2: u32,
    o2: u32,
) -> *mut c_char {
    let json = with_shape(handle, slide_idx, shape_id, "roe_impress_text_style_query", |shape| {
        match &shape.content {
            roe_model::impress::ShapeContent::Text(ps) => Ok(query_text_style_v318x(ps, p1, o1, p2, o2)),
            _ => Err("该形状没有文字".to_string()),
        }
    });
    match json {
        Some(j) => json_to_cstring(j, "roe_impress_text_style_query"),
        None => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod text_style_query_tests_v318x {
    use super::*;
    use roe_model::writer::{Paragraph, Run};
    fn run(t: &str, f: Option<&str>, s: Option<f32>) -> Run {
        Run { text: t.to_string(), font_family: f.map(|x| x.to_string()), font_size: s, ..Default::default() }
    }
    #[test]
    fn uniform_and_mixed() {
        let ps = vec![Paragraph {
            runs: vec![run("ab", Some("Liberation Serif"), Some(26.0)), run("cd", None, Some(18.0))],
            ..Default::default()
        }];
        let j = query_text_style_v318x(&ps, 0, 0, 0, 2);
        assert!(j.contains("\"font\":\"Liberation Serif\"") && j.contains("\"size\":26"), "{j}");
        let j = query_text_style_v318x(&ps, 0, 0, 0, 4);
        assert!(j.contains("\"font_mixed\":true") && j.contains("\"size_mixed\":true"), "{j}");
        let j = query_text_style_v318x(&ps, 0, 3, 0, 3);
        assert!(j.contains("\"font\":null"), "{j}");
        let j = query_text_style_v318x(&ps, u32::MAX, 0, 0, 0);
        assert!(j.contains("\"font_mixed\":true"), "{j}");
    }
}


/// v318y: 段落样式（纯函数）。p1==u32::MAX 表示全部段落，否则 [min(p1,p2), max(p1,p2)]。
/// 命令：align:left|center|right|justify
fn apply_para_style_v318y(ps: &mut [roe_model::writer::Paragraph], p1: u32, p2: u32, cmd: &str) -> Result<(), String> {
    let (k, v) = cmd.split_once(':').ok_or_else(|| format!("非法段落命令 {cmd}"))?;
    if ps.is_empty() { return Err("形状里没有段落".to_string()); }
    let (lo, hi) = if p1 == u32::MAX { (0usize, ps.len() - 1) } else { (p1.min(p2) as usize, p1.max(p2) as usize) };
    match k {
        "align" => {
            use roe_model::style::Alignment;
            let a = match v.trim() {
                "left" => Alignment::Left,
                "center" => Alignment::Center,
                "right" => Alignment::Right,
                "justify" => Alignment::Justify,
                other => return Err(format!("未知对齐 {other}")),
            };
            for (i, p) in ps.iter_mut().enumerate() {
                if i >= lo && i <= hi { p.alignment = Some(a.clone()); }
            }
            Ok(())
        }
        "bullet" => {
            use roe_model::impress::{BulletInfo, BulletKind};
            let first_on = ps.get(lo).and_then(|p| p.bullet.as_ref())
                .map(|b| matches!(b.kind, BulletKind::Char { .. } | BulletKind::AutoNum { .. }))
                .unwrap_or(false);
            let want_on = match v.trim() { "1" => true, "0" => false, _ => !first_on };
            for (i, p) in ps.iter_mut().enumerate() {
                if i < lo || i > hi { continue; }
                p.bullet_from_master = false;
                p.bullet = Some(if want_on {
                    BulletInfo { kind: BulletKind::Char { ch: "\u{2022}".to_string(), font: None }, mar_l: Some(27.0), indent: Some(-27.0), color: None }
                } else {
                    BulletInfo { kind: BulletKind::None, mar_l: None, indent: None, color: None }
                });
            }
            Ok(())
        }
        "indent" => {
            use roe_model::impress::{BulletInfo, BulletKind};
            let step: f32 = match v.trim() {
                "+1" | "1" => 27.0,
                "-1" => -27.0,
                other => return Err(format!("非法缩进 {other}")),
            };
            for (i, p) in ps.iter_mut().enumerate() {
                if i < lo || i > hi { continue; }
                let b = p.bullet.get_or_insert_with(|| BulletInfo { kind: BulletKind::None, mar_l: None, indent: None, color: None });
                let cur = b.mar_l.unwrap_or(0.0);
                let min_m = match (&b.kind, b.indent) { (BulletKind::None, _) => 0.0_f32, (_, Some(i)) if i < 0.0 => -i, _ => 0.0_f32 };
                b.mar_l = Some((cur + step).clamp(min_m.min(cur), 27.0 * 8.0));
                p.bullet_from_master = false;
            }
            Ok(())
        }
        "spacing" => {
            let val: f32 = if v.trim() == "cycle" {
                let cur = ps.get(lo).and_then(|p| p.line_height).unwrap_or(1.0);
                if cur < 1.25 { 1.5 } else if cur < 1.75 { 2.0 } else { 1.0 }
            } else {
                v.trim().parse::<f32>().map_err(|_| format!("非法行距 {v}"))?.clamp(0.5, 5.0)
            };
            for (i, p) in ps.iter_mut().enumerate() {
                if i >= lo && i <= hi { p.line_height = Some(val); }
            }
            Ok(())
        }
        "spcbef" | "spcaft" => {
            let before = k == "spcbef";
            let cur = ps.get(lo).and_then(|p| if before { p.space_before } else { p.space_after }).unwrap_or(0.0);
            let val: f32 = if v.trim() == "toggle" {
                if cur > 0.0 { 0.0 } else { 12.0 }
            } else {
                v.trim().parse::<f32>().map_err(|_| format!("bad spacing {v}"))?.clamp(0.0, 200.0)
            };
            for (i, p) in ps.iter_mut().enumerate() {
                if i >= lo && i <= hi {
                    if before { p.space_before = Some(val); } else { p.space_after = Some(val); }
                }
            }
            Ok(())
        }
        "bulletstyle" | "numstyle" => {
            use roe_model::impress::{BulletInfo, BulletKind};
            const CH: [&str; 6] = ["", "\u{2022}", "\u{25CB}", "\u{25A0}", "\u{2013}", "\u{2713}"];
            const SC: [&str; 7] = ["", "arabicPeriod", "arabicParenR", "alphaLcPeriod", "alphaUcPeriod", "romanLcPeriod", "romanUcPeriod"];
            let idx: usize = v.trim().parse().map_err(|_| format!("非法序号 {v}"))?;
            let kind = if idx == 0 {
                BulletKind::None
            } else if k == "bulletstyle" {
                if idx >= CH.len() { return Err(format!("非法符号序号 {idx}")); }
                BulletKind::Char { ch: CH[idx].to_string(), font: None }
            } else {
                if idx >= SC.len() { return Err(format!("非法编号序号 {idx}")); }
                BulletKind::AutoNum { scheme: SC[idx].to_string(), start_at: None }
            };
            let on = !matches!(kind, BulletKind::None);
            for (i, p) in ps.iter_mut().enumerate() {
                if i < lo || i > hi { continue; }
                let old = p.bullet.take();
                let was_on = old.as_ref().map_or(false, |b| !matches!(b.kind, BulletKind::None));
                let old_ind = old.as_ref().and_then(|b| b.indent);
                let old_mar = old.as_ref().and_then(|b| b.mar_l);
                let color = old.as_ref().and_then(|b| b.color);
                p.bullet_from_master = false;
                p.bullet = Some(if on {
                    {
                        let om = old_mar.unwrap_or(0.0);
                        let (m, ind) = if was_on && old_ind.map_or(false, |i| i < 0.0) { (om, old_ind.unwrap_or(-27.0)) } else if was_on { (om.max(27.0), -27.0) } else { (om + 27.0, -27.0) };
                        BulletInfo { kind: kind.clone(), mar_l: Some(m), indent: Some(ind), color }
                    }
                } else {
                    BulletInfo { kind: BulletKind::None, mar_l: if was_on { let hang = old_ind.map_or(0.0, |i| (-i).max(0.0)); Some((old_mar.unwrap_or(0.0) - hang).max(0.0)) } else { old_mar }, indent: None, color: None }
                });
            }
            Ok(())
        }
        _ => Err(format!("未知段落命令 {k}")),
    }
}

/// v318y: 设置段落样式。p1==u32::MAX 表示整个形状。
#[no_mangle]
pub extern "C" fn roe_impress_set_para_style(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    p1: u32,
    p2: u32,
    cmd: *const c_char,
) -> bool {
    let cmd = cstr_or_empty_v318o(cmd);
    with_shape_mut(handle, slide_idx, shape_id, "roe_impress_set_para_style", |shape| {
        match &mut shape.content {
            roe_model::impress::ShapeContent::Text(ps) => apply_para_style_v318y(ps, p1, p2, &cmd),
            _ => Err("该形状没有文字".to_string()),
        }
    }).is_some()
}

#[cfg(test)]
mod para_style_tests_v318y {
    use super::*;
    use roe_model::writer::Paragraph;
    #[test]
    fn align_range_and_all() {
        let mut ps = vec![Paragraph::default(), Paragraph::default(), Paragraph::default()];
        apply_para_style_v318y(&mut ps, 1, 1, "align:center").unwrap();
        assert!(ps[0].alignment.is_none());
        assert!(matches!(ps[1].alignment, Some(roe_model::style::Alignment::Center)));
        assert!(ps[2].alignment.is_none());
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "align:right").unwrap();
        assert!(ps.iter().all(|p| matches!(p.alignment, Some(roe_model::style::Alignment::Right))));
        apply_para_style_v318y(&mut ps, 2, 0, "align:justify").unwrap();
        assert!(ps.iter().all(|p| matches!(p.alignment, Some(roe_model::style::Alignment::Justify))));
        assert!(apply_para_style_v318y(&mut ps, 0, 0, "align:bogus").is_err());
    }
}


#[cfg(test)]
mod bullet_tests_v318z {
    use super::*;
    use roe_model::writer::Paragraph;
    use roe_model::impress::BulletKind;
    #[test]
    fn bullet_toggle() {
        let mut ps = vec![Paragraph::default(), Paragraph::default()];
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "bullet:toggle").unwrap();
        assert!(ps.iter().all(|p| matches!(p.bullet.as_ref().unwrap().kind, BulletKind::Char { .. })));
        assert_eq!(ps[0].bullet.as_ref().unwrap().mar_l, Some(27.0));
        // 只对第2段再点：该段原本有符号 -> 关闭；第1段不受影响
        apply_para_style_v318y(&mut ps, 1, 1, "bullet:toggle").unwrap();
        assert!(matches!(ps[0].bullet.as_ref().unwrap().kind, BulletKind::Char { .. }));
        assert!(matches!(ps[1].bullet.as_ref().unwrap().kind, BulletKind::None));
        // 全部段落：以第1段（有符号）为准 -> 全部关闭
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "bullet:toggle").unwrap();
        assert!(ps.iter().all(|p| matches!(p.bullet.as_ref().unwrap().kind, BulletKind::None)));
        apply_para_style_v318y(&mut ps, 0, 0, "bullet:1").unwrap();
        assert!(matches!(ps[0].bullet.as_ref().unwrap().kind, BulletKind::Char { .. }));
    }
}


#[cfg(test)]
mod indent_spacing_tests_v319d {
    use super::*;
    use roe_model::writer::Paragraph;
    #[test]
    fn indent_and_spacing() {
        let mut ps = vec![Paragraph::default(), Paragraph::default()];
        apply_para_style_v318y(&mut ps, 0, 0, "indent:+1").unwrap();
        apply_para_style_v318y(&mut ps, 0, 0, "indent:+1").unwrap();
        assert_eq!(ps[0].bullet.as_ref().unwrap().mar_l, Some(54.0));
        assert!(ps[1].bullet.is_none());
        apply_para_style_v318y(&mut ps, 0, 0, "indent:-1").unwrap();
        assert_eq!(ps[0].bullet.as_ref().unwrap().mar_l, Some(27.0));
        for _ in 0..3 { apply_para_style_v318y(&mut ps, 0, 0, "indent:-1").unwrap(); }
        assert_eq!(ps[0].bullet.as_ref().unwrap().mar_l, Some(0.0));
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "spacing:cycle").unwrap();
        assert_eq!(ps[1].line_height, Some(1.5));
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "spacing:cycle").unwrap();
        assert_eq!(ps[1].line_height, Some(2.0));
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "spacing:cycle").unwrap();
        assert_eq!(ps[1].line_height, Some(1.0));
    }
}


#[cfg(test)]
mod highlight_tests_v319e {
    use super::*;
    use roe_model::writer::{Paragraph, Run};
    #[test]
    fn background_and_auto_color() {
        let mut ps = vec![Paragraph { runs: vec![Run { text: "a".into(), ..Default::default() }], ..Default::default() }];
        apply_text_style_v318p(&mut ps, "background:#ffff00").unwrap();
        assert!(ps[0].runs[0].background.is_some());
        apply_text_style_v318p(&mut ps, "background:none").unwrap();
        assert!(ps[0].runs[0].background.is_none());
        apply_text_style_v318p(&mut ps, "color:#ff0000").unwrap();
        assert!(ps[0].runs[0].color.is_some());
        apply_text_style_v318p(&mut ps, "color:auto").unwrap();
        assert!(ps[0].runs[0].color.is_none());
    }
}


#[cfg(test)]
mod list_style_tests_v319g {
    use super::*;
    use roe_model::writer::Paragraph;
    use roe_model::impress::BulletKind;
    #[test]
    fn bullet_and_number_styles() {
        let mut ps = vec![Paragraph::default(), Paragraph::default()];
        apply_para_style_v318y(&mut ps, 0, 0, "bulletstyle:3").unwrap();
        assert!(matches!(&ps[0].bullet.as_ref().unwrap().kind, BulletKind::Char { ch, .. } if ch == "\u{25A0}"));
        assert!(ps[1].bullet.is_none());
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "numstyle:5").unwrap();
        assert!(matches!(&ps[1].bullet.as_ref().unwrap().kind, BulletKind::AutoNum { scheme, .. } if scheme == "romanLcPeriod"));
        assert_eq!(ps[1].bullet.as_ref().unwrap().mar_l, Some(27.0));
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "numstyle:0").unwrap();
        assert!(matches!(ps[0].bullet.as_ref().unwrap().kind, BulletKind::None));
        assert!(apply_para_style_v318y(&mut ps, 0, 0, "bulletstyle:9").is_err());
    }
}


#[cfg(test)]
mod spacing_tests_v319h {
    use super::*;
    use roe_model::writer::Paragraph;
    #[test]
    fn space_before_after() {
        let mut ps = vec![Paragraph::default(), Paragraph::default()];
        apply_para_style_v318y(&mut ps, 0, 0, "spcbef:toggle").unwrap();
        assert_eq!(ps[0].space_before, Some(12.0));
        assert_eq!(ps[1].space_before, None);
        apply_para_style_v318y(&mut ps, 0, 0, "spcbef:toggle").unwrap();
        assert_eq!(ps[0].space_before, Some(0.0));
        apply_para_style_v318y(&mut ps, u32::MAX, 0, "spcaft:6").unwrap();
        assert_eq!(ps[0].space_after, Some(6.0));
        assert_eq!(ps[1].space_after, Some(6.0));
        assert!(apply_para_style_v318y(&mut ps, 0, 0, "spcbef:abc").is_err());
    }
}

// v221 新增：Impress 图表插入/读取/整体替换（普通 ChartModel，17 种）。
// 统计类图表（直方/箱线/瀑布/漏斗/旭日/树状）放第二阶段。

use roe_parser::RoeDocument;

fn read_cstr_v221(p: *const std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_str().ok().map(|s| s.to_string())
}

fn parse_chart_model_v221(json: &str) -> Result<roe_model::impress::ChartModel, String> {
    serde_json::from_str::<roe_model::impress::ChartModel>(json)
        .map_err(|e| format!("图表 JSON 解析失败: {e}"))
}

/// 在指定幻灯片插入图表形状。chart_json 为 ChartModel 的 JSON：
/// 必填 kind / x_labels / series，其余字段缺省时用模型默认值。
/// kind 取值：Bar HorizontalBar Line Pie Scatter Radar Area Doughnut；
/// grouping 取值：Standard Stacked PercentStacked；系列 as_line=true 即柱线组合。
/// x/y/w/h 单位 pt。返回新形状 id，失败 -1（详情见 last_error）。
#[no_mangle]
pub extern "C" fn roe_impress_insert_chart(
    handle: u64,
    slide_idx: u32,
    chart_json: *const std::os::raw::c_char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> i32 {
    let json = match read_cstr_v221(chart_json) {
        Some(s) => s,
        None => {
            crate::error::set_last_error("roe_impress_insert_chart: chart_json 为空或不是 UTF-8".to_string());
            return -1;
        }
    };
    let model = match parse_chart_model_v221(&json) {
        Ok(m) => m,
        Err(e) => {
            crate::error::set_last_error(format!("roe_impress_insert_chart: {e}"));
            return -1;
        }
    };
    if !(w > 0.0 && h > 0.0) {
        crate::error::set_last_error("roe_impress_insert_chart: 宽高必须大于 0".to_string());
        return -1;
    }

    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let next_id = slide.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
        let mut m = model.clone();
        m.id = next_id as u64;
        m.x_pt = x;
        m.y_pt = y;
        m.width_pt = w;
        m.height_pt = h;
        slide.shapes.push(roe_model::impress::Shape {
            id: next_id,
            geometry: roe_model::impress::ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x, y, w, h),
            style: roe_model::common::ShapeStyle {
                fill: None,
                stroke: None,
                ..Default::default()
            },
            content: roe_model::impress::ShapeContent::Chart(m),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        Ok(next_id)
    });

    match result {
        None => {
            crate::error::set_last_error("roe_impress_insert_chart: 无效句柄".to_string());
            -1
        }
        Some(Err(msg)) => {
            crate::error::set_last_error(format!("roe_impress_insert_chart: {msg}"));
            -1
        }
        Some(Ok(id)) => id as i32,
    }
}

/// 读取图表形状的完整 ChartModel（JSON 字符串，调用方用现有的字符串释放函数释放）。
/// 编辑数据窗口用它取当前数据。非图表形状或失败返回 null。
#[no_mangle]
pub extern "C" fn roe_impress_get_chart_json(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
) -> *mut std::os::raw::c_char {
    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let shape = slide
            .shapes
            .iter()
            .find(|sh| sh.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        match &shape.content {
            roe_model::impress::ShapeContent::Chart(m) => {
                serde_json::to_string(m).map_err(|e| format!("序列化失败: {e}"))
            }
            _ => Err("该形状不是图表".to_string()),
        }
    });

    match result {
        None => {
            crate::error::set_last_error("roe_impress_get_chart_json: 无效句柄".to_string());
            std::ptr::null_mut()
        }
        Some(Err(msg)) => {
            crate::error::set_last_error(format!("roe_impress_get_chart_json: {msg}"));
            std::ptr::null_mut()
        }
        Some(Ok(s)) => match std::ffi::CString::new(s) {
            Ok(c) => c.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
    }
}

/// 用新的 ChartModel JSON 整体替换已有图表形状的内容（改类型、改数据、改样式都走这里）。
/// 保留原来的 id 与位置/尺寸。成功 true。
#[no_mangle]
pub extern "C" fn roe_impress_set_chart_json(
    handle: u64,
    slide_idx: u32,
    shape_id: u32,
    chart_json: *const std::os::raw::c_char,
) -> bool {
    let json = match read_cstr_v221(chart_json) {
        Some(s) => s,
        None => {
            crate::error::set_last_error("roe_impress_set_chart_json: chart_json 为空或不是 UTF-8".to_string());
            return false;
        }
    };
    let new_model = match parse_chart_model_v221(&json) {
        Ok(m) => m,
        Err(e) => {
            crate::error::set_last_error(format!("roe_impress_set_chart_json: {e}"));
            return false;
        }
    };

    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let shape = slide
            .shapes
            .iter_mut()
            .find(|sh| sh.id == shape_id)
            .ok_or_else(|| format!("shape_id={shape_id} 不存在"))?;
        match &mut shape.content {
            roe_model::impress::ShapeContent::Chart(old) => {
                let mut nm = new_model.clone();
                nm.id = old.id;
                nm.x_pt = old.x_pt;
                nm.y_pt = old.y_pt;
                nm.width_pt = old.width_pt;
                nm.height_pt = old.height_pt;
                *old = nm;
                Ok(())
            }
            _ => Err("该形状不是图表".to_string()),
        }
    });

    match result {
        None => {
            crate::error::set_last_error("roe_impress_set_chart_json: 无效句柄".to_string());
            false
        }
        Some(Err(msg)) => {
            crate::error::set_last_error(format!("roe_impress_set_chart_json: {msg}"));
            false
        }
        Some(Ok(())) => true,
    }
}

// v222: 插入统计类图表（直方/箱线/瀑布/漏斗/旭日/树状）。
// stat_json 为完整的 StatChartModel JSON（该模型无 serde 默认值，字段需写全）。
// x/y/w/h 单位 pt。返回新形状 id，失败 -1。
#[no_mangle]
pub extern "C" fn roe_impress_insert_stat_chart(
    handle: u64,
    slide_idx: u32,
    stat_json: *const std::os::raw::c_char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> i32 {
    let json = match read_cstr_v221(stat_json) {
        Some(s) => s,
        None => {
            crate::error::set_last_error("roe_impress_insert_stat_chart: json 为空或不是 UTF-8".to_string());
            return -1;
        }
    };
    let model = match serde_json::from_str::<roe_model::stat_chart::StatChartModel>(&json) {
        Ok(m) => m,
        Err(e) => {
            crate::error::set_last_error(format!("roe_impress_insert_stat_chart: JSON 解析失败: {e}"));
            return -1;
        }
    };
    if !(w > 0.0 && h > 0.0) {
        crate::error::set_last_error("roe_impress_insert_stat_chart: 宽高必须大于 0".to_string());
        return -1;
    }
    let result = crate::handle::with_entry(handle, |entry| {
        let RoeDocument::Impress(pres) = &mut entry.doc else {
            return Err("非 Impress 文档".to_string());
        };
        let slide = pres
            .slides
            .get_mut(slide_idx as usize)
            .ok_or_else(|| format!("slide_idx={slide_idx} 越界"))?;
        let next_id = slide.shapes.iter().map(|sh| sh.id).max().unwrap_or(0) + 1;
        slide.shapes.push(roe_model::impress::Shape {
            id: next_id,
            geometry: roe_model::impress::ShapeGeometry::Rect,
            position: roe_model::common::Transform::from_rect(x, y, w, h),
            style: roe_model::common::ShapeStyle {
                fill: None,
                stroke: None,
                ..Default::default()
            },
            content: roe_model::impress::ShapeContent::StatChart(model.clone()),
            text_body: Default::default(),
            placeholder: None,
            st_cxn: None,
            end_cxn: None,
        });
        Ok(next_id)
    });
    match result {
        None => {
            crate::error::set_last_error("roe_impress_insert_stat_chart: 无效句柄".to_string());
            -1
        }
        Some(Err(msg)) => {
            crate::error::set_last_error(format!("roe_impress_insert_stat_chart: {msg}"));
            -1
        }
        Some(Ok(id)) => id as i32,
    }
}

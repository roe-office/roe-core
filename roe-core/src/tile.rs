//! Tile 渲染基础设施
//! TileBitmap：内核输出，前端只负责贴图（QML/WASM 无感知）

pub use crate::tile_geometry::*;
pub use crate::tile_render_helpers::*;
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

pub struct CalcTileRenderer<'a> {
    pub workbook: &'a roe_model::calc::Workbook,
    pub config:   CalcRenderConfig,
    pub sheet_idx: usize,
    pub font_context: parley::FontContext,
    pub layout_context: parley::LayoutContext,
    pub scale_context: swash::scale::ScaleContext,
    /// ROOT-078 (eval_hook)：条件格式 expr 类型规则 / cfvo formula-num
    /// 阈值求值所需的公式引擎能力，由 roe-api 组装（`roe-calc::CachedEvaluator`）
    /// 后注入。`None` 时（如未通过 `with_formula_evaluator` 设置、或独立单元
    /// 测试场景）相关求值分支退化为不命中/字面量兜底，不 panic。
    pub formula_evaluator: Option<Box<dyn crate::formula_hook::FormulaEvaluator + 'a>>,
}

impl<'a> CalcTileRenderer<'a> {
    pub fn new(workbook: &'a roe_model::calc::Workbook) -> Self {
        Self {
            workbook,
            config: CalcRenderConfig::default(),
            sheet_idx: 0,
            font_context: parley::FontContext::new(),
            layout_context: parley::LayoutContext::new(),
            scale_context: swash::scale::ScaleContext::new(),
            formula_evaluator: None,
        }
    }

    /// ROOT-078 (eval_hook)：注入公式求值能力（构建者模式，链式调用）。
    /// 由 roe-api 在构造完 CalcTileRenderer 后立即调用，传入包装了
    /// `roe-calc::formula_engine::eval_ast_with_resolver_and_cell` 的
    /// `CachedEvaluator`。不调用则保持 `None`，相关条件格式求值分支
    /// 退化为不命中/字面量兜底，渲染主流程不受影响。
    pub fn with_formula_evaluator(mut self, evaluator: Box<dyn crate::formula_hook::FormulaEvaluator + 'a>) -> Self {
        self.formula_evaluator = Some(evaluator);
        self
    }

    /// 复用外部已建立的字体资源，避免每次渲染重新扫描系统字体（性能修复，v86任务A）。
    /// 调用方渲染完毕后应调用 take_fonts() 把资源取回，供下一帧复用。
    pub fn new_with_fonts(
        workbook: &'a roe_model::calc::Workbook,
        font_context: parley::FontContext,
        layout_context: parley::LayoutContext,
        scale_context: swash::scale::ScaleContext,
    ) -> Self {
        Self {
            workbook,
            config: CalcRenderConfig::default(),
            sheet_idx: 0,
            font_context,
            layout_context,
            scale_context,
            formula_evaluator: None,
        }
    }

    /// 取回字体资源（消耗 self 中的 font_system/swash_cache），供调用方存回缓存复用。
    pub fn take_fonts(self) -> (parley::FontContext, parley::LayoutContext, swash::scale::ScaleContext) {
        (self.font_context, self.layout_context, self.scale_context)
    }

}


// ROOT-005修复(v126)：从paint_tile内pivot分支的局部闭包提取为独立函数，
// 供render_sheet_preview（普通表格autoFilter按钮）和paint_tile（pivot字段按钮）
// 共用，避免逻辑重复维护。
fn draw_field_dropdown_btn(
    pixmap: &mut tiny_skia::PixmapMut,
    sheet: &roe_model::calc::Sheet,
    cfg: &CalcRenderConfig,
    tx: tiny_skia::Transform,
    sheet_col: u32,
    label_row: u32,
    is_filtered: bool,
) {
    let cw = col_width_at_px(sheet, sheet_col, cfg);
    let ch = row_height_at_px(sheet, label_row, cfg);
    let cx = col_x_px(sheet, sheet_col, cfg);
    let cy = row_y_px(sheet, label_row, cfg);

    // ROOT-051实验：已筛选状态用反色（绿底白图标）提升辨识度，
    // 未筛选保持原有白底灰色倒三角样式。
    let mut btn_paint = tiny_skia::Paint::default();
    if is_filtered {
        btn_paint.set_color_rgba8(0x21, 0x73, 0x46, 0xff);
    } else {
        btn_paint.set_color_rgba8(0xff, 0xff, 0xff, 0xff);
    }
    btn_paint.anti_alias = true;
    let mut tri_paint = tiny_skia::Paint::default();
    tri_paint.set_color_rgba8(0x40, 0x40, 0x40, 0xff);
    tri_paint.anti_alias = true;

    let btn_margin = 1.0_f32;
    let btn_size = (ch - btn_margin * 2.0).min(cw - btn_margin * 2.0).max(4.0_f32).min(16.0_f32);
    let bx = cx + cw - btn_size - btn_margin;
    let byy = cy + (ch - btn_size) / 2.0;

    if let Some(rect) = tiny_skia::Rect::from_xywh(bx, byy, btn_size, btn_size) {
        pixmap.fill_rect(rect, &btn_paint, tx, None);
    }

    let mut btn_border_paint = tiny_skia::Paint::default();
    btn_border_paint.set_color_rgba8(0x80, 0x80, 0x80, 0xff);
    if let Some(r) = tiny_skia::Rect::from_xywh(bx, byy, btn_size, 1.0) { pixmap.fill_rect(r, &btn_border_paint, tx, None); }
    if let Some(r) = tiny_skia::Rect::from_xywh(bx, byy + btn_size - 1.0, btn_size, 1.0) { pixmap.fill_rect(r, &btn_border_paint, tx, None); }
    if let Some(r) = tiny_skia::Rect::from_xywh(bx, byy, 1.0, btn_size) { pixmap.fill_rect(r, &btn_border_paint, tx, None); }
    if let Some(r) = tiny_skia::Rect::from_xywh(bx + btn_size - 1.0, byy, 1.0, btn_size) { pixmap.fill_rect(r, &btn_border_paint, tx, None); }

    if is_filtered {
        // ROOT-005补充(v126)：该列当前已应用筛选（col_filters非空），
        // 用简化版实心漏斗图标替代普通倒三角，与Excel"已筛选=漏斗"视觉
        // 语义一致，方便一眼区分"有下拉按钮"和"已实际筛选"两种状态。
        // 颜色用Excel经典绿(#217346)，与旁边深灰倒三角形成明显区分。
        // 形状：上宽梯形收窄，中间收成一条短竖线（漏斗颈部），整体实心填充。
        let mut funnel_paint = tiny_skia::Paint::default();
        funnel_paint.set_color_rgba8(0xff, 0xff, 0xff, 0xff);
        funnel_paint.anti_alias = true;

        let fw_top = 10.0_f32;    // 漏斗顶部宽度
        let fw_neck = 2.2_f32;   // 漏斗颈部宽度
        let f_body_h = 4.0_f32;  // 梯形（漏斗上半）高度
        let f_neck_h = 3.0_f32;  // 颈部（漏斗下半细杆）高度
        let f_total_h = f_body_h + f_neck_h;
        let fx0 = bx + (btn_size - fw_top) / 2.0;
        let fy0 = byy + (btn_size - f_total_h) / 2.0;
        let neck_x0 = bx + (btn_size - fw_neck) / 2.0;
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(fx0, fy0);
        pb.line_to(fx0 + fw_top, fy0);
        pb.line_to(neck_x0 + fw_neck, fy0 + f_body_h);
        pb.line_to(neck_x0 + fw_neck, fy0 + f_total_h);
        pb.line_to(neck_x0, fy0 + f_total_h);
        pb.line_to(neck_x0, fy0 + f_body_h);
        pb.close();
        if let Some(path) = pb.finish() {
            pixmap.fill_path(&path, &funnel_paint, tiny_skia::FillRule::Winding, tx, None);
        }
    } else {
        let tri_w = 8.0_f32;
        let tri_h = 5.0_f32;
        let tx0 = bx + (btn_size - tri_w) / 2.0;
        let ty0 = byy + (btn_size - tri_h) / 2.0;
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(tx0, ty0);
        pb.line_to(tx0 + tri_w, ty0);
        pb.line_to(tx0 + tri_w / 2.0, ty0 + tri_h);
        pb.close();
        if let Some(path) = pb.finish() {
            pixmap.fill_path(&path, &tri_paint, tiny_skia::FillRule::Winding, tx, None);
        }
    }
}

// 分类汇总折叠/展开按钮渲染。汇总行判定: 当前行row_outline_level严格小于
// 下一行(支持任意嵌套深度,不写死具体级数)。画在当前视口最左可见列
// (col_start)——insert_subtotals不持久化group_col,分组标签惯例上总在
// 最左列,与draw_field_dropdown_btn/draw_autofilter_buttons取col_start
// 做起点是同一模式。图标贴单元格左边界(bx = cx + margin),与筛选按钮贴
// 右边界故意相反,避免和汇总标签文字(左对齐)重叠。
fn draw_collapse_btn(
    pixmap: &mut tiny_skia::PixmapMut,
    sheet: &roe_model::calc::Sheet,
    cfg: &CalcRenderConfig,
    tx: tiny_skia::Transform,
    sheet_col: u32,
    label_row: u32,
    is_collapsed: bool,
) {
    let cw = col_width_at_px(sheet, sheet_col, cfg);
    let ch = row_height_at_px(sheet, label_row, cfg);
    let cx = col_x_px(sheet, sheet_col, cfg);
    let cy = row_y_px(sheet, label_row, cfg);

    // 折叠态(画+)用浅绿底白符号,呼应筛选按钮"已筛选=绿底"的视觉语言；
    // 展开态(画-)用白底深灰符号,与筛选按钮未筛选态一致。
    let mut btn_paint = tiny_skia::Paint::default();
    if is_collapsed {
        btn_paint.set_color_rgba8(0x21, 0x73, 0x46, 0xff);
    } else {
        btn_paint.set_color_rgba8(0xff, 0xff, 0xff, 0xff);
    }
    btn_paint.anti_alias = true;
    let mut sym_paint = tiny_skia::Paint::default();
    if is_collapsed {
        sym_paint.set_color_rgba8(0xff, 0xff, 0xff, 0xff);
    } else {
        sym_paint.set_color_rgba8(0x40, 0x40, 0x40, 0xff);
    }
    sym_paint.anti_alias = true;

    let btn_margin = 1.0_f32;
    let btn_size = (ch - btn_margin * 2.0).min(cw - btn_margin * 2.0).max(4.0_f32).min(16.0_f32);
    let bx = cx + cw - btn_size - btn_margin;
    let byy = cy + (ch - btn_size) / 2.0;

    if let Some(rect) = tiny_skia::Rect::from_xywh(bx, byy, btn_size, btn_size) {
        pixmap.fill_rect(rect, &btn_paint, tx, None);
    }

    let mut border_paint = tiny_skia::Paint::default();
    border_paint.set_color_rgba8(0x80, 0x80, 0x80, 0xff);
    if let Some(r) = tiny_skia::Rect::from_xywh(bx, byy, btn_size, 1.0) { pixmap.fill_rect(r, &border_paint, tx, None); }
    if let Some(r) = tiny_skia::Rect::from_xywh(bx, byy + btn_size - 1.0, btn_size, 1.0) { pixmap.fill_rect(r, &border_paint, tx, None); }
    if let Some(r) = tiny_skia::Rect::from_xywh(bx, byy, 1.0, btn_size) { pixmap.fill_rect(r, &border_paint, tx, None); }
    if let Some(r) = tiny_skia::Rect::from_xywh(bx + btn_size - 1.0, byy, 1.0, btn_size) { pixmap.fill_rect(r, &border_paint, tx, None); }

    // +/- 符号:用矩形线段画,比Path三角形更简单,缩放后更清晰。
    let sym_len = btn_size * 0.5;
    let sym_thick = 1.6_f32;
    let sym_cx = bx + btn_size / 2.0;
    let sym_cy = byy + btn_size / 2.0;
    // 横线(-和+都有)
    if let Some(r) = tiny_skia::Rect::from_xywh(sym_cx - sym_len / 2.0, sym_cy - sym_thick / 2.0, sym_len, sym_thick) {
        pixmap.fill_rect(r, &sym_paint, tx, None);
    }
    // 竖线(只有+需要,标示折叠态"可展开")
    if is_collapsed {
        if let Some(r) = tiny_skia::Rect::from_xywh(sym_cx - sym_thick / 2.0, sym_cy - sym_len / 2.0, sym_thick, sym_len) {
            pixmap.fill_rect(r, &sym_paint, tx, None);
        }
    }
}

impl<'a> CalcTileRenderer<'a> {
    /// 虚拟视口渲染：把从 (scroll_x_pt, scroll_y_pt) 开始的可视区域渲染到 buf。
    /// vw/vh：viewport 像素宽高（含标题栏）。
    /// header_col_w_pt：左侧行号标题栏宽度（逻辑 pt）。
    /// header_row_h_pt：顶部列字母标题栏高度（逻辑 pt）。
    /// zoom_pct：缩放百分比（100=1x，200=2x）。
    /// buf.len() 必须等于 vw * vh * 4，否则返回 false。
    pub fn render_viewport_into(
        &mut self,
        scroll_x_pt: f32,
        scroll_y_pt: f32,
        vw: u32,
        vh: u32,
        zoom_pct: u32,
        header_col_w_pt: f32,
        header_row_h_pt: f32,
        buf: &mut [u8],
    ) -> bool {
        let expected = (vw as usize) * (vh as usize) * 4;
        if buf.len() != expected { return false; }
        let mut pixmap = match tiny_skia::PixmapMut::from_bytes(buf, vw, vh) {
            Some(p) => p,
            None => return false,
        };
        let scale = (zoom_pct.max(100) as f32) / 100.0;
        let cfg = &self.config;
        let sheet = match self.workbook.sheets.get(self.sheet_idx) {
            Some(s) => s,
            None => return false,
        };

        pixmap.fill(tiny_skia::Color::WHITE);

        let data_px_x = (header_col_w_pt * scale) as i32;
        let data_px_y = (header_row_h_pt * scale) as i32;
        let data_vw = vw as i32 - data_px_x;
        let data_vh = vh as i32 - data_px_y;
        if data_vw <= 0 || data_vh <= 0 { return true; }

        let data_logical_w = data_vw as f32 / scale;
        let data_logical_h = data_vh as f32 / scale;

        // scroll/data_logical_* 是像素语义(与 tx、col_x_px 一致),col_at_x/row_at_y 按 pt 累加,先换成 pt 再查行列
        let col_start = col_at_x(sheet, scroll_x_pt / PT_TO_PX, cfg);
        let col_end   = col_at_x(sheet, (scroll_x_pt + data_logical_w) / PT_TO_PX, cfg) + 1;
        let row_start = row_at_y(sheet, scroll_y_pt / PT_TO_PX, cfg);
        let row_end   = row_at_y(sheet, (scroll_y_pt + data_logical_h) / PT_TO_PX, cfg) + 1;

        let tx = tiny_skia::Transform::from_translate(
            -scroll_x_pt + header_col_w_pt,
            -scroll_y_pt + header_row_h_pt,
        ).post_scale(scale, scale);

        // 网格线（ROOT-015对齐：show_grid_lines默认None/Some(true)时画，只有显式
        // Some(false)（对应xlsx showGridLines="0"）时才跳过整块绘制。从render_sheet_preview
        // 迁移而来，此前viewport路径无条件绘制网格线，完全没有这个开关判断，是本次
        // 系统排查发现的第七处渲染缺口）。
        if sheet.show_grid_lines != Some(false) {
        let mut grid_paint = tiny_skia::Paint::default();
        grid_paint.set_color_rgba8(cfg.grid_color[0], cfg.grid_color[1], cfg.grid_color[2], cfg.grid_color[3]);
        let stroke = tiny_skia::Stroke { width: 1.0, ..Default::default() };

        for c in col_start..=col_end {
            let x = col_x_px(sheet, c, cfg);
            let mut pb = tiny_skia::PathBuilder::new();
            pb.move_to(x, scroll_y_pt);
            pb.line_to(x, scroll_y_pt + data_logical_h);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &grid_paint, &stroke, tx, None);
            }
        }
        for r in row_start..=row_end {
            let y = row_y_px(sheet, r, cfg);
            let mut pb = tiny_skia::PathBuilder::new();
            pb.move_to(scroll_x_pt, y);
            pb.line_to(scroll_x_pt + data_logical_w, y);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &grid_paint, &stroke, tx, None);
            }
        }
        } // if sheet.show_grid_lines != Some(false)

        // 合并单元格：背景色覆盖内部网格线，再画外边框（从render_sheet_preview/
        // paint_tile迁移而来。此前render_viewport_into完全没有这段逻辑，导致
        // 合并单元格在前台交互滚动场景下网格线贯穿整个合并区域、且没有外边框，
        // 是本次系统排查发现的第四处viewport渲染缺口）。只处理与当前viewport
        // 可见范围有交叉的合并区域，画在网格线之上、col_style/cond_formats之下。
        {
            let mut bg_paint = tiny_skia::Paint::default();
            bg_paint.set_color_rgba8(cfg.bg_color[0], cfg.bg_color[1], cfg.bg_color[2], cfg.bg_color[3]);
            let mut border_paint = tiny_skia::Paint::default();
            border_paint.set_color_rgba8(cfg.grid_color[0], cfg.grid_color[1], cfg.grid_color[2], cfg.grid_color[3]);
            let merge_stroke = tiny_skia::Stroke { width: 1.0, ..Default::default() };
            for mc in &sheet.merged_cells {
                if mc.r2 < row_start || mc.r1 >= row_end { continue; }
                if mc.c2 < col_start || mc.c1 >= col_end { continue; }
                let x0 = col_x_px(sheet, mc.c1, cfg);
                let y0 = row_y_px(sheet, mc.r1, cfg);
                let total_w: f32 = (mc.c1..=mc.c2).map(|c| col_width_at_px(sheet, c, cfg)).sum();
                let total_h: f32 = (mc.r1..=mc.r2).map(|r| row_height_at_px(sheet, r, cfg)).sum();
                if let Some(inner) = tiny_skia::Rect::from_xywh(x0 + 1.0, y0 + 1.0, (total_w - 2.0).max(0.0), (total_h - 2.0).max(0.0)) {
                    let inner_path = tiny_skia::PathBuilder::from_rect(inner);
                    pixmap.fill_path(&inner_path, &bg_paint, tiny_skia::FillRule::Winding, tx, None);
                }
                if let Some(outer) = tiny_skia::Rect::from_xywh(x0, y0, total_w, total_h) {
                    let outer_path = tiny_skia::PathBuilder::from_rect(outer);
                    pixmap.stroke_path(&outer_path, &border_paint, &merge_stroke, tx, None);
                }
            }
        }

        // 列级默认样式背景色（xlsx <col style="N">，ROOT column_style修复）
        // 从render_sheet_preview迁移而来（该迁移此前遗漏，导致viewport架构下
        // 列级默认背景色不显示）。只在可见范围（col_start..col_end, row_start..row_end）
        // 内绘制，画在网格线之上、cond_formats/单元格自身背景色之下。
        {
            let mut col_bg_paint = tiny_skia::Paint::default();
            for col_idx in col_start..col_end {
                let fmt = match sheet.col_style.get(col_idx as usize).and_then(|o| o.as_ref()) {
                    Some(f) => f,
                    None => continue,
                };
                let bg = match fmt.bg_color.as_deref().and_then(parse_hex_color) {
                    Some(c) => c,
                    None => continue,
                };
                if bg[3] == 0 { continue; }
                col_bg_paint.set_color_rgba8(bg[0], bg[1], bg[2], bg[3]);
                let x = col_x_px(sheet, col_idx, cfg);
                let w = col_width_at_px(sheet, col_idx, cfg);
                let y = row_y_px(sheet, row_start, cfg);
                let h = row_y_px(sheet, row_end, cfg) - y;
                if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
                    let path = tiny_skia::PathBuilder::from_rect(rect);
                    pixmap.fill_path(&path, &col_bg_paint, tiny_skia::FillRule::Winding, tx, None);
                }
            }
        }

        // 表格样式底色(Table/ListObject):按 Table.range ∩ 视口逐格补画,无数据的空表也能显示。
        // 画在条件格式/数据条之下;单元格自身背景色在文字循环里后画,自然覆盖这里(自身格式优先)。
        for t in &sheet.tables {
            if t.style_name.is_none() { continue; }
            let (tr1, tc1) = t.range.start;
            let (tr2, tc2) = t.range.end;
            let r_lo = tr1.max(row_start);
            let r_hi = tr2.min(row_end.saturating_sub(1));
            let c_lo = tc1.max(col_start);
            let c_hi = tc2.min(col_end.saturating_sub(1));
            if r_lo > r_hi || c_lo > c_hi { continue; }
            for r in r_lo..=r_hi {
                if sheet.row_hidden.get(r as usize).copied().unwrap_or(false) { continue; }
                for c in c_lo..=c_hi {
                    if sheet.col_hidden.get(c as usize).copied().unwrap_or(false) { continue; }
                    let bg = match roe_model::excel_table_styles::table_style_at(t, r, c).and_then(|st| st.bg) {
                        Some(b) => b,
                        None => continue,
                    };
                    let x = col_x_px(sheet, c, cfg) + 1.0;
                    let y = row_y_px(sheet, r, cfg) + 1.0;
                    let w = (col_width_at_px(sheet, c, cfg) - 2.0).max(0.0);
                    let h = (row_height_at_px(sheet, r, cfg) - 2.0).max(0.0);
                    if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
                        let mut p = tiny_skia::Paint::default();
                        p.set_color_rgba8(bg[0], bg[1], bg[2], 255);
                        let path = tiny_skia::PathBuilder::from_rect(rect);
                        pixmap.fill_path(&path, &p, tiny_skia::FillRule::Winding, tx, None);
                    }
                }
            }
        }

        // 条件格式背景色渲染（在网格线之上、文字之下）
        // Bug修复：与floating_charts/pivot/floating_images同批遗漏，
        // render_sheet_preview升级到render_viewport_into时cond_formats渲染逻辑没有一并迁移。
        {
            let mut cf_paint = tiny_skia::Paint::default();
            for rule in &sheet.cond_formats {
                let (condition, format) = match &rule.kind {
                    roe_model::calc::ConditionalFormatKind::CellFormat { condition, format } => (condition, format),
                    _ => continue,
                };
                let bg = match format.bg_color.as_deref().and_then(parse_hex_color) {
                    Some(c) => c,
                    None => continue,
                };
                let r1 = rule.range.start.0;
                let c1 = rule.range.start.1;
                let r2 = rule.range.end.0;
                let c2 = rule.range.end.1;
                if r2 < row_start || r1 >= row_end { continue; }
                if c2 < col_start || c1 >= col_end { continue; }
                cf_paint.set_color_rgba8(bg[0], bg[1], bg[2], bg[3]);
                for row_idx in r1.max(row_start)..=r2.min(row_end.saturating_sub(1)) {
                    // ROOT-003对齐：隐藏行不绘制条件格式背景色（从render_sheet_preview
                    // 迁移而来，此前viewport路径缺少这个判断，是本次系统排查发现的
                    // 第五处渲染缺口，与文字渲染循环同一性质的遗漏）。
                    if sheet.row_hidden.get(row_idx as usize).copied().unwrap_or(false) {
                        continue;
                    }
                    for col_idx in c1.max(col_start)..=c2.min(col_end.saturating_sub(1)) {
                        // ROOT-032对齐：隐藏列同隐藏行处理。
                        if sheet.col_hidden.get(col_idx as usize).copied().unwrap_or(false) {
                            continue;
                        }
                        let val = sheet.cells.get(row_idx, col_idx);
                        if !cond_matches_ctx(&val, condition, Some(sheet), Some(self.workbook), self.formula_evaluator.as_deref(), Some((row_idx, col_idx)), Some(rule.range.start)) { continue; }
                        let x = col_x_px(sheet, col_idx, cfg) + 1.0;
                        let y = row_y_px(sheet, row_idx, cfg) + 1.0;
                        let w = (col_width_at_px(sheet, col_idx, cfg) - 2.0).max(0.0);
                        let h = (row_height_at_px(sheet, row_idx, cfg) - 2.0).max(0.0);
                        if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
                            let path = tiny_skia::PathBuilder::from_rect(rect);
                            pixmap.fill_path(&path, &cf_paint, tiny_skia::FillRule::Winding, tx, None);
                        }
                    }
                }
            }
        }


        // ROOT-067(v130)第二轮修复：DataBar/ColorScale命中单元格的静态
        // 背景色(cellXfs/col_style.bg_color)需要先画一遍，再画DataBar/
        // ColorScale本身叠在上层——顺序颠倒会导致要么背景盖住数据条
        // （第一轮bug），要么跳过背景色画出数据条但丢失了背景色本身
        // （第一轮"修复"引入的新问题，OnlyOffice参考图里这类单元格背景
        // 色是应该保留、只是在其上层叠加数据条的）。
        {
            let mut pre_bg_paint = tiny_skia::Paint::default();
            for rule in &sheet.cond_formats {
                let is_bar_or_scale = matches!(
                    &rule.kind,
                    roe_model::calc::ConditionalFormatKind::DataBar { .. }
                        | roe_model::calc::ConditionalFormatKind::ColorScale { .. }
                );
                if !is_bar_or_scale { continue; }
                let r1 = rule.range.start.0;
                let c1 = rule.range.start.1;
                let r2 = rule.range.end.0;
                let c2 = rule.range.end.1;
                if r2 < row_start || r1 >= row_end { continue; }
                if c2 < col_start || c1 >= col_end { continue; }
                for row_idx in r1.max(row_start)..=r2.min(row_end.saturating_sub(1)) {
                    if sheet.row_hidden.get(row_idx as usize).copied().unwrap_or(false) { continue; }
                    for col_idx in c1.max(col_start)..=c2.min(col_end.saturating_sub(1)) {
                        if sheet.col_hidden.get(col_idx as usize).copied().unwrap_or(false) { continue; }
                        let cf = sheet.cell_formats.get(&(row_idx, col_idx))
                            .or_else(|| sheet.col_style.get(col_idx as usize).and_then(|o| o.as_ref()));
                        let bg = match cf.and_then(|f| f.bg_color.as_deref()).and_then(parse_hex_color) {
                            Some(c) => c,
                            None => continue,
                        };
                        pre_bg_paint.set_color_rgba8(bg[0], bg[1], bg[2], bg[3]);
                        let x = col_x_px(sheet, col_idx, cfg) + 1.0;
                        let y = row_y_px(sheet, row_idx, cfg) + 1.0;
                        let w = (col_width_at_px(sheet, col_idx, cfg) - 2.0).max(0.0);
                        let h = (row_height_at_px(sheet, row_idx, cfg) - 2.0).max(0.0);
                        if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
                            let path = tiny_skia::PathBuilder::from_rect(rect);
                            pixmap.fill_path(&path, &pre_bg_paint, tiny_skia::FillRule::Winding, tx, None);
                        }
                    }
                }
            }
        }

        // 数据条 / 色阶渲染（独立文件实现，见 cond_format_render.rs）
        crate::cond_format_render::render_databar_and_colorscale(
            &mut pixmap, sheet, cfg, tx, row_start, row_end, col_start, col_end,
            self.workbook, self.formula_evaluator.as_deref(),
        );

        // Sparkline（迷你图）渲染（独立文件实现，见 sparkline_render.rs）
        crate::sparkline_render::render_sparklines(
            &mut pixmap, sheet, cfg, tx, row_start, row_end, col_start, col_end,
        );

        // 数据验证可视化标记（list下拉箭头/错误红角，独立文件实现，见 data_validation_render.rs）
        crate::data_validation_render::render_data_validation_marks(
            &mut pixmap, sheet, cfg, tx, row_start, row_end, col_start, col_end,
        );

        // 单元格文字
        // 计算透视表整体占用区域（若存在），避免与静态快照单元格重复渲染
        // 透视表跳过区域：只跳过表头快照部分（列表头行范围 + 行字段列范围 + 角落区），
        // 数据区本身（data_row_start行、data_col_start列之后）不跳过，
        // 让静态快照里的实际数值通过下方常规单元格渲染逻辑画出来（方案2：复用快照数值）。
        let pivot_occupied_region: Option<(u32, u32, u32, u32)> = sheet.pivot.as_ref().map(|p| {
            (p.base_row, p.base_col, p.base_row + p.data_row_start, p.last_col + 1)
        });
        let pivot_row_header_region: Option<(u32, u32, u32, u32)> = sheet.pivot.as_ref().map(|p| {
            (p.base_row, p.base_col, p.last_row + 1, p.base_col + p.data_col_start)
        });
        let pivot_data_region: Option<(u32, u32, u32, u32)> = sheet.pivot.as_ref().and_then(|p| {
            if p.data.cells.is_empty() { None } else {
                Some((p.base_row + p.data_row_start, p.base_col + p.data_col_start, p.last_row + 1, p.last_col + 1))
            }
        });

        // 物化后的真实包围盒(含端点):区域内的普通格一律由透视 meta 绘制,这里跳过避免叠字
        let pivot_cells_rect: Option<(u32, u32, u32, u32)> = sheet.pivot.as_ref().and_then(|p| p.cells_rect);

        for col_idx in col_start..col_end {
            if let Some(sc) = sheet.cells.cols.get(&col_idx) {
                for block in &sc.blocks {
                    for (i, val) in block.values.iter().enumerate() {
                        let row_idx = block.start_row + i as u32;
                        if row_idx < row_start || row_idx >= row_end { continue; }
                        // ROOT-003/ROOT-032对齐：隐藏行/列不绘制文字（从render_sheet_preview
                        // 迁移而来，此前viewport路径的文字渲染循环缺少这两个判断，是本次
                        // 系统排查发现的第六处渲染缺口）。
                        if sheet.row_hidden.get(row_idx as usize).copied().unwrap_or(false) {
                            continue;
                        }
                        if sheet.col_hidden.get(col_idx as usize).copied().unwrap_or(false) {
                            continue;
                        }
                        if let Some((r1, c1, r2, c2)) = pivot_cells_rect {
                            if row_idx >= r1 && row_idx <= r2 && col_idx >= c1 && col_idx <= c2 { continue; }
                        }
                        if let Some((br, bc, last_row, last_col)) = pivot_occupied_region {
                            if row_idx >= br && row_idx < last_row && col_idx >= bc && col_idx < last_col { continue; }
                        }
                        if let Some((br, bc, last_row, last_col)) = pivot_row_header_region {
                            if row_idx >= br && row_idx < last_row && col_idx >= bc && col_idx < last_col { continue; }
                        }
                        if let Some((br, bc, last_row, last_col)) = pivot_data_region {
                            if row_idx >= br && row_idx < last_row && col_idx >= bc && col_idx < last_col { continue; }
                        }
                        // cf: 单元格自身样式（fallback到列级默认样式col_style），
                        // 提到循环体最外层供背景色渲染和下方文字颜色/粗体渲染共用。
                        // 此前只在背景色渲染的{}块内部定义，出了块就不可见，导致
                        // 文字颜色/粗体渲染无法访问cell_formats.fg_color/bold，
                        // 是本次新发现的第三处viewport渲染缺口（与auto_filter/
                        // cell_formats背景色同一性质：render_sheet_preview有、
                        // render_viewport_into从未跟进的字体样式部分）。
                        let cf = sheet.cell_formats.get(&(row_idx, col_idx))
                            .or_else(|| sheet.col_style.get(col_idx as usize).and_then(|o| o.as_ref()));
                        // 表格样式(Table/ListObject):仅在单元格自身格式缺省时作为回退
                        let tstyle = roe_model::excel_table_styles::table_cell_style(sheet, row_idx, col_idx);
                        // 单元格自身背景色渲染（cell_formats.bg_color，从render_sheet_preview
                        // 迁移而来。此前render_viewport_into单元格渲染循环只取了num_format，
                        // 从未画过背景色，导致所有用户手动设置背景色/Table样式展开成
                        // cell_formats的场景，前台完全看不到颜色，只有走render_sheet_preview
                        // 的批量核对工具能看到——三条路径功能分裂的又一处具体表现）。
                        // 与render_sheet_preview一致：错误值打底浅红色；否则查cell_formats，
                        // 没有则fallback到col_style（列级默认样式）。画在网格线之上、文字之下，
                        // 必须放在text.is_empty()判断之前，否则空单元格（无文字但有背景色）会
                        // 被漏画。
                        {
                            let is_error = matches!(val, roe_model::calc::CellValue::Error(_));
                            let mut cell_bg_paint = tiny_skia::Paint::default();
                            if is_error {
                                let fill = cell_fill_color(val);
                                cell_bg_paint.set_color_rgba8(fill[0], fill[1], fill[2], fill[3]);
                                let x = col_x_px(sheet, col_idx, cfg) + 2.0;
                                let y = row_y_px(sheet, row_idx, cfg) + 2.0;
                                let w = (col_width_at_px(sheet, col_idx, cfg) - 4.0).max(0.0);
                                let h = (row_height_at_px(sheet, row_idx, cfg) - 4.0).max(0.0);
                                if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
                                    let path = tiny_skia::PathBuilder::from_rect(rect);
                                    pixmap.fill_path(&path, &cell_bg_paint, tiny_skia::FillRule::Winding, tx, None);
                                }
                            } else if let Some(bg) = cf.and_then(|f| f.bg_color.as_deref()).and_then(parse_hex_color) {
                                // ROOT-067(v130)：DataBar/ColorScale规则命中的单元格，
                                // 静态背景色(cellXfs/col_style)不应再整格铺色覆盖，
                                // 否则会把已经画好的数据条/色阶完全盖住（无论是纯色
                                // 覆盖还是半透明混合，视觉上都是错的——OnlyOffice/Excel
                                // 效果是数据条/色阶本身替代了单元格底色，不是叠加关系）。
                                let covered_by_databar_or_colorscale = sheet.cond_formats.iter().any(|rule| {
                                    let is_bar_or_scale = matches!(
                                        &rule.kind,
                                        roe_model::calc::ConditionalFormatKind::DataBar { .. }
                                            | roe_model::calc::ConditionalFormatKind::ColorScale { .. }
                                    );
                                    is_bar_or_scale
                                        && row_idx >= rule.range.start.0 && row_idx <= rule.range.end.0
                                        && col_idx >= rule.range.start.1 && col_idx <= rule.range.end.1
                                });
                                if !covered_by_databar_or_colorscale {
                                cell_bg_paint.set_color_rgba8(bg[0], bg[1], bg[2], bg[3]);
                                let x = col_x_px(sheet, col_idx, cfg) + 1.0;
                                let y = row_y_px(sheet, row_idx, cfg) + 1.0;
                                let w = (col_width_at_px(sheet, col_idx, cfg) - 2.0).max(0.0);
                                let h = (row_height_at_px(sheet, row_idx, cfg) - 2.0).max(0.0);
                                if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
                                    let path = tiny_skia::PathBuilder::from_rect(rect);
                                    pixmap.fill_path(&path, &cell_bg_paint, tiny_skia::FillRule::Winding, tx, None);
                                }
                                }
                            }
                        }

                        // ROOT-077: 数字格式同样需要"条件格式优先，fallback到静态样式"，
                        // 与下方text_color/bold的模式保持一致。此前只查cell_formats（静态
                        // cellXfs样式），完全没有处理条件格式(dxf)携带的numFmt——Excel条件
                        // 格式支持"命中规则时改变数字显示格式"（如new_cond_format_test_export
                        // 用例：dxf内联<numFmt formatCode="0.00"/>，命中expression规则的单元格
                        // 应显示为两位小数，未命中的单元格保持原始整数显示），此前这条路径
                        // 完全没打通，dxf解析阶段num_format也从未被填值（本次同批修复）。
                        let num_fmt = sheet.cond_formats.iter()
                            .find_map(|rule| {
                                let (condition, format) = match &rule.kind {
                                    roe_model::calc::ConditionalFormatKind::CellFormat { condition, format } => (condition, format),
                                    _ => return None,
                                };
                                if row_idx >= rule.range.start.0 && row_idx <= rule.range.end.0 &&
                                   col_idx >= rule.range.start.1 && col_idx <= rule.range.end.1 &&
                                   cond_matches_ctx(&sheet.cells.get(row_idx, col_idx), condition, Some(sheet), Some(self.workbook), self.formula_evaluator.as_deref(), Some((row_idx, col_idx)), Some(rule.range.start)) &&
                                   format.num_format.is_some()
                                {
                                    format.num_format.as_deref()
                                } else {
                                    None
                                }
                            })
                            .or_else(|| sheet.cell_formats.get(&(row_idx, col_idx)).and_then(|f| f.num_format.as_deref()));
                        let text = cell_value_to_text_fmt(val, num_fmt, sheet.date1904);
                        if text.is_empty() { continue; }
                        let merge = sheet.merged_cells.iter().find(|mc|
                            row_idx >= mc.r1 && row_idx <= mc.r2 &&
                            col_idx >= mc.c1 && col_idx <= mc.c2
                        );
                        if let Some(mc) = merge {
                            if row_idx != mc.r1 || col_idx != mc.c1 { continue; }
                        }
                        // ROOT-076: wrap_text 提前到 avail_w 计算之前算出——wrapText
                        // 单元格不能像普通单元格那样借用右侧连续空单元格的宽度来避免
                        // 截断，Excel/WPS的行为是wrapText单元格严格限制在自身列宽内
                        // 自动折行，与"非wrapText单元格允许文字溢出到右侧空单元格"是
                        // 两套完全不同的规则。此前统一走compute_available_width_for_row
                        // 导致wrapText单元格拿到的avail_w是"自身列宽+右侧所有连续空列
                        // 宽度之和"，实测tdf123026用例E列右侧F~T列都是空列，avail_w被
                        // 累加到1316pt，远超Parley断行阈值需要的实际列宽，导致wrapText
                        // 视觉上完全没有触发折行效果（并非没有解析到wrapText=true，
                        // 也不是宽度换算系数错误，而是这里传入的"可用宽度"语义本身
                        // 对wrapText场景就是错的）。
                        let wrap_text = cf.and_then(|f| f.wrap_text).unwrap_or(false);
                        // ROOT-050 Bug A修复：avail_w/cell_x/cell_y/cell_h统一改用pt版本函数，
                        // 配合draw_cell_text调用处scale改为scale*PT_TO_PX，让函数内部统一完成
                        // pt→px转换，与该处像素级绘制（cell_rect_px等，仍用_px版本）分开两套坐标。
                        let avail_w = if let Some(mc) = merge {
                            (mc.c1..=mc.c2).map(|c| col_width_at(sheet, c, cfg)).sum::<f32>()
                        } else if wrap_text {
                            col_width_at(sheet, col_idx, cfg)
                        } else {
                            compute_available_width_for_row(sheet, col_idx, row_idx, col_end, cfg)
                        };
                        // ROOT-082对齐修复第二轮：avail_w在非merge非wrap场景下语义是"允许溢出的
                        // 可用宽度"（可能横跨多个空列），用来做center/right/distributed对齐的
                        // 参照宽度会导致文字被推到远超单元格自身范围的位置（实测tdf123139的C2，
                        // distributed对齐+avail_w=2051px，文字被推到视觉上像是"跑到后面几列"的
                        // 位置）。cell_own_width专门表示"单元格自身宽度"（merge/wrap分支与avail_w
                        // 一致，因为这两种场景avail_w本来就不含溢出语义；仅非merge非wrap分支换成
                        // col_width_at单列宽度），只用于非Left对齐时的align参照，不影响avail_w
                        // 本身承担的换行/裁剪宽度职责。
                        let cell_own_width = if let Some(mc) = merge {
                            (mc.c1..=mc.c2).map(|c| col_width_at(sheet, c, cfg)).sum::<f32>()
                        } else {
                            col_width_at(sheet, col_idx, cfg)
                        };
                        let mut cell_x = col_x(sheet, col_idx, cfg);
                        let cell_y = row_y(sheet, row_idx, cfg);
                        let cell_h = if let Some(mc) = merge {
                            (mc.r1..=mc.r2).map(|r| row_height_at(sheet, r, cfg)).sum::<f32>()
                        } else {
                            row_height_at(sheet, row_idx, cfg)
                        };
                        // ROOT-024后续修复：IconSet单元格给图标预留左侧空间
                        let icon_reserve = crate::icon_set_render::icon_set_text_reserve_width(
                            sheet, cfg, row_idx, col_idx,
                        );
                        // 图标只画在左侧，不占单元格宽度：仅左对齐文字需要避让图标，
                        // 右对齐/居中的文字按整格宽度对齐（与Excel/WPS一致）。
                        let icon_text_left = {
                            let h_raw = cf.and_then(|f| f.align_h.as_deref()).unwrap_or("");
                            match h_raw {
                                "center" | "centerContinuous" | "distributed" | "right" => false,
                                "" | "general" => {
                                    let mut inner = val;
                                    while let roe_model::calc::CellValue::Formula { cached, .. } = inner {
                                        inner = &**cached;
                                    }
                                    !matches!(
                                        inner,
                                        roe_model::calc::CellValue::Number(_)
                                            | roe_model::calc::CellValue::Bool(_)
                                            | roe_model::calc::CellValue::Error(_)
                                    )
                                }
                                _ => true,
                            }
                        };
                        let icon_reserve = if icon_text_left { icon_reserve } else { 0.0 };
                        let avail_w = (avail_w - icon_reserve).max(0.0);
                        cell_x += icon_reserve;
                        // 文字颜色：条件格式命中优先，否则fallback到单元格自身样式
                        // cell_formats.fg_color（从render_sheet_preview迁移而来，
                        // 此前viewport路径只处理了cond_formats覆盖色，没有这层fallback，
                        // 导致所有仅靠cellXfs/Table样式设置字体颜色、未命中条件格式的
                        // 场景，前台文字颜色全部退化成默认灰黑色）。
                        let text_color = sheet.cond_formats.iter()
                            .find_map(|rule| {
                                let (condition, format) = match &rule.kind {
                                    roe_model::calc::ConditionalFormatKind::CellFormat { condition, format } => (condition, format),
                                    _ => return None,
                                };
                                if row_idx >= rule.range.start.0 && row_idx <= rule.range.end.0 &&
                                   col_idx >= rule.range.start.1 && col_idx <= rule.range.end.1 &&
                                   cond_matches_ctx(&sheet.cells.get(row_idx, col_idx), condition, Some(sheet), Some(self.workbook), self.formula_evaluator.as_deref(), Some((row_idx, col_idx)), Some(rule.range.start)) &&
                                   format.fg_color.is_some()
                                {
                                    format.fg_color.as_deref()
                                } else {
                                    None
                                }
                            })
                            .and_then(parse_hex_color)
                            .map(|c| [c[0], c[1], c[2]])
                            .or_else(|| cf.and_then(|f| f.fg_color.as_deref()).and_then(parse_hex_color).map(|c| [c[0], c[1], c[2]]))
                            .or_else(|| tstyle.as_ref().and_then(|t| t.fg))
                            .unwrap_or([0x20, 0x20, 0x20]);
                        // 粗体：同样从render_sheet_preview迁移而来，此前viewport路径
                        // 固定传false，导致cell_formats.bold=true的单元格前台不显示粗体。
                        let bold = cf.and_then(|f| f.bold).unwrap_or_else(|| tstyle.as_ref().map_or(false, |t| t.bold));
                        let italic = cf.and_then(|f| f.italic).unwrap_or(false);
                        let underline = cf.and_then(|f| f.underline).unwrap_or(false);
                        // ROOT-078: 单元格是sharedStrings富文本（多run混合格式，
                        // 目前仅支持strikethrough）时，走独立的draw_cell_rich_text
                        // 路径以画出run级格式；其余绝大多数单元格（Text/Number/
                        // Formula等）保持原有draw_cell_text不变，不受影响。这是
                        // 20处draw_cell_text调用点里唯一加了这个分支的一处——其余
                        // 19处（透视表/公式栏等次要展示场景）继续统一走cell_value_
                        // to_text拼接纯文本的原有行为，暂不支持富文本格式展示，
                        // 属于本次范围内的合理简化，不是遗漏。
                        // ROOT-082: 字号改读cell_formats.font_size（缺省12.0），
                        // 此前draw_cell_text/draw_cell_rich_text内部硬编码12.0，
                        // 与自动行高测量（measure_row_height_cheap/wrap）必须用
                        // 同一个字号来源，否则会出现"行高按真实字号算，但文字仍按
                        // 12.0画"的测绘撕裂。这是本次唯一改动字号来源的核心路径；
                        // 其余19处透视表/公式栏次要展示调用点保持传字面量12.0不变
                        // （它们没有per-cell CellFormat概念，维持现状是合理简化）。
                        let font_size_pt = cf.and_then(|f| f.font_size).unwrap_or(10.0);
                        // ROOT-082 Paint阶段font_family接入：与font_size_pt同一模式，
                        // 读取真实CellFormat.font_family（fallback Calibri，fontconfig
                        // 环境下会metric-compatible映射到Carlito）。这是本次唯一改动
                        // 字体族来源的核心路径；其余19处透视表/公式栏次要展示调用点
                        // 保持传字面量"Calibri"不变（它们没有per-cell CellFormat概念，
                        // 维持现状是合理简化，与font_size_pt的处理模式一致）。
                        let font_family_raw = cf.and_then(|f| f.font_family.as_deref()).unwrap_or("Calibri");
                        // 字体名归一化修复：与Layout阶段（row_height_measure.rs）必须
                        // 调用同一个函数，否则"Calibri"这个原始字符串直接传给Parley
                        // 会导致数字/符号字形丢失（见resolve_font_family_for_parley
                        // 注释、roe-ext_progress_v142.md 1.5节踩坑记录）。这是Paint阶段
                        // font_family接入后暴露的新bug，Layout阶段此前侥幸没暴露是因为
                        // 只做测量不渲染字形。
                        let font_family = crate::row_height_measure::resolve_font_family_for_parley(font_family_raw);
                        // ROOT-082对齐接入：此前渲染层完全没有读取CellFormat.align_h/
                        // align_v（水平永远Alignment::Start即左对齐，垂直永远居中），
                        // 与xlsx实际设置的对齐方式无关。与font_size_pt/font_family同一
                        // 模式：主循环传真实值，其余19处透视表调用点传字面量默认值。
                        let align_h_raw = cf.and_then(|f| f.align_h.as_deref()).unwrap_or("");
                        // 常规对齐（未指定或general）：渲染时按值类型决定，不写进格式。
                        // 数字右对齐，布尔/错误居中，文本及其余左对齐；公式取缓存结果判断。
                        let align_h = if align_h_raw.is_empty() || align_h_raw == "general" {
                            let mut inner = val;
                            while let roe_model::calc::CellValue::Formula { cached, .. } = inner {
                                inner = &**cached;
                            }
                            match inner {
                                roe_model::calc::CellValue::Number(_) => "right",
                                roe_model::calc::CellValue::Bool(_)
                                | roe_model::calc::CellValue::Error(_) => "center",
                                _ => "",
                            }
                        } else {
                            align_h_raw
                        };
                        let align_v = cf.and_then(|f| f.align_v.as_deref()).unwrap_or("");
                        if let roe_model::calc::CellValue::RichText(runs) = val {
                            draw_cell_rich_text(
                                &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                runs, cell_x, cell_y, avail_w, cell_h,
                                scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale, text_color, bold, wrap_text,
                                font_size_pt, font_family, align_h, align_v, cell_own_width,
                            );
                        } else {
                            crate::tile_render_helpers::draw_cell_text_styled(
                                &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                &text, cell_x, cell_y, avail_w, cell_h,
                                scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale, text_color,
                                crate::tile_render_helpers::CellTextStyle { bold, italic, underline },
                                wrap_text,
                                font_size_pt, font_family, align_h, align_v, cell_own_width,
                            );
                        }
                    }
                }
            }
        }

        // ── 单元格边框 pass(第一期:实线 hair/thin/medium/thick,double 与虚线按 thin 实线画)──
        // 每条竖边/横边只画一次:候选为两侧格子的对边,粗者优先,同粗取右/下格。
        // 合并区内部的边跳过。放在文字循环之后,边框叠在最上层。
        {
            use roe_model::calc::BorderLine;
            fn bw(s: &str) -> f32 {
                match s {
                    "hair" => 0.5,
                    "medium" | "mediumDashed" | "mediumDashDot" | "mediumDashDotDot" | "slantDashDot" => 2.0,
                    "thick" => 3.0,
                    _ => 1.0,
                }
            }
            // side: 0=top 1=bottom 2=left 3=right。单元格自身该边无边框时回退表格样式边框
            // (TableCellStyle.borders 顺序同此)。返回拥有型,因表格边框是临时合成的。
            fn edge(sh: &roe_model::calc::Sheet, r: u32, c: u32, side: u8) -> Option<BorderLine> {
                let own = sh.cell_formats.get(&(r, c))
                    .or_else(|| sh.col_style.get(c as usize).and_then(|o| o.as_ref()))
                    .and_then(|f| f.borders.as_ref())
                    .and_then(|b| match side {
                        0 => b.top.as_ref(),
                        1 => b.bottom.as_ref(),
                        2 => b.left.as_ref(),
                        _ => b.right.as_ref(),
                    })
                    .cloned();
                if own.is_some() || sh.tables.is_empty() {
                    return own;
                }
                let ts = roe_model::excel_table_styles::table_cell_style(sh, r, c)?;
                let tb = ts.borders[side as usize]?;
                let style = match tb.style {
                    2 => "medium",
                    3 => "double",
                    4 => "thick",
                    _ => "thin",
                };
                Some(BorderLine {
                    style: style.to_string(),
                    color: Some(format!("#{:02X}{:02X}{:02X}", tb.color[0], tb.color[1], tb.color[2])),
                })
            }
            fn pick(a: Option<BorderLine>, b: Option<BorderLine>) -> Option<BorderLine> {
                match (a, b) {
                    (Some(x), Some(y)) => if bw(&x.style) > bw(&y.style) { Some(x) } else { Some(y) },
                    (Some(x), None) => Some(x),
                    (None, y) => y,
                }
            }
            let vis_merges: Vec<&roe_model::calc::MergedCell> = sheet.merged_cells.iter()
                .filter(|m| m.r2 >= row_start && m.r1 < row_end && m.c2 >= col_start && m.c1 < col_end)
                .collect();
            let mut segs: Vec<(f32, f32, f32, f32, BorderLine)> = Vec::new();
            // 竖边:行 r,列边界 c(位于第 c-1 列与第 c 列之间)
            for r in row_start..row_end {
                let h = row_height_at_px(sheet, r, cfg);
                if h <= 0.0 { continue; }
                let y0 = row_y_px(sheet, r, cfg);
                for c in col_start..=col_end {
                    let l = if c > 0 { edge(sheet, r, c - 1, 3) } else { None };
                    let rt = edge(sheet, r, c, 2);
                    if let Some(line) = pick(l, rt) {
                        if vis_merges.iter().any(|m| m.c1 < c && c <= m.c2 && m.r1 <= r && r <= m.r2) { continue; }
                        let x = col_x_px(sheet, c, cfg);
                        segs.push((x, y0, x, y0 + h, line));
                    }
                }
            }
            // 横边:行边界 r(位于第 r-1 行与第 r 行之间),列 c
            for r in row_start..=row_end {
                let y = row_y_px(sheet, r, cfg);
                for c in col_start..col_end {
                    let w = col_width_at_px(sheet, c, cfg);
                    if w <= 0.0 { continue; }
                    let t = if r > 0 { edge(sheet, r - 1, c, 1) } else { None };
                    let b = edge(sheet, r, c, 0);
                    if let Some(line) = pick(t, b) {
                        if vis_merges.iter().any(|m| m.r1 < r && r <= m.r2 && m.c1 <= c && c <= m.c2) { continue; }
                        let x0 = col_x_px(sheet, c, cfg);
                        segs.push((x0, y, x0 + w, y, line));
                    }
                }
            }
            for (x0, y0, x1, y1, line) in segs {
                let col = line.color.as_deref().and_then(parse_hex_color).unwrap_or([0, 0, 0, 255]);
                let mut paint = tiny_skia::Paint::default();
                paint.set_color_rgba8(col[0], col[1], col[2], col[3]);
                paint.anti_alias = false;
                let stroke = tiny_skia::Stroke { width: bw(&line.style), line_cap: tiny_skia::LineCap::Square, ..Default::default() };
                let mut pb = tiny_skia::PathBuilder::new();
                pb.move_to(x0, y0);
                pb.line_to(x1, y1);
                if let Some(path) = pb.finish() {
                    pixmap.stroke_path(&path, &paint, &stroke, tx, None);
                }
            }
        }

        // 浮动图片渲染
        // Bug修复：与floating_charts/pivot同批遗漏，render_sheet_preview升级到
        // render_viewport_into时floating_images渲染逻辑没有一并迁移。
        // ROOT-051修复：此前是手写像素循环做最近邻缩放+alpha混合，完全没有
        // 旋转能力。改用tiny_skia::Pixmap::draw_pixmap，用组合变换矩阵一次性
        // 完成"缩放到目标尺寸→绕中心旋转→平移到画布目标位置"，复用
        // tile_render_helpers::draw_floating_shape已验证过的
        // "from_rotate_at角度方向与OOXML顺时针语义一致，无需取负"结论。
        // hidden的图片已在roe-parser::collect_xlsx_images解析层被continue
        // 跳过，不会出现在floating_images里；这里额外判断img.hidden只是
        // 双重保险，防止未来出现其他构造FloatingImage的路径忘记过滤。
        for img in &sheet.floating_images {
            if img.hidden { continue; }
            let img_w = img.width_pt as u32;
            let img_h = img.height_pt as u32;
            if img_w == 0 || img_h == 0 { continue; }
            use base64::Engine;
            let raw = match base64::engine::general_purpose::STANDARD.decode(
                img.data_base64.replace([' ', '\n', '\r'], "").as_bytes()
            ) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let dyn_img = match image::load_from_memory(&raw) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let rgba = dyn_img.to_rgba8();
            let src_w = rgba.width();
            let src_h = rgba.height();
            if src_w == 0 || src_h == 0 { continue; }
            // tiny_skia::Pixmap要求预乘alpha（premultiplied），from_bytes会校验，
            // 用premultiply_pixmap_from_rgba手动构造更稳妥，避免直接塞未预乘数据
            // 导致运行时panic。
            let mut src_pixmap = match tiny_skia::Pixmap::new(src_w, src_h) {
                Some(p) => p,
                None => continue,
            };
            {
                let dst = src_pixmap.data_mut();
                for (i, px) in rgba.pixels().enumerate() {
                    let a = px[3] as f32 / 255.0;
                    let base = i * 4;
                    if base + 3 >= dst.len() { break; }
                    dst[base]     = (px[0] as f32 * a) as u8;
                    dst[base + 1] = (px[1] as f32 * a) as u8;
                    dst[base + 2] = (px[2] as f32 * a) as u8;
                    dst[base + 3] = px[3];
                }
            }
            // 目标位置/尺寸（画布像素坐标系，与原手写循环的dst_x/dst_y/dst_w/dst_h含义一致）
            // ROOT-049 同源修复：x_pt/width_pt 是 pt，需乘 PT_TO_PX 与单元格网格同一坐标系
            let dst_x = (img.x_pt * PT_TO_PX - scroll_x_pt) * scale + data_px_x as f32;
            let dst_y = (img.y_pt * PT_TO_PX - scroll_y_pt) * scale + data_px_y as f32;
            let dst_w = img.width_pt * PT_TO_PX * scale;
            let dst_h = img.height_pt * PT_TO_PX * scale;
            if dst_w <= 0.0 || dst_h <= 0.0 { continue; }
            let scale_x = dst_w / src_w as f32;
            let scale_y = dst_h / src_h as f32;
            // 组合变换：先缩放到目标框大小（原点仍在左上角），再绕目标框中心
            // 旋转，最后平移到画布上的目标位置。矩阵按"从右往左应用"的
            // 顺序读（tiny_skia::Transform是行主序仿射变换，post_concat
            // 语义等价于先应用self再应用参数）。
            let transform = tiny_skia::Transform::from_scale(scale_x, scale_y)
                .post_translate(dst_x, dst_y);
            let transform = if img.rot != 0.0 {
                let cx = dst_x + dst_w / 2.0;
                let cy = dst_y + dst_h / 2.0;
                transform.post_concat(tiny_skia::Transform::from_rotate_at(img.rot, cx, cy))
            } else {
                transform
            };
            let paint = tiny_skia::PixmapPaint::default();
            pixmap.draw_pixmap(0, 0, src_pixmap.as_ref(), &paint, transform, None);
        }

        // 浮动形状渲染（xdr:sp，含文本框）
        // WROOT-XLSX-SHAPE-01: 与 floating_images 同一批视口渲染逻辑，
        // 复用 tile_render_helpers::draw_floating_shape。
        for shape in &sheet.floating_shapes {
            draw_floating_shape(
                &mut self.font_context, &mut self.layout_context, &mut pixmap,
                shape,
                scroll_x_pt, scroll_y_pt, data_px_x, data_px_y, vw, vh, scale,
            );
        }

        // 数据透视表筛选字段下拉按钮
        // ROOT-051修复：改为调用共用函数draw_field_dropdown_btn，此前手写的
        // 按钮绘制用纯scale（不含PT_TO_PX）而"(全部)"文字用scale*PT_TO_PX，
        // 两者对同一批pt坐标换算系数不一致（相差约1.333倍），导致按钮位置
        // 比正确位置偏左约1/4按钮宽度对应的像素量，与文字重叠。
        // 同时补上此前缺失的"已筛选"状态判断（此前"(全部)"文字硬编码固定
        // 显示，即使selected_values非空也不反映筛选状态）。
        if let Some(pivot) = &sheet.pivot {
            for (fi, _ff) in pivot.filter_fields.iter().enumerate() {
                let cell_row = fi as u32; // 筛选字段固定从sheet第0行开始往下排，与base_row无关
                let cell_col = pivot.base_col + 1; // 筛选值所在列（角落列右边一列）
                if cell_row < row_start || cell_row >= row_end { continue; }
                if cell_col < col_start || cell_col >= col_end { continue; }
                let cw = col_width_at(sheet, cell_col, cfg);
                let ch = row_height_at(sheet, cell_row, cfg);
                let cx = col_x(sheet, cell_col, cfg);
                let cy = row_y(sheet, cell_row, cfg);
                let btn_margin = 1.0_f32;
                let btn_size = (ch - btn_margin * 2.0).min(cw - btn_margin * 2.0).max(4.0_f32).min(16.0_f32);

                let is_filtered = !_ff.selected_values.is_empty();
                draw_field_dropdown_btn(&mut pixmap, sheet, cfg, tx, cell_col, cell_row, is_filtered);

                // 字段名文字（画在按钮列左边一列）
                let name_col = pivot.base_col;
                if name_col >= col_start && name_col < col_end {
                    let name_cx = col_x(sheet, name_col, cfg);
                    let name_cy = row_y(sheet, cell_row, cfg);
                    let name_w = col_width_at(sheet, name_col, cfg);
                    let name_h = row_height_at(sheet, cell_row, cfg);
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        &_ff.name, name_cx, name_cy, name_w, name_h,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                        10.0,
                        "Carlito",
                        "", "", name_w,
                    );
                }

                // 筛选值文字：未筛选显示"(全部)"，筛选单值显示该值，多值显示"(多项)"
                let value_label = if _ff.selected_values.is_empty() {
                    "(全部)".to_string()
                } else if _ff.selected_values.len() == 1 {
                    _ff.selected_values[0].clone()
                } else {
                    "(多项)".to_string()
                };
                let label_avail_w = (cw - btn_size - btn_margin * 2.0).max(0.0);
                if label_avail_w > 0.0 {
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        &value_label, cx, cy, label_avail_w, ch,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x40, 0x40, 0x40], false, false,
                        10.0,
                        "Carlito",
                        "", "", label_avail_w,
                    );
                }
            }
        }

        // 透视表多级列/行表头渲染（阶段一：背景+文字；阶段二：边框）
        if let Some(pivot) = &sheet.pivot {
            let mut pivot = pivot.clone();
            pivot.compute_spans();

            // 颜色常量
            // v151更正：此前注释"临时透明，排查坐标问题"已过时——不加背景色是既定设计决定，非待修bug。
            let header_bg   = tiny_skia::Color::from_rgba8(0xff, 0xff, 0xff, 0x00); // 有意透明，无背景色设计
            let subtotal_bg = tiny_skia::Color::from_rgba8(0xff, 0xff, 0xff, 0x00); // 有意透明，无背景色设计
            let border_col  = tiny_skia::Color::from_rgba8(0x9b, 0xb0, 0xd4, 0xff); // 表头边框

            // 坐标辅助闭包：物理格子 (sheet_row, sheet_col) → 屏幕像素 (px_x, px_y, px_w, px_h)
            let cell_rect_px = |sr: u32, sc: u32| -> Option<(f32, f32, f32, f32)> {
                let cx = col_x_px(sheet, sc, cfg);
                let cy = row_y_px(sheet, sr, cfg);
                let cw = col_width_at_px(sheet, sc, cfg);
                let ch = row_height_at_px(sheet, sr, cfg);
                let px = (cx - scroll_x_pt) * scale + header_col_w_pt * scale;
                let py = (cy - scroll_y_pt) * scale + header_row_h_pt * scale;
                Some((px, py, cw * scale, ch * scale))
            };

            // 透视表数据区数值查询用：叶子节点物理偏移 -> 完整路径（从根到叶子的value序列）
            let mut col_leaf_paths: std::collections::HashMap<u32, Vec<String>> = std::collections::HashMap::new();
            let mut row_leaf_paths: std::collections::HashMap<u32, Vec<String>> = std::collections::HashMap::new();
            let mut pivot_data_matrix_drawn: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();

            // ── 阶段一：列表头（横向，从 data_row_start 向上，每层一行）──
            // col_tree.children 是外层字段各取值；每取值节点 cached_span = 横跨列数
            // 物理行：filter_fields.len() + 层级索引（0=外层）
            // 物理列：data_col_start + 累计偏移
            {
                // BFS 逐层渲染 col_tree
                // 用栈记录：(节点引用, 层级depth, 起始物理列)
                struct ColItem<'a> {
                    node: &'a roe_model::calc::PivotHeaderNode,
                    depth: u32,
                    col_offset: u32, // 相对 data_col_start 的列偏移
                    path: Vec<String>,
                }
                let mut stack: Vec<ColItem> = Vec::new();
                let mut col_cursor = 0u32;
                for child in &pivot.col_tree.children {
                    let seg = if child.is_subtotal { String::new() } else { child.value.clone() };
                    stack.push(ColItem { node: child, depth: 0, col_offset: col_cursor, path: vec![seg] });
                    col_cursor += child.cached_span;
                }
                while let Some(item) = stack.pop() {
                    if item.node.children.is_empty() {
                        col_leaf_paths.insert(item.col_offset, item.path.clone());
                    }
                    let sheet_row = pivot.base_row + pivot.filter_fields.len() as u32 + 1 + item.depth; // +1 空行
                    let sheet_col_start = pivot.base_col + pivot.data_col_start + item.col_offset;
                    // 跨 cached_span 列合并：只画第一列宽度 * span
                    if let Some((px, py, _, ph)) = cell_rect_px(sheet_row, sheet_col_start) {
                        // 计算合并后总宽
                        let total_w: f32 = (0..item.node.cached_span)
                            .map(|k| col_width_at(sheet, sheet_col_start + k, cfg) * scale)
                            .sum();
                        // 背景
                        let bg = if item.node.is_subtotal { subtotal_bg } else { header_bg };
                        let mut bg_paint = tiny_skia::Paint::default();
                        bg_paint.set_color(bg);
                        if let Some(rect) = tiny_skia::Rect::from_xywh(px + 1.0, py + 1.0, (total_w - 2.0).max(1.0), (ph - 2.0).max(1.0)) {
                            pixmap.fill_rect(rect, &bg_paint, tiny_skia::Transform::identity(), None);
                        }
                        // 边框（外框）
                        // 文字（非小计节点才显示）
                        if !item.node.value.is_empty() {
                            let text_cell_x = col_x(sheet, sheet_col_start, cfg);
                            let text_cell_y = row_y(sheet, sheet_row, cfg);
                            let text_avail_w: f32 = (0..item.node.cached_span)
                                .map(|k| col_width_at(sheet, sheet_col_start + k, cfg))
                                .sum();
                            let text_cell_h = row_height_at(sheet, sheet_row, cfg);
                            draw_cell_text(
                                &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                &item.node.value, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                                scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                                10.0,
                                "Carlito",
                                "", "", text_avail_w,
                            );
                        }
                    }
                    // 子节点入栈
                    // v152：小计节点固定借用父节点自己的 depth（item.depth，不 +1），
                    // 使其绘制列位置与父节点对齐（如"红Total"与"红"同一行），而非
                    // 落入子节点应处的下一层列位置。此规则在任意层级下语义都成立
                    // （"小计代表父节点自身的一个补充数据点"），但当前小计插入逻辑
                    // （pivot_v2_insert_col_subtotals）仅处理最外层（tree.children），
                    // 故此处实际只会在 item.depth==0 时被小计节点命中。若未来扩展为
                    // 多层小计（技术债，见 v152 交接记录），此行代码不需要改动。
                    let mut child_cursor = item.col_offset;
                    for child in &item.node.children {
                        let mut child_path = item.path.clone();
                        let seg = if child.is_subtotal { String::new() } else { child.value.clone() };
                        child_path.push(seg);
                        let child_depth = if child.is_subtotal { item.depth } else { item.depth + 1 };
                        stack.push(ColItem { node: child, depth: child_depth, col_offset: child_cursor, path: child_path });
                        child_cursor += child.cached_span;
                    }
                }
            }

            // ── 阶段一：行表头（纵向，从 data_col_start 向左，每层一列）──
            // 支持两种模式：Outline（父节点独占一行，子节点另起一行）
            // 和 Tabular（父节点与子节点共享纵向空间，原有逻辑不变）。
            {
                let is_outline = pivot.layout_mode == roe_model::calc::PivotLayoutMode::Outline;

                struct RowItem<'a> {
                    node: &'a roe_model::calc::PivotHeaderNode,
                    depth: u32,
                    row_offset: u32,
                    // Outline 模式：父节点自身标题行画完后，子节点从此行开始（父行之后另起一行）
                    // Tabular 模式：此值与 row_offset 相同（子节点与父节点共享起始行）
                    self_row_span: u32,
                    path: Vec<String>,
                }
                let mut stack: Vec<RowItem> = Vec::new();
                let mut row_cursor = 0u32;
                for child in &pivot.row_tree.children {
                    let span = if is_outline { child.outline_span } else { child.cached_span };
                    let seg = if child.is_subtotal { String::new() } else { child.value.clone() };
                    stack.push(RowItem { node: child, depth: 0, row_offset: row_cursor, self_row_span: span, path: vec![seg] });
                    row_cursor += span;
                }
                while let Some(item) = stack.pop() {
                    // ROOT-063: 此前只有叶子节点（children.is_empty()）才收集查值路径，
                    // 导致中间层父节点（如2层row_fields场景下的外层字段值那一行）永远
                    // 查不到数值、渲染空白。根因是 xlsx rowItems 里每一条 <i> 记录不论
                    // 声明路径长度是否达到最深层，都对应一个独立、需要显示自身聚合值的
                    // 物理行（不是"小计"语义，是源数据自己声明的数据行，与叶子行平级）。
                    // 修复：不论是否为叶子，每个节点都收集一份自身路径→row_offset的
                    // 查值映射。数据引擎层（compute_pivot_data_matrix）已经为这些中间层
                    // 路径准备好了聚合值（前提：col_fields为空场景，见该函数的ROOT-063
                    // 注释；col_fields非空场景经xlsx静态缓存值验证，中间层本来就不显示
                    // 数值，此处依然照常收集路径去查，若矩阵里没有对应key，
                    // pivot.data.get()会返回None，渲染层自然跳过不画，行为保持正确）。
                    row_leaf_paths.insert(item.row_offset, item.path.clone());
                    let sheet_col = pivot.base_col + item.depth; // 行表头：第0层在第0列，第1层在第1列...
                    let sheet_row_start = pivot.base_row + pivot.data_row_start + item.row_offset;
                    // 该节点自身标题格占用的物理行数：
                    // Outline 模式下有子节点的父节点只占1行（标题独立），叶节点占1行；
                    // Tabular 模式下沿用 cached_span（父子共享空间，原有效果）。
                    let own_rows: u32 = if is_outline {
                        1
                    } else {
                        item.node.cached_span
                    };
                    if let Some((px, py, pw, _)) = cell_rect_px(sheet_row_start, sheet_col) {
                        let total_h: f32 = (0..own_rows)
                            .map(|k| row_height_at(sheet, sheet_row_start + k, cfg) * scale)
                            .sum();
                        let bg = if item.node.is_subtotal { subtotal_bg } else { header_bg };
                        let mut bg_paint = tiny_skia::Paint::default();
                        bg_paint.set_color(bg);
                        if let Some(rect) = tiny_skia::Rect::from_xywh(px + 1.0, py + 1.0, (pw - 2.0).max(1.0), (total_h - 2.0).max(1.0)) {
                            pixmap.fill_rect(rect, &bg_paint, tiny_skia::Transform::identity(), None);
                        }
                        // ROOT-062: value 为空字符串且非小计节点，代表源数据是"空白/missing"
                        // （xlsx <m/> 项）。value 本身必须保持空字符串不变（PivotDataMatrix 的
                        // row_path/col_path key 依赖它与 roe-polars-bridge 里 null 值转换结果
                        // 逐字一致，见 pivot.rs 的 ROOT-062 注释），这里仅在绘制文字时替换显示
                        // 文案为"(空白)"，不回写 item.node.value，不影响后续数据查值路径。
                        let display_text: &str = if item.node.value.is_empty() && !item.node.is_subtotal {
                            "(空白)"
                        } else {
                            &item.node.value
                        };
                        if !display_text.is_empty() {
                            let text_cell_x = col_x(sheet, sheet_col, cfg);
                            let text_cell_y = row_y(sheet, sheet_row_start, cfg);
                            let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                            let text_cell_h: f32 = (0..own_rows)
                                .map(|k| row_height_at(sheet, sheet_row_start + k, cfg))
                                .sum();
                            draw_cell_text(
                                &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                display_text, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                                scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                                10.0,
                                "Carlito",
                                "", "", text_avail_w,
                            );
                        }
                    }
                    // 子节点纵向起始偏移：
                    // Outline 模式：父节点独占的 own_rows 行之后，子节点另起一行开始排列
                    // Tabular 模式：子节点与父节点共享同一起始行（row_offset 不变）
                    let children_start_offset = if is_outline { item.row_offset + own_rows } else { item.row_offset };
                    let mut child_cursor = children_start_offset;
                    // v152：小计节点固定借用父节点自己的 depth（item.depth，不 +1），
                    // 使其绘制列位置（行表头是纵向树，depth对应物理列）与父节点对齐
                    // （如"服装Total"落在"服装"所在的A列，而非"普通/高端"所在的B列）。
                    // 理由同列头处的对称改动，见上方 ColItem 分支注释。
                    for child in &item.node.children {
                        let span = if is_outline { child.outline_span } else { child.cached_span };
                        let mut child_path = item.path.clone();
                        let seg = if child.is_subtotal { String::new() } else { child.value.clone() };
                        child_path.push(seg);
                        let child_depth = if child.is_subtotal { item.depth } else { item.depth + 1 };
                        stack.push(RowItem { node: child, depth: child_depth, row_offset: child_cursor, self_row_span: span, path: child_path });
                        child_cursor += span;
                    }
                }
            }

            // ── 阶段一.八：数据区数值渲染（PivotDataMatrix 查值，替换旧快照方案）──
            let empty_path: Vec<String> = Vec::new();
            let col_iter: Vec<(u32, &Vec<String>)> = if col_leaf_paths.is_empty() {
                vec![(0u32, &empty_path)]
            } else {
                col_leaf_paths.iter().map(|(k, v)| (*k, v)).collect()
            };
            let row_iter: Vec<(u32, &Vec<String>)> = if row_leaf_paths.is_empty() {
                vec![(0u32, &empty_path)]
            } else {
                row_leaf_paths.iter().map(|(k, v)| (*k, v)).collect()
            };
            for &(col_offset, col_path) in &col_iter {
                for &(row_offset, row_path) in &row_iter {
                    for value_idx in 0..pivot.value_fields.len().max(1) {
                        if let Some(val) = pivot.data.get(row_path, col_path, value_idx) {
                            // ROOT-060: dataOnRows=true 场景下，多个 value_fields 纵向排列
                            // （Sum of C 一行、Sum of D 一行），value_idx 加到行偏移；
                            // dataOnRows=false（原有逻辑不变）：value_idx 加到列偏移，
                            // 多个 value_fields 横向紧挨排列。
                            let (sheet_row, sheet_col) = if pivot.data_on_rows {
                                (
                                    pivot.base_row + pivot.data_row_start + row_offset + value_idx as u32,
                                    pivot.base_col + pivot.data_col_start + col_offset,
                                )
                            } else {
                                (
                                    pivot.base_row + pivot.data_row_start + row_offset,
                                    pivot.base_col + pivot.data_col_start + col_offset + value_idx as u32,
                                )
                            };
                            if pivot_data_matrix_drawn.contains(&(sheet_row, sheet_col)) {
                                continue;
                            }
                            if let Some((_px, _py, _pw, _ph)) = cell_rect_px(sheet_row, sheet_col) {
                                let text_cell_x = col_x(sheet, sheet_col, cfg);
                                let text_cell_y = row_y(sheet, sheet_row, cfg);
                                let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                                let text_cell_h = row_height_at(sheet, sheet_row, cfg);
                                let text = format!("{:.2}", val);
                                draw_cell_text(
                                    &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                    &text, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                                    scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                                    10.0,
                                    "Carlito",
                                    "", "", text_avail_w,
                                );
                                pivot_data_matrix_drawn.insert((sheet_row, sheet_col));
                            }
                        }
                    }
                }
            }

            // ── 阶段一.九：RowTotal（最右新增一列）/ ColTotal（最下新增一行）──
            // 规则（已与用户确认）：
            //   有col字段（不管有没有row字段） → 显示竖向总计（ColTotal，最下一行），受 row_grand_totals 控制
            //   有row字段（不管有没有col字段） → 显示横向总计（RowTotal，最右一列），受 col_grand_totals 控制
            //   两者都有时，右下角额外显示 GrandTotal 单格
            //   多值字段：横向总计每个值字段各显示一个值；竖向总计只显示一个值（不循环value_idx）
            {
                let empty_p: Vec<String> = Vec::new();
                let has_row = !pivot.row_field_names.is_empty();
                let has_col = !pivot.col_field_names.is_empty();
                let n_values = pivot.value_fields.len().max(1) as u32;

                // ColTotal所在行 / RowTotal所在列：用 row_leaf_paths/col_leaf_paths 的
                // offset 累加逻辑现算，不依赖 pivot.last_row/last_col。
                let grand_total_row: u32 = {
                    let max_row_offset = row_leaf_paths.keys().max().copied().unwrap_or(0);
                    pivot.base_row + pivot.data_row_start + max_row_offset + 1
                };
                let grand_total_col: u32 = {
                    let max_col_offset = col_leaf_paths.keys().max().copied().unwrap_or(0);
                    pivot.base_col + pivot.data_col_start + max_col_offset + n_values
                };

                let total_label = "总计";

                // ── 横向总计（RowTotal，最右一列）：有col字段时显示（数据横向罗列），受 col_grand_totals 控制 ──
                if has_col && pivot.col_grand_totals {
                    // 标题：画在列表头顶层那一行、grand_total_col 那一列
                    let label_row = pivot.base_row + pivot.filter_fields.len() as u32 + 1;
                    for value_idx in 0..n_values {
                        let sheet_col = grand_total_col + value_idx;
                        if let Some((px, py, pw, ph)) = cell_rect_px(label_row, sheet_col) {
                            let text_cell_x = col_x(sheet, sheet_col, cfg);
                            let text_cell_y = row_y(sheet, label_row, cfg);
                            let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                            let text_cell_h = row_height_at(sheet, label_row, cfg);
                            let label_text = if n_values > 1 {
                                format!("{}-{}", total_label, pivot.value_fields[value_idx as usize].name)
                            } else {
                                total_label.to_string()
                            };
                            draw_cell_text(
                                &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                &label_text, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                                scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                                10.0,
                                "Carlito",
                                "", "", text_avail_w,
                            );
                        }
                    }
                    // 数值：每个row叶子一行，多值字段则横向平铺 n_values 个格子
                    // row_leaf_paths 为空（无row字段）时，用单个空路径代表整表
                    let row_iter_for_total: Vec<(u32, &Vec<String>)> = if row_leaf_paths.is_empty() {
                        vec![(0u32, &empty_p)]
                    } else {
                        row_leaf_paths.iter().map(|(k, v)| (*k, v)).collect()
                    };
                    for (row_offset, row_path) in row_iter_for_total {
                        for value_idx in 0..n_values {
                            if let Some(val) = pivot.data.get(row_path, &empty_p, value_idx as usize) {
                                let sheet_row = pivot.base_row + pivot.data_row_start + row_offset;
                                let sheet_col = grand_total_col + value_idx;
                                if pivot_data_matrix_drawn.contains(&(sheet_row, sheet_col)) { continue; }
                                if let Some((_px, _py, _pw, _ph)) = cell_rect_px(sheet_row, sheet_col) {
                                    let text_cell_x = col_x(sheet, sheet_col, cfg);
                                    let text_cell_y = row_y(sheet, sheet_row, cfg);
                                    let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                                    let text_cell_h = row_height_at(sheet, sheet_row, cfg);
                                    let text = format!("{:.2}", val);
                                    draw_cell_text(
                                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                        &text, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                                        10.0,
                                        "Carlito",
                                        "", "", text_avail_w,
                                    );
                                    pivot_data_matrix_drawn.insert((sheet_row, sheet_col));
                                }
                            }
                        }
                    }
                }

                // ── 竖向总计（ColTotal，最下一行）：有row字段时显示（数据纵向罗列），受 row_grand_totals 控制 ──
                if has_row && pivot.row_grand_totals {
                    // 标题：画在 grand_total_row 那一行、行表头第0列（base_col）
                    if let Some((px, py, pw, ph)) = cell_rect_px(grand_total_row, pivot.base_col) {
                        let text_cell_x = col_x(sheet, pivot.base_col, cfg);
                        let text_cell_y = row_y(sheet, grand_total_row, cfg);
                        let text_avail_w = col_width_at(sheet, pivot.base_col, cfg);
                        let text_cell_h = row_height_at(sheet, grand_total_row, cfg);
                        draw_cell_text(
                            &mut self.font_context, &mut self.layout_context, &mut pixmap,
                            total_label, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                            scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                            10.0,
                            "Carlito",
                            "", "", text_avail_w,
                        );
                    }
                    // 数值：每个col叶子一列，多值字段时该叶子对应的n_values个物理列各自
                    // 取对应value_idx的总计值（col_offset已在compute_spans阶段乘过n_values，
                    // 因此叶子内第k个物理列 = col_offset + k，对应value_idx = k）
                    // col_leaf_paths 为空（无col字段）时，用单个空路径代表整表，画在数据区正下方一格
                    let col_iter_for_total: Vec<(u32, &Vec<String>)> = if col_leaf_paths.is_empty() {
                        vec![(0u32, &empty_p)]
                    } else {
                        col_leaf_paths.iter().map(|(k, v)| (*k, v)).collect()
                    };
                    for (col_offset, col_path) in col_iter_for_total {
                        for value_idx in 0..n_values {
                            if let Some(val) = pivot.data.get(&empty_p, col_path, value_idx as usize) {
                                let sheet_row = grand_total_row;
                                let sheet_col = pivot.base_col + pivot.data_col_start + col_offset + value_idx;
                                if pivot_data_matrix_drawn.contains(&(sheet_row, sheet_col)) { continue; }
                                if let Some((_px, _py, _pw, _ph)) = cell_rect_px(sheet_row, sheet_col) {
                                    let text_cell_x = col_x(sheet, sheet_col, cfg);
                                    let text_cell_y = row_y(sheet, sheet_row, cfg);
                                    let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                                    let text_cell_h = row_height_at(sheet, sheet_row, cfg);
                                    let text = format!("{:.2}", val);
                                    draw_cell_text(
                                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                        &text, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                                        10.0,
                                        "Carlito",
                                        "", "", text_avail_w,
                                    );
                                    pivot_data_matrix_drawn.insert((sheet_row, sheet_col));
                                }
                            }
                        }
                    }
                }

                // ── GrandTotal 单格：只有 row 和 col 都存在时才有意义 ──
                // ROOT-065: 此前只判断 has_row && has_col，未检查 row_grand_totals /
                // col_grand_totals 开关，导致 xlsx 显式关闭总计（rowGrandTotals="0"
                // colGrandTotals="0"，如 case7）时，右下角仍会画出一个不该存在的
                // GrandTotal 数字（因为该值本身在 PivotDataMatrix 里是存在的，只是
                // 不该被渲染出来）。修复：GrandTotal 单格需要 row_grand_totals 和
                // col_grand_totals 都为 true 才画（与横向/纵向总计各自的开关判断
                // 逻辑保持一致，任一开关关闭都不显示交叉总计格）。
                if has_row && has_col && pivot.row_grand_totals && pivot.col_grand_totals {
                    for value_idx in 0..n_values {
                        if let Some(val) = pivot.data.get(&empty_p, &empty_p, value_idx as usize) {
                            let sheet_row = grand_total_row;
                            let sheet_col = grand_total_col + value_idx;
                            if !pivot_data_matrix_drawn.contains(&(sheet_row, sheet_col)) {
                                if let Some((_px, _py, _pw, _ph)) = cell_rect_px(sheet_row, sheet_col) {
                                    let text_cell_x = col_x(sheet, sheet_col, cfg);
                                    let text_cell_y = row_y(sheet, sheet_row, cfg);
                                    let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                                    let text_cell_h = row_height_at(sheet, sheet_row, cfg);
                                    let text = format!("{:.2}", val);
                                    draw_cell_text(
                                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                                        &text, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20, 0x20, 0x20], false, false,
                                        10.0,
                                        "Carlito",
                                        "", "", text_avail_w,
                                    );
                                    pivot_data_matrix_drawn.insert((sheet_row, sheet_col));
                                }
                            }
                        }
                    }
                }
            }
        }

        // ── 阶段一.五：角落区标签（行字段名 / 列字段名，均横向排列）──
        // 行字段名：角落矩形最后一行（紧邻数据区上方，第 base_row+data_row_start-1 行），
        //           从第 base_col 列起横向依次排列，每个字段占一格。
        // 列字段名：角落矩形第一行（即 base_row 行），
        //           从第 base_col+data_col_start 列起横向依次排列，每个字段占一格。
        // 值字段名（角落格/"值"标签/纯列表头场景）暂不在此处理，留待值字段渲染阶段。
        if let Some(pivot) = &sheet.pivot {
            let corner_label_paint = [0x20u8, 0x20, 0x20];

            // ROOT-005修复(v126)：改为调用提取出的顶层函数draw_field_dropdown_btn
            let draw_field_dropdown_btn = |pixmap: &mut tiny_skia::PixmapMut, sheet_col: u32, label_row: u32| {
                // pivot字段按钮暂不区分"已筛选"状态，固定传false（保持原有倒三角样式）
                crate::tile::draw_field_dropdown_btn(pixmap, sheet, cfg, tx, sheet_col, label_row, false);
            };

            if pivot.data_row_start > 0 {
                let label_row = pivot.base_row + pivot.data_row_start - 1;
                for (i, name) in pivot.row_field_names.iter().enumerate() {
                    let sheet_col = pivot.base_col + i as u32;
                    if sheet_col >= pivot.base_col + pivot.data_col_start { break; }
                    let text_cell_x = col_x(sheet, sheet_col, cfg);
                    let text_cell_y = row_y(sheet, label_row, cfg);
                    let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                    let text_cell_h = row_height_at(sheet, label_row, cfg);
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        name, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,corner_label_paint, false, false,
                        10.0,
                        "Carlito",
                        "", "", text_avail_w,
                    );
                    draw_field_dropdown_btn(&mut pixmap, sheet_col, label_row);
                }
            }

            if pivot.data_col_start > 0 {
                let label_row = pivot.base_row + pivot.filter_fields.len() as u32;
                for (i, name) in pivot.col_field_names.iter().enumerate() {
                    let sheet_col = pivot.base_col + pivot.data_col_start + i as u32;
                    let text_cell_x = col_x(sheet, sheet_col, cfg);
                    let text_cell_y = row_y(sheet, label_row, cfg);
                    let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                    let text_cell_h = row_height_at(sheet, label_row, cfg);
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        name, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,corner_label_paint, false, false,
                        10.0,
                        "Carlito",
                        "", "", text_avail_w,
                    );
                    draw_field_dropdown_btn(&mut pixmap, sheet_col, label_row);
                }
                // "值"标题：多值字段场景下，紧跟在最后一个col_field_name右边同一行
                // ROOT-060: dataOnRows=true 时"值/Values"标题改到新增的角落列专用逻辑处理
                // （与 col_tree 表头同一行、纵向对齐 Sum of C/D，而不是横向紧跟col字段名）。
                // ROOT-061: 无 col_fields 时不画"值"标题这一行（与 col_header_rows 公式保持一致）。
                if pivot.value_fields.len() > 1 && !pivot.data_on_rows && !pivot.col_field_names.is_empty() {
                    let sheet_col = pivot.base_col + pivot.data_col_start + pivot.col_field_names.len() as u32;
                    let text_cell_x = col_x(sheet, sheet_col, cfg);
                    let text_cell_y = row_y(sheet, label_row, cfg);
                    let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                    let text_cell_h = row_height_at(sheet, label_row, cfg);
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        "值", text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,corner_label_paint, false, false,
                        10.0,
                        "Carlito",
                        "", "", text_avail_w,
                    );
                }
            }
        }

        // ── 阶段一.五.五：dataOnRows专用表头（"值/Values"标题 + 值字段名纵向标签）──
        // 场景：pivot.data_on_rows == true（值字段展开在行方向，如"Sum of C"/"Sum of D"
        // 各占数据区一行）。角落列（base_col）需要：
        //   - 与 col_tree 表头同一行（base_row+filter_fields.len()+1）写"值"/"Values"标题；
        //   - 数据区每一行（base_row+data_row_start+value_idx）写对应 value_fields[value_idx].name。
        // 与阶段一.五"值"标题分支（横向排列，dataOnCols场景）互斥，见上方 !pivot.data_on_rows 条件。
        if let Some(pivot) = &sheet.pivot {
            if pivot.data_on_rows {
                let corner_label_paint = [0x20u8, 0x20, 0x20];
                let label_col = pivot.base_col;

                let values_title_row = pivot.base_row + pivot.filter_fields.len() as u32 + 1;
                {
                    let text_cell_x = col_x(sheet, label_col, cfg);
                    let text_cell_y = row_y(sheet, values_title_row, cfg);
                    let text_avail_w = col_width_at(sheet, label_col, cfg);
                    let text_cell_h = row_height_at(sheet, values_title_row, cfg);
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        "值", text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale, corner_label_paint, false, false,
                        10.0,
                        "Carlito",
                        "", "", text_avail_w,
                    );
                }

                for (value_idx, vf) in pivot.value_fields.iter().enumerate() {
                    let label_row = pivot.base_row + pivot.data_row_start + value_idx as u32;
                    let text_cell_x = col_x(sheet, label_col, cfg);
                    let text_cell_y = row_y(sheet, label_row, cfg);
                    let text_avail_w = col_width_at(sheet, label_col, cfg);
                    let text_cell_h = row_height_at(sheet, label_row, cfg);
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        &vf.name, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale, corner_label_paint, false, false,
                        10.0,
                        "Carlito",
                        "", "", text_avail_w,
                    );
                }
            }
        }

        // ── 阶段一.六：值字段名标签（单值字段场景）──
        // 场景：无行字段、有列字段、单值字段 → 值字段名显示在数据区第一行的行标题位置
        //       （即 base_row+data_row_start 行、base_col 列，与行字段名占同一类位置）。
        // 其余值字段名场景（1row1col角落格 / 多值字段"值"标签行）留待下一阶段处理。
        if let Some(pivot) = &sheet.pivot {
            if !pivot.row_field_names.is_empty() && !pivot.col_field_names.is_empty() && pivot.value_fields.len() == 1 {
                let label_row = pivot.base_row + pivot.filter_fields.len() as u32;
                let label_col = pivot.base_col;
                let name = &pivot.value_fields[0].name;
                let text_cell_x = col_x(sheet, label_col, cfg);
                let text_cell_y = row_y(sheet, label_row, cfg);
                let text_avail_w = col_width_at(sheet, label_col, cfg);
                let text_cell_h = row_height_at(sheet, label_row, cfg);
                draw_cell_text(
                    &mut self.font_context, &mut self.layout_context, &mut pixmap,
                    name, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                    scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20u8, 0x20, 0x20], false, false,
                    10.0,
                    "Carlito",
                    "", "", text_avail_w,
                );
            }
            if pivot.row_field_names.is_empty() && !pivot.col_field_names.is_empty() && pivot.value_fields.len() == 1 {
                let label_row = pivot.base_row + pivot.data_row_start;
                let label_col = pivot.base_col;
                let name = &pivot.value_fields[0].name;
                let text_cell_x = col_x(sheet, label_col, cfg);
                let text_cell_y = row_y(sheet, label_row, cfg);
                let text_avail_w = col_width_at(sheet, label_col, cfg);
                let text_cell_h = row_height_at(sheet, label_row, cfg);
                draw_cell_text(
                    &mut self.font_context, &mut self.layout_context, &mut pixmap,
                    name, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                    scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20u8, 0x20, 0x20], false, false,
                    10.0,
                    "Carlito",
                    "", "", text_avail_w,
                );
            }
        }

        // ── 阶段一.六.五：值字段名标签（场景C：纯row，无col）──
        // 场景：有行字段、无列字段、值字段任意数量 → 值字段名紧跟在行字段名后面，
        // 与行字段名共享同一行（base_row+data_row_start-1），从 data_col_start 列起
        // 循环排列（1个值字段则只显示1次，多个则循环，逻辑与场景D一致）。
        if let Some(pivot) = &sheet.pivot {
            if !pivot.row_field_names.is_empty() && pivot.col_field_names.is_empty() && !pivot.value_fields.is_empty() && !pivot.data_on_rows {
                let label_row = pivot.base_row + pivot.data_row_start - 1;
                let n_data_cols: u32 = pivot.col_tree.children.iter().map(|c| c.cached_span).sum::<u32>().max(pivot.value_fields.len() as u32);
                let n_values = pivot.value_fields.len() as u32;
                for i in 0..n_data_cols {
                    let sheet_col = pivot.base_col + pivot.data_col_start + i;
                    let vf_idx = (i % n_values) as usize;
                    let name = &pivot.value_fields[vf_idx].name;
                    let text_cell_x = col_x(sheet, sheet_col, cfg);
                    let text_cell_y = row_y(sheet, label_row, cfg);
                    let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                    let text_cell_h = row_height_at(sheet, label_row, cfg);
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        name, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20u8, 0x20, 0x20], false, false,
                        10.0,
                        "Carlito",
                        "", "", text_avail_w,
                    );
                }
            }
        }

        // ── 阶段一.七：多值字段标签（数据区第一行显示值字段名循环）──
        // 场景：value_fields.len() >= 2 时，数据区第一行每列显示对应值字段名，
        // 按 col_tree 顶层跨度（不含总计列）循环排列。"值"标签本身暂不处理。
        if let Some(pivot) = &sheet.pivot {
            if pivot.value_fields.len() > 1 && !pivot.data_on_rows {
                let n_data_cols: u32 = pivot.col_tree.children.iter().map(|c| c.cached_span).sum();
                let value_label_row = pivot.base_row + pivot.data_row_start - 1;
                let n_values = pivot.value_fields.len() as u32;
                for i in 0..n_data_cols {
                    let sheet_col = pivot.base_col + pivot.data_col_start + i;
                    let vf_idx = (i % n_values) as usize;
                    let name = &pivot.value_fields[vf_idx].name;
                    let text_cell_x = col_x(sheet, sheet_col, cfg);
                    let text_cell_y = row_y(sheet, value_label_row, cfg);
                    let text_avail_w = col_width_at(sheet, sheet_col, cfg);
                    let text_cell_h = row_height_at(sheet, value_label_row, cfg);
                    draw_cell_text(
                        &mut self.font_context, &mut self.layout_context, &mut pixmap,
                        name, text_cell_x, text_cell_y, text_avail_w, text_cell_h,
                        scroll_x_pt / PT_TO_PX, scroll_y_pt / PT_TO_PX, header_col_w_pt * scale, header_row_h_pt * scale, scale * PT_TO_PX, scale,[0x20u8, 0x20, 0x20], false, false,
                        10.0,
                        "Carlito",
                        "", "", text_avail_w,
                    );
                }
            }
        }

        // 浮动图表渲染（ChartModel -> SVG -> rasterize -> blit）
        // Bug修复：Calc从旧render_sheet_preview升级到render_viewport_into时，
        // floating_charts渲染逻辑没有一并迁移，导致viewport架构下图表完全不显示。
        // 统一的 RGBA 浮动对象 blit:普通图与统计图共用,避免两份合成逻辑
        let blit_floating = |pixmap: &mut tiny_skia::PixmapMut, rgba: &[u8], dst_x: i32, dst_y: i32, dst_w: u32, dst_h: u32| {
            let buf = pixmap.data_mut();
            for dy in 0..dst_h as usize {
                for dx in 0..dst_w as usize {
                    let px = dst_x + dx as i32;
                    let py = dst_y + dy as i32;
                    if px < data_px_x || py < data_px_y || px >= vw as i32 || py >= vh as i32 { continue; }
                    let idx_src = (dy * dst_w as usize + dx) * 4;
                    if idx_src + 3 >= rgba.len() { continue; }
                    let idx = (py as usize * vw as usize + px as usize) * 4;
                    if idx + 3 < buf.len() {
                        let a = rgba[idx_src + 3] as f32 / 255.0;
                        buf[idx]     = (rgba[idx_src]     as f32 * a + buf[idx]     as f32 * (1.0 - a)) as u8;
                        buf[idx + 1] = (rgba[idx_src + 1] as f32 * a + buf[idx + 1] as f32 * (1.0 - a)) as u8;
                        buf[idx + 2] = (rgba[idx_src + 2] as f32 * a + buf[idx + 2] as f32 * (1.0 - a)) as u8;
                        buf[idx + 3] = 255;
                    }
                }
            }
        };
        for chart in &sheet.floating_charts {
            // ROOT-049 同源修复：chart.x_pt/width_pt 是 pt，单元格网格按 pt*PT_TO_PX 排布，
            // 浮动对象必须同样换算，否则图表落点偏左上、尺寸偏小 25%。
            // 直接按目标物理像素尺寸渲染，避免低分辨率渲染后最近邻放大。
            let dst_x = ((chart.x_pt * PT_TO_PX - scroll_x_pt) * scale).round() as i32 + data_px_x;
            let dst_y = ((chart.y_pt * PT_TO_PX - scroll_y_pt) * scale).round() as i32 + data_px_y;
            let dst_w = (chart.width_pt * PT_TO_PX * scale).round() as u32;
            let dst_h = (chart.height_pt * PT_TO_PX * scale).round() as u32;
            if dst_w == 0 || dst_h == 0 { continue; }
            let rgba = match crate::chart_render::render_chart_to_rgba_scaled(chart, dst_w, dst_h, scale) {
                Some(r) => r,
                None => continue,
            };
            blit_floating(&mut pixmap, &rgba, dst_x, dst_y, dst_w, dst_h);
        }
        // 统计图(chartEx):与普通图同一套坐标换算,叠在普通图之上
        for sc in &sheet.floating_stat_charts {
            let dst_x = ((sc.x_pt * PT_TO_PX - scroll_x_pt) * scale).round() as i32 + data_px_x;
            let dst_y = ((sc.y_pt * PT_TO_PX - scroll_y_pt) * scale).round() as i32 + data_px_y;
            let dst_w = (sc.width_pt * PT_TO_PX * scale).round() as u32;
            let dst_h = (sc.height_pt * PT_TO_PX * scale).round() as u32;
            if dst_w == 0 || dst_h == 0 { continue; }
            let rgba = match crate::stat_chart_render::render_stat_chart_to_rgba(&sc.model, dst_w, dst_h, scale) {
                Some(r) => r,
                None => continue,
            };
            blit_floating(&mut pixmap, &rgba, dst_x, dst_y, dst_w, dst_h);
        }

        // autoFilter筛选下拉按钮渲染（从render_sheet_preview迁移，ROOT-050修复）。
        // 覆盖两种来源：
        // 1. sheet.auto_filter —— sheet级别独立<autoFilter>（<worksheet>下）
        // 2. sheet.tables[].auto_filter —— 结构化表格(Table/ListObject)自带的
        //    <autoFilter>，写在xl/tables/tableN.xml的<table>内部（ROOT-050规模34个样本）
        {
            let draw_autofilter_buttons = |pixmap: &mut tiny_skia::PixmapMut, af: &roe_model::calc::AutoFilter| {
                if af.header_row < row_end {
                    let end_col = af.end_col.min(col_end.saturating_sub(1));
                    for c in af.header_col..=end_col {
                        if c < col_start || c > col_end { continue; }
                        let is_non_rightmost_in_merge = sheet.merged_cells.iter().any(|mc| {
                            mc.r1 <= af.header_row && af.header_row <= mc.r2
                                && mc.c1 <= c && c < mc.c2
                        });
                        if is_non_rightmost_in_merge { continue; }

                        let col_offset = c - af.header_col;
                        if af.hidden_buttons.contains(&col_offset) { continue; }
                        let is_filtered = af.col_filters.contains_key(&col_offset);
                        draw_field_dropdown_btn(pixmap, sheet, cfg, tx, c, af.header_row, is_filtered);
                    }
                }
            };

            if let Some(af) = &sheet.auto_filter {
                draw_autofilter_buttons(&mut pixmap, af);
            }
            for table in &sheet.tables {
                if let Some(af) = &table.auto_filter {
                    draw_autofilter_buttons(&mut pixmap, af);
                }
            }
        }

        // 分类汇总折叠/展开按钮渲染。汇总行判定: 当前行row_outline_level
        // 严格小于下一行(insert_subtotals插入后,汇总行level比其上方数据行低,
        // 支持任意嵌套深度,不写死具体级数)。画在col_start(当前视口最左可见列)。
        {
            for row in row_start..row_end {
                let cur_idx = row as usize;
                let cur_level = sheet.row_outline_level.get(cur_idx).copied().unwrap_or(0);
                let next_level = sheet.row_outline_level.get(cur_idx + 1).copied();
                let Some(next_level) = next_level else { continue };
                if cur_level >= next_level { continue; }
                if col_start >= col_end { continue; }
                // 折叠态定义: 紧邻下方那一行(该分组的第一条数据行)当前是否hidden。
                let is_collapsed = sheet.row_hidden.get(cur_idx + 1).copied().unwrap_or(false);
                draw_collapse_btn(&mut pixmap, sheet, cfg, tx, col_start, row, is_collapsed);
            }
        }

        // 列字母标题栏（顶部）
        if header_row_h_pt > 0.0 {
            let hdr_h_px = data_px_y as f32;
            let mut hdr_paint = tiny_skia::Paint::default();
            hdr_paint.set_color_rgba8(242, 242, 242, 255);
            if let Some(rect) = tiny_skia::Rect::from_xywh(data_px_x as f32, 0.0, vw as f32 - data_px_x as f32, hdr_h_px) {
                pixmap.fill_rect(rect, &hdr_paint, tiny_skia::Transform::identity(), None);
            }
            // 列标题最左侧分隔线
            {
                let mut sep = tiny_skia::Paint::default();
                sep.set_color_rgba8(200, 200, 200, 255);
                let first_cx_px = col_x_px(sheet, col_start, cfg);
                let first_px_x = (first_cx_px - scroll_x_pt) * scale + header_col_w_pt * scale;
                if let Some(sr) = tiny_skia::Rect::from_xywh(first_px_x, 0.0, scale, hdr_h_px) {
                    pixmap.fill_rect(sr, &sep, tiny_skia::Transform::identity(), None);
                }
            }
            for c in col_start..col_end {
                let cx_px = col_x_px(sheet, c, cfg);
                let cw_px = col_width_at_px(sheet, c, cfg);
                let px_x = ((cx_px - scroll_x_pt) * scale) as i32 + data_px_x;
                let px_w = (cw_px * scale) as u32;
                if px_x + px_w as i32 <= data_px_x || px_x >= vw as i32 { continue; }
                let label = col_label(c);
                draw_header_text(
                    &mut self.font_context, &mut self.layout_context, &mut pixmap,
                    &label, px_x as f32, 0.0, px_w as f32, hdr_h_px, scale,
                );
                // 列标题右侧分隔线
                let mut sep = tiny_skia::Paint::default();
                sep.set_color_rgba8(200, 200, 200, 255);
                let sep_x = (cx_px - scroll_x_pt + cw_px) * scale + header_col_w_pt * scale - scale;
                if let Some(sr) = tiny_skia::Rect::from_xywh(sep_x, 0.0, scale, hdr_h_px) {
                    pixmap.fill_rect(sr, &sep, tiny_skia::Transform::identity(), None);
                }
            }
            let mut line_paint = tiny_skia::Paint::default();
            line_paint.set_color_rgba8(200, 200, 200, 255);
            if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, hdr_h_px - scale, vw as f32, scale) {
                pixmap.fill_rect(rect, &line_paint, tiny_skia::Transform::identity(), None);
            }
        }

        // 行号标题栏（左侧）
        if header_col_w_pt > 0.0 {
            let hdr_w_px = data_px_x as f32;
            let mut hdr_paint = tiny_skia::Paint::default();
            hdr_paint.set_color_rgba8(242, 242, 242, 255);
            if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, data_px_y as f32, hdr_w_px, vh as f32 - data_px_y as f32) {
                pixmap.fill_rect(rect, &hdr_paint, tiny_skia::Transform::identity(), None);
            }
            // 行标题最顶部分隔线
            {
                let mut sep = tiny_skia::Paint::default();
                sep.set_color_rgba8(200, 200, 200, 255);
                let first_ry_px = row_y_px(sheet, row_start, cfg);
                let first_px_y = (first_ry_px - scroll_y_pt) * scale + header_row_h_pt * scale;
                if let Some(sr) = tiny_skia::Rect::from_xywh(0.0, first_px_y, hdr_w_px, scale) {
                    pixmap.fill_rect(sr, &sep, tiny_skia::Transform::identity(), None);
                }
            }
            for r in row_start..row_end {
                // 隐藏行不画行号:高度为0,若不跳过,其行号会与紧随其后的可见行叠画
                // (同 ROOT-003 对齐的其它 viewport 绘制循环,此处为行号标题栏的遗漏)。
                if sheet.row_hidden.get(r as usize).copied().unwrap_or(false) { continue; }
                let ry_px = row_y_px(sheet, r, cfg);
                let rh_px = row_height_at_px(sheet, r, cfg);
                let px_y = ((ry_px - scroll_y_pt) * scale) as i32 + data_px_y;
                let px_h = (rh_px * scale) as u32;
                if px_y + px_h as i32 <= data_px_y || px_y >= vh as i32 { continue; }
                let label = format!("{}", r + 1);
                draw_header_text(
                    &mut self.font_context, &mut self.layout_context, &mut pixmap,
                    &label, 0.0, px_y as f32, hdr_w_px, px_h as f32, scale,
                );
                // 行标题下侧分隔线
                let mut sep = tiny_skia::Paint::default();
                sep.set_color_rgba8(200, 200, 200, 255);
                let sep_y = (ry_px - scroll_y_pt + rh_px) * scale + header_row_h_pt * scale - scale;
                if let Some(sr) = tiny_skia::Rect::from_xywh(0.0, sep_y, hdr_w_px, scale) {
                    pixmap.fill_rect(sr, &sep, tiny_skia::Transform::identity(), None);
                }
            }
            let mut line_paint = tiny_skia::Paint::default();
            line_paint.set_color_rgba8(200, 200, 200, 255);
            if let Some(rect) = tiny_skia::Rect::from_xywh(hdr_w_px - scale, 0.0, scale, vh as f32) {
                pixmap.fill_rect(rect, &line_paint, tiny_skia::Transform::identity(), None);
            }
        }

        // 左上角空白格
        if header_col_w_pt > 0.0 && header_row_h_pt > 0.0 {
            let mut corner_paint = tiny_skia::Paint::default();
            corner_paint.set_color_rgba8(242, 242, 242, 255);
            if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, data_px_x as f32, data_px_y as f32) {
                pixmap.fill_rect(rect, &corner_paint, tiny_skia::Transform::identity(), None);
            }
        }

        true
    }
}


// ──────────────────────────────────────────────
// 测试
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::calc::*;
    use smallvec::smallvec;
    use std::collections::HashMap;

    pub(super) fn make_workbook() -> roe_model::calc::Workbook {
        let mut grid = SparseGrid::default();
        // col 0: Number, Text, Formula
        grid.cols.insert(0, SparseColumn {
            blocks: smallvec![CellBlock {
                start_row: 0,
                values: smallvec![
                    CellValue::Number(42.0),
                    CellValue::Text("hello".into()),
                    CellValue::Formula { source: "=A1+1".into(), cached: Box::new(CellValue::Empty) },
                ],
            }],
        });
        // col 1: Bool, Error
        grid.cols.insert(1, SparseColumn {
            blocks: smallvec![CellBlock {
                start_row: 0,
                values: smallvec![
                    CellValue::Bool(true),
                    CellValue::Error(CellError::Div0),
                ],
            }],
        });
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: grid,
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                date1904: false,
                zoom_scale: None,
                show_grid_lines: None,
                col_hidden: vec![],
                sparkline_groups: vec![],
                row_hidden: vec![],
                col_outline_level: vec![],
                row_outline_level: vec![],
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                floating_images:  vec![],
                floating_charts:  vec![],
                floating_stat_charts: vec![],
                floating_shapes:  vec![],
                next_object_id: 0,
                default_col_width: 0.0,
                default_row_height: 0.0,
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn make_workbook_with_chart() -> roe_model::calc::Workbook {
        let mut wb = make_workbook();
        wb.sheets[0].floating_charts.push(roe_model::impress::ChartModel {
            id: 0,
            kind: roe_model::impress::ChartKind::Bar,
            title: Some("Test".to_string()),
            x_labels: vec!["A".to_string(), "B".to_string()],
            series: vec![roe_model::impress::ChartSeries {
                name: "S1".to_string(),
                data: vec![10.0, 20.0],
                color: None,
                as_line: false, trendline: false,
            }],
            x_pt: 0.0,
            y_pt: 0.0,
            width_pt: 100.0,
            height_pt: 100.0,
            show_legend: true,
            show_grid: true,
            x_axis_label: String::new(),
            y_axis_label: String::new(),
            border_color: roe_model::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 },
            border_width: 1.0,
            gap_width_pct: None,
            overlap_pct: None,
            grouping: roe_model::impress::ChartGrouping::Standard,
            hole_size_pct: None,
            data_labels: Default::default(),
            slice_border_color: None,
        });
        wb
    }


    fn make_workbook_with_data_validation(rule: roe_model::calc::DataValidationRule) -> roe_model::calc::Workbook {
        let mut wb = make_workbook();
        wb.sheets[0].data_validations.push(rule);
        wb
    }

    fn make_workbook_with_pivot() -> roe_model::calc::Workbook {
        let mut wb = make_workbook();
        wb.sheets[0].pivot = Some(roe_model::calc::PivotMeta {
            source_sheet: "Sheet1".to_string(),
            filter_fields: vec![roe_model::calc::PivotFieldDef::new("Region")],
            row_field_names: vec![],
            col_field_names: vec![],
            value_fields: vec![],
            data: roe_model::calc::PivotDataMatrix::default(),
            agg: "sum".to_string(),
            data_row_start: 2,
            data_col_start: 0,
            col_tree: Default::default(),
            row_tree: Default::default(),
            base_row: 0,
            base_col: 0,
            last_row: 10,
            last_col: 5,
            layout_mode: Default::default(),
            row_grand_totals: true,
            col_grand_totals: true,
            data_on_rows: false,
            cells_rect: None,
            ..Default::default()
        });
        wb
    }

    fn make_workbook_with_image() -> roe_model::calc::Workbook {
        let mut wb = make_workbook();
        // 1x1 红色像素 PNG 的 base64 编码
        let tiny_png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        wb.sheets[0].floating_images.push(roe_model::calc::FloatingImage {
            id: 0,
            data_base64: tiny_png_b64.to_string(),
            mime: "image/png".to_string(),
            width_pt: 50.0,
            height_pt: 50.0,
            x_pt: 0.0,
            y_pt: 0.0,
            rot: 0.0,
            hidden: false,
        });
        wb
    }

    /// 图片落点必须与单元格同一坐标系(pt*PT_TO_PX)：锚在 B2，用差异包围盒断言位置与宽高。
    #[test]
    fn test_viewport_image_placed_in_pt_to_px_space() {
        let base_wb = make_workbook();
        let mut wb_img = make_workbook_with_image();
        let cfg = CalcRenderConfig::default();
        let bx = col_x(&base_wb.sheets[0], 1, &cfg);
        let by = row_y(&base_wb.sheets[0], 1, &cfg);
        {
            let im = &mut wb_img.sheets[0].floating_images[0];
            im.x_pt = bx; im.y_pt = by; im.width_pt = 90.0; im.height_pt = 60.0;
        }
        let (vw, vh, zoom) = (600u32, 400u32, 100u32);
        let (hw, hh) = (40.0f32, 20.0f32);
        let render = |wb: &roe_model::calc::Workbook| {
            let mut r = CalcTileRenderer::new(wb);
            let mut buf = vec![0u8; (vw * vh * 4) as usize];
            assert!(r.render_viewport_into(0.0, 0.0, vw, vh, zoom, hw, hh, &mut buf));
            buf
        };
        let without = render(&base_wb);
        let with = render(&wb_img);
        let (mut minx, mut miny, mut maxx, mut maxy) = (i32::MAX, i32::MAX, -1i32, -1i32);
        for y in 0..vh as i32 { for x in 0..vw as i32 {
            let i = ((y as u32 * vw + x as u32) * 4) as usize;
            if with[i..i + 4] != without[i..i + 4] {
                minx = minx.min(x); miny = miny.min(y); maxx = maxx.max(x); maxy = maxy.max(y);
            }
        }}
        assert!(maxx >= 0, "图片应被绘制");
        let scale = zoom as f32 / 100.0;
        let exp_x = (bx * PT_TO_PX * scale) as i32 + (hw * scale) as i32;
        let exp_y = (by * PT_TO_PX * scale) as i32 + (hh * scale) as i32;
        let exp_w = (90.0 * PT_TO_PX * scale) as i32;
        let exp_h = (60.0 * PT_TO_PX * scale) as i32;
        assert!((minx - exp_x).abs() <= 2, "左边界 {} 应≈{}", minx, exp_x);
        assert!((miny - exp_y).abs() <= 2, "上边界 {} 应≈{}", miny, exp_y);
        assert!(((maxx - minx + 1) - exp_w).abs() <= 3, "宽度 {} 应≈{}", maxx - minx + 1, exp_w);
        assert!(((maxy - miny + 1) - exp_h).abs() <= 3, "高度 {} 应≈{}", maxy - miny + 1, exp_h);
    }

    #[test]
    fn test_render_viewport_draws_floating_image() {
        let wb_img = make_workbook_with_image();
        let wb_empty = make_workbook();
        let mut r_img = CalcTileRenderer::new(&wb_img);
        let mut r_empty = CalcTileRenderer::new(&wb_empty);
        let vw: u32 = 400;
        let vh: u32 = 300;
        let mut buf_img = vec![0u8; (vw * vh * 4) as usize];
        let mut buf_empty = vec![0u8; (vw * vh * 4) as usize];
        let ok1 = r_img.render_viewport_into(0.0, 0.0, vw, vh, 100, 40.0, 20.0, &mut buf_img);
        let ok2 = r_empty.render_viewport_into(0.0, 0.0, vw, vh, 100, 40.0, 20.0, &mut buf_empty);
        assert!(ok1 && ok2, "render_viewport_into 应返回 true");
        assert_ne!(
            buf_img, buf_empty,
            "带 floating_image 的 viewport 渲染结果应与空 sheet 不同，说明图片被画出来了"
        );

        let pixmap = tiny_skia::Pixmap::from_vec(
            buf_img, tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        pixmap.save_png("/tmp/roe_viewport_image.png").unwrap();
    }

    #[test]
    fn test_render_pivot_from_v2_new_build_path() {
        // 用 roe-polars-bridge::pivot_sheet_into_workbook_v2 从原始明细数据
        // 现场构建透视表（1行字段+1列字段+2值字段），验证从"新建"路径生成的
        // PivotMeta 也能被 tile.rs 现有渲染逻辑正确画出来，而不仅仅是
        // "打开已有xlsx"路径（parse_pivot_table）生成的 PivotMeta。
        use roe_model::calc::{CellBlock, CellValue, Sheet, SparseColumn, SparseGrid, Workbook};
        use roe_polars_bridge::{pivot_sheet_into_workbook_v2, PivotConfig};

        let mut grid = SparseGrid::default();
        grid.cols.insert(0, SparseColumn {
            blocks: smallvec![CellBlock {
                start_row: 0,
                values: smallvec![
                    CellValue::Text("Region".into()),
                    CellValue::Text("East".into()),
                    CellValue::Text("East".into()),
                    CellValue::Text("West".into()),
                    CellValue::Text("West".into()),
                ],
            }],
        });
        grid.cols.insert(1, SparseColumn {
            blocks: smallvec![CellBlock {
                start_row: 0,
                values: smallvec![
                    CellValue::Text("Product".into()),
                    CellValue::Text("Apple".into()),
                    CellValue::Text("Banana".into()),
                    CellValue::Text("Apple".into()),
                    CellValue::Text("Banana".into()),
                ],
            }],
        });
        grid.cols.insert(2, SparseColumn {
            blocks: smallvec![CellBlock {
                start_row: 0,
                values: smallvec![
                    CellValue::Text("Sales".into()),
                    CellValue::Number(100.0),
                    CellValue::Number(200.0),
                    CellValue::Number(150.0),
                    CellValue::Number(50.0),
                ],
            }],
        });
        grid.cols.insert(3, SparseColumn {
            blocks: smallvec![CellBlock {
                start_row: 0,
                values: smallvec![
                    CellValue::Text("Quantity".into()),
                    CellValue::Number(10.0),
                    CellValue::Number(20.0),
                    CellValue::Number(15.0),
                    CellValue::Number(5.0),
                ],
            }],
        });

        let source_sheet = Sheet {
            col_style: vec![],
            tables: vec![],
            name: "data".into(),
            cells: grid,
            col_widths: vec![],
            row_heights: vec![],
            frozen_rows: 0,
            frozen_cols: 0,
                date1904: false,
            zoom_scale: None,
            show_grid_lines: None,
            col_hidden: vec![],
            sparkline_groups: vec![],
            row_hidden: vec![],
            col_outline_level: vec![],
            row_outline_level: vec![],
            cell_formats:     std::collections::HashMap::new(),
            cond_formats:     vec![],
            data_validations: vec![],
            auto_filter:      None,
            merged_cells:     vec![],
            pivot:            None,
            floating_images:  vec![],
            floating_charts:  vec![],
            floating_stat_charts: vec![],
            floating_shapes:  vec![],
            next_object_id: 0,
            default_col_width: 0.0,
            default_row_height: 0.0,
        };

        let mut wb = Workbook {
            sheets: vec![source_sheet],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };

        let cfg = PivotConfig {
            rows: vec!["Region".into()],
            cols: vec!["Product".into()],
            values: vec!["Sales".into(), "Quantity".into()],
            agg: "sum".into(),
            filters: Default::default(),
        };

        let idx = pivot_sheet_into_workbook_v2(&mut wb, 0, &cfg, "PivotResult".into()).unwrap();
        assert!(wb.sheets[idx].pivot.is_some(), "v2生成的sheet必须带pivot元数据");

        // 渲染这个透视结果 sheet；render_tile 默认渲染 sheets[0]，
        // 所以把透视结果 sheet 挪到最前面（clone workbook，只保留pivot结果sheet做渲染）
        let pivot_sheet = wb.sheets[idx].clone();
        let render_wb = Workbook {
            sheets: vec![pivot_sheet],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };

        let mut renderer = CalcTileRenderer::new(&render_wb);
        let vw: u32 = 600;
        let vh: u32 = 400;
        let mut buf = vec![0u8; (vw * vh * 4) as usize];
        let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, 40.0, 20.0, &mut buf);
        assert!(ok, "render_viewport_into 应返回 true");
        let pixmap = tiny_skia::Pixmap::from_vec(
            buf, tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        pixmap.save_png("/tmp/roe_v2_newbuild_pivot.png").unwrap();
    }

    fn make_workbook_with_databar() -> roe_model::calc::Workbook {
        let mut grid = SparseGrid::default();
        grid.cols.insert(0, SparseColumn {
            blocks: smallvec![CellBlock {
                start_row: 0,
                values: smallvec![
                    CellValue::Number(10.0),
                    CellValue::Number(100.0),
                ],
            }],
        });
        let mut wb = Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "Sheet1".into(),
                cells: grid,
                col_widths: vec![],
                row_heights: vec![],
                frozen_rows: 0,
                frozen_cols: 0,
                date1904: false,
                zoom_scale: None,
                show_grid_lines: None,
                col_hidden: vec![],
                sparkline_groups: vec![],
                row_hidden: vec![],
                col_outline_level: vec![],
                row_outline_level: vec![],
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                floating_images:  vec![],
                floating_charts:  vec![],
                floating_stat_charts: vec![],
                floating_shapes:  vec![],
                next_object_id: 0,
                default_col_width: 0.0,
                default_row_height: 0.0,
            }],
            named_ranges: Default::default(),
            named_formulas: HashMap::new(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        wb.sheets[0].cond_formats.push(roe_model::calc::ConditionalFormatRule {
            range: roe_model::calc::CellRange {
                sheet: "Sheet1".to_string(),
                start: (0, 0),
                end: (1, 0),
            },
            stop_if_true: false,
            kind: roe_model::calc::ConditionalFormatKind::DataBar {
                min: roe_model::calc::CfValue { kind: "min".to_string(), val: String::new(), gte: true },
                max: roe_model::calc::CfValue { kind: "max".to_string(), val: String::new(), gte: true },
                bar_color: "#638EC6".to_string(),
                border_color: None,
                direction: roe_model::calc::BarDirection::Auto,
                bar_only: false,
                solid_fill: true,
                negative_fill_color: None,
                axis_color: None,
                axis_position: "automatic".to_string(),
            },
        });
        wb
    }

    #[test]
    fn test_databar_renders_proportional_bar_width() {
        // 值=100(max) 的格子在靠近右边缘处应该被条形颜色覆盖；
        // 值=10(min) 的格子在同样位置应该还是背景色（未被覆盖）。
        let wb = make_workbook_with_databar();
        let mut renderer = CalcTileRenderer::new(&wb);
        let vw: u32 = 400;
        let vh: u32 = 300;
        let mut buf = vec![0u8; (vw * vh * 4) as usize];
        let header_col_w: f32 = 40.0;
        let header_row_h: f32 = 20.0;
        let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, header_col_w, header_row_h, &mut buf);
        assert!(ok, "render_viewport_into 应返回 true");

        let cfg = CalcRenderConfig::default();
        let cw = cfg.default_col_width as i32;
        let ch = cfg.default_row_height as i32;
        let hx = header_col_w as i32;
        let hy = header_row_h as i32;

        let sample_x = hx + (cw as f32 * 0.9) as i32;
        let row0_y = hy + ch / 2;
        let row1_y = hy + ch + ch / 2;

        let idx0 = ((row0_y * vw as i32 + sample_x) * 4) as usize;
        let idx1 = ((row1_y * vw as i32 + sample_x) * 4) as usize;
        assert!(idx1 + 3 < buf.len(), "像素索引越界");

        let (r0, g0, b0) = (buf[idx0], buf[idx0 + 1], buf[idx0 + 2]);
        let (r1, g1, b1) = (buf[idx1], buf[idx1 + 1], buf[idx1 + 2]);

        assert!(
            (r0, g0, b0) != (r1, g1, b1),
            "value=10 和 value=100 在90%宽度采样点的颜色应不同,说明数据条按比例绘制, got row0=({},{},{}) row1=({},{},{})",
            r0, g0, b0, r1, g1, b1
        );
        assert!(b1 >= r1, "value=100 行应已被数据条覆盖，蓝色分量应不低于红色分量, got rgb=({},{},{})", r1, g1, b1);
        println!("databar proportional width ✓ row0=({},{},{}) row1=({},{},{})", r0, g0, b0, r1, g1, b1);
    }

    #[test]
    fn test_colorscale_renders_distinct_colors_for_min_and_max() {
        let mut wb = make_workbook_with_databar(); // 复用同一份数据 (10, 100)
        wb.sheets[0].cond_formats.clear();
        wb.sheets[0].cond_formats.push(roe_model::calc::ConditionalFormatRule {
            range: roe_model::calc::CellRange {
                sheet: "Sheet1".to_string(),
                start: (0, 0),
                end: (1, 0),
            },
            stop_if_true: false,
            kind: roe_model::calc::ConditionalFormatKind::ColorScale {
                min_type: "min".to_string(),
                min_value: None,
                min_color: "#F8696B".to_string(), // 红
                mid: None,
                max_type: "max".to_string(),
                max_value: None,
                max_color: "#63BE7B".to_string(), // 绿
            },
        });

        let mut renderer = CalcTileRenderer::new(&wb);
        let vw: u32 = 400;
        let vh: u32 = 300;
        let mut buf = vec![0u8; (vw * vh * 4) as usize];
        let header_col_w: f32 = 40.0;
        let header_row_h: f32 = 20.0;
        let ok = renderer.render_viewport_into(0.0, 0.0, vw, vh, 100, header_col_w, header_row_h, &mut buf);
        assert!(ok, "render_viewport_into 应返回 true");

        let cfg = CalcRenderConfig::default();
        let cw = cfg.default_col_width as i32;
        let ch = cfg.default_row_height as i32;
        let hx = header_col_w as i32;
        let hy = header_row_h as i32;

        let sample_x = hx + cw / 2;
        let row0_y = hy + ch / 2;        // value=10 (min) -> 应接近红色
        let row1_y = hy + ch + ch / 2;   // value=100 (max) -> 应接近绿色

        let idx0 = ((row0_y * vw as i32 + sample_x) * 4) as usize;
        let idx1 = ((row1_y * vw as i32 + sample_x) * 4) as usize;
        assert!(idx1 + 3 < buf.len(), "像素索引越界");

        let (r0, g0, _b0) = (buf[idx0], buf[idx0 + 1], buf[idx0 + 2]);
        let (r1, g1, _b1) = (buf[idx1], buf[idx1 + 1], buf[idx1 + 2]);

        assert!(r0 > g0, "min值单元格应偏红色(min_color=#F8696B), got rgb=({},{},{})", r0, g0, _b0);
        assert!(g1 > r1, "max值单元格应偏绿色(max_color=#63BE7B), got rgb=({},{},{})", r1, g1, _b1);
        println!("colorscale distinct colors ✓ row0=({},{},{}) row1=({},{},{})", r0, g0, _b0, r1, g1, _b1);
    }

    fn make_workbook_with_cond_format() -> roe_model::calc::Workbook {
        let mut wb = make_workbook();
        wb.sheets[0].cond_formats.push(roe_model::calc::ConditionalFormatRule {
            range: roe_model::calc::CellRange {
                sheet: "Sheet1".to_string(),
                start: (0, 0),
                end: (2, 0),
            },
            stop_if_true: false,
            kind: roe_model::calc::ConditionalFormatKind::CellFormat {
                condition: "gt:0".to_string(),
                format: roe_model::calc::CellFormat {
                    bold: None,
                    italic: None,
                    underline: None,
                    font_size: None,
                    font_family: None,
                    fg_color: Some("#FFFFFF".to_string()),
                    bg_color: Some("#FFD700".to_string()),
                    align_h: None,
                    align_v: None,
                    wrap_text: None,
                    num_format: None,
                    borders: None,
                },
            },
        });
        wb
    }

    #[test]
    fn test_render_viewport_draws_cond_format() {
        let wb_cf = make_workbook_with_cond_format();
        let wb_empty = make_workbook();
        let mut r_cf = CalcTileRenderer::new(&wb_cf);
        let mut r_empty = CalcTileRenderer::new(&wb_empty);
        let vw: u32 = 400;
        let vh: u32 = 300;
        let mut buf_cf = vec![0u8; (vw * vh * 4) as usize];
        let mut buf_empty = vec![0u8; (vw * vh * 4) as usize];
        let ok1 = r_cf.render_viewport_into(0.0, 0.0, vw, vh, 100, 40.0, 20.0, &mut buf_cf);
        let ok2 = r_empty.render_viewport_into(0.0, 0.0, vw, vh, 100, 40.0, 20.0, &mut buf_empty);
        assert!(ok1 && ok2, "render_viewport_into 应返回 true");
        assert_ne!(
            buf_cf, buf_empty,
            "带 cond_formats 的 viewport 渲染结果应与空 sheet 不同，说明条件格式被画出来了"
        );

        let pixmap = tiny_skia::Pixmap::from_vec(
            buf_cf, tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        pixmap.save_png("/tmp/roe_viewport_condformat.png").unwrap();
    }

    #[test]
    fn test_render_viewport_draws_pivot_filter_triangle() {
        let wb_pivot = make_workbook_with_pivot();
        let wb_empty = make_workbook();
        let mut r_pivot = CalcTileRenderer::new(&wb_pivot);
        let mut r_empty = CalcTileRenderer::new(&wb_empty);
        let vw: u32 = 400;
        let vh: u32 = 300;
        let mut buf_pivot = vec![0u8; (vw * vh * 4) as usize];
        let mut buf_empty = vec![0u8; (vw * vh * 4) as usize];
        let ok1 = r_pivot.render_viewport_into(0.0, 0.0, vw, vh, 100, 40.0, 20.0, &mut buf_pivot);
        let ok2 = r_empty.render_viewport_into(0.0, 0.0, vw, vh, 100, 40.0, 20.0, &mut buf_empty);
        assert!(ok1 && ok2, "render_viewport_into 应返回 true");
        assert_ne!(
            buf_pivot, buf_empty,
            "带 pivot 筛选字段的 viewport 渲染结果应与空 sheet 不同，说明三角标记被画出来了"
        );

        let pixmap = tiny_skia::Pixmap::from_vec(
            buf_pivot, tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        pixmap.save_png("/tmp/roe_viewport_pivot.png").unwrap();
    }

    /// 图表落点必须与单元格同一坐标系(pt*PT_TO_PX)：锚在 B2 时，
    /// 图表区域左/上外侧一像素不变，内侧第一像素被改写；右侧外一像素不变(宽度正确)。
    #[test]
    fn test_viewport_chart_placed_in_pt_to_px_space() {
        let base_wb = make_workbook();
        let mut wb_chart = make_workbook_with_chart();
        let cfg = CalcRenderConfig::default();
        let bx = col_x(&base_wb.sheets[0], 1, &cfg);
        let by = row_y(&base_wb.sheets[0], 1, &cfg);
        wb_chart.sheets[0].floating_charts[0].x_pt = bx;
        wb_chart.sheets[0].floating_charts[0].y_pt = by;
        wb_chart.sheets[0].floating_charts[0].width_pt = 100.0;
        wb_chart.sheets[0].floating_charts[0].height_pt = 60.0;

        let (vw, vh, zoom) = (600u32, 400u32, 100u32);
        let (hw, hh) = (40.0f32, 20.0f32);
        let render = |wb: &roe_model::calc::Workbook| {
            let mut r = CalcTileRenderer::new(wb);
            let mut buf = vec![0u8; (vw * vh * 4) as usize];
            assert!(r.render_viewport_into(0.0, 0.0, vw, vh, zoom, hw, hh, &mut buf));
            buf
        };
        let without = render(&base_wb);
        let with = render(&wb_chart);
        let at = |b: &Vec<u8>, x: i32, y: i32| -> [u8; 4] {
            let i = ((y as u32 * vw + x as u32) * 4) as usize;
            [b[i], b[i + 1], b[i + 2], b[i + 3]]
        };
        let scale = zoom as f32 / 100.0;
        let w = (100.0 * PT_TO_PX * scale) as i32;
        // 内侧(图表左上边框附近)应被改写，外侧(左/右)保持不变
        // 扫描与无图表版本不同的像素包围盒,用它断言位置和尺寸(±2px 容差)
        let (mut minx, mut miny, mut maxx, mut maxy) = (i32::MAX, i32::MAX, -1i32, -1i32);
        for y in 0..vh as i32 { for x in 0..vw as i32 {
            if at(&with, x, y) != at(&without, x, y) {
                minx = minx.min(x); miny = miny.min(y); maxx = maxx.max(x); maxy = maxy.max(y);
            }
        }}
        assert!(maxx >= 0, "图表应被绘制");
        let exp_x = (bx * PT_TO_PX * scale) as i32 + (hw * scale) as i32;
        let exp_y = (by * PT_TO_PX * scale) as i32 + (hh * scale) as i32;
        let exp_h = (60.0 * PT_TO_PX * scale) as i32;
        assert!((minx - exp_x).abs() <= 2, "左边界 {} 应≈{}", minx, exp_x);
        assert!((miny - exp_y).abs() <= 2, "上边界 {} 应≈{}", miny, exp_y);
        assert!(((maxx - minx + 1) - w).abs() <= 3, "宽度 {} 应≈{}", maxx - minx + 1, w);
        assert!(((maxy - miny + 1) - exp_h).abs() <= 3, "高度 {} 应≈{}", maxy - miny + 1, exp_h);
    }

    #[test]
    fn test_render_viewport_draws_floating_chart() {
        let wb_chart = make_workbook_with_chart();
        let mut r_chart = CalcTileRenderer::new(&wb_chart);
        let vw: u32 = 400;
        let vh: u32 = 300;
        let mut buf = vec![0u8; (vw * vh * 4) as usize];
        let ok = r_chart.render_viewport_into(
            0.0, 0.0, vw, vh, 100, 40.0, 20.0, &mut buf,
        );
        assert!(ok, "render_viewport_into 应返回 true");

        let all_white = buf.chunks(4).all(|p| p[0] == 0xff && p[1] == 0xff && p[2] == 0xff);
        assert!(!all_white, "viewport 渲染结果应包含非白像素（网格线+图表），说明图表被画出来了");

        // 存 PNG 供人工确认图表位置/内容是否正确
        let pixmap = tiny_skia::Pixmap::from_vec(
            buf, tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        pixmap.save_png("/tmp/roe_viewport_chart.png").unwrap();
    }

    #[test]
    fn test_preview_dimensions() {
        // v115迁移：改用 render_viewport_into 模拟整表渲染，与旧
        // render_sheet_preview(max_cols, max_rows) 语义等价。
        let wb = make_workbook();
        let mut renderer = CalcTileRenderer::new(&wb);
        let sheet = &wb.sheets[0];
        let cfg = &renderer.config;
        let w = crate::tile_geometry::col_x_px(sheet, 10, cfg) as u32 + 1;
        let h = crate::tile_geometry::row_y_px(sheet, 10, cfg) as u32 + 1;
        let mut data = vec![0u8; (w as usize) * (h as usize) * 4];
        let ok = renderer.render_viewport_into(0.0, 0.0, w, h, 100, 0.0, 0.0, &mut data);
        assert!(ok);
        assert_eq!(data.len(), (w * h * 4) as usize);
        assert!(w > 0 && h > 0);
    }

    #[test]
    fn test_save_preview_png() {
        // 生成预览图保存到 /tmp，用于人工目检
        // v115迁移：改用 render_viewport_into 模拟整表渲染。
        let wb = make_workbook();
        let mut renderer = CalcTileRenderer::new(&wb);
        let sheet = &wb.sheets[0];
        let cfg = &renderer.config;
        let w = crate::tile_geometry::col_x_px(sheet, 8, cfg) as u32 + 1;
        let h = crate::tile_geometry::row_y_px(sheet, 6, cfg) as u32 + 1;
        let mut data = vec![0u8; (w as usize) * (h as usize) * 4];
        let ok = renderer.render_viewport_into(0.0, 0.0, w, h, 100, 0.0, 0.0, &mut data);
        assert!(ok);
        let pixmap = tiny_skia::Pixmap::from_vec(
            data,
            tiny_skia::IntSize::from_wh(w, h).unwrap(),
        ).unwrap();
        pixmap.save_png("/tmp/roe_calc_preview.png").unwrap();
        println!("preview saved to /tmp/roe_calc_preview.png  ({w}x{h})");
    }

    #[test]
    fn test_col_width_at_uses_real_width_when_present() {
        let mut wb = make_workbook();
        wb.sheets[0].col_widths = vec![50.0, 200.0]; // 第0列窄，第1列宽
        let cfg = CalcRenderConfig::default();
        assert_eq!(col_width_at(&wb.sheets[0], 0, &cfg), 50.0);
        assert_eq!(col_width_at(&wb.sheets[0], 1, &cfg), 200.0);
        // 第2列没有数据，fallback 默认值
        assert_eq!(col_width_at(&wb.sheets[0], 2, &cfg), cfg.default_col_width);
    }

    #[test]
    fn test_compute_available_width_uniform_uses_real_col_width_not_hardcoded_default() {
        // ROOT-050 Bug B 回归测试：compute_available_width_uniform 此前起始宽度和
        // 向右累加宽度都硬编成 cfg.default_col_width（48pt兜底默认值），完全没有
        // 读取该sheet每列各自的 customWidth（col_widths），导致设置了自定义列宽的
        // sheet文字被严重提前截断。本测试断言：
        // 1) 当前列自己的可用宽度 = 该列的真实customWidth，不是硬编默认值；
        // 2) 向右探测空列时，累加的也是每列各自的真实宽度，不是假设所有列等宽。
        let mut wb = make_workbook();
        // 第0列customWidth=123pt（远大于default_col_width），第1列有数据（不空）
        // 用于测试点1：起始宽度不应是硬编的default_col_width
        wb.sheets[0].col_widths = vec![123.0];
        let cfg = CalcRenderConfig::default();
        assert_ne!(
            cfg.default_col_width, 123.0,
            "前提条件：default_col_width不应恰好等于123.0，否则测试无法区分两种取值"
        );
        // make_workbook() 里第1列（col_idx=1）本身有数据（Bool/Error），所以向右
        // 探测会在第1列就停止（该列非空），avail_w应该只等于第0列自己的真实宽度
        let avail_w = compute_available_width_uniform(&wb.sheets[0], 0, 2, &cfg);
        assert_eq!(
            avail_w, 123.0,
            "第0列有customWidth=123pt且右侧第1列非空，avail_w应等于第0列真实宽度，\
             不应是硬编的default_col_width={}",
            cfg.default_col_width
        );

        // 测试点2：向右累加空列宽度时，应使用每列各自的真实宽度，不是假设等宽。
        // 构造一个第1列也有customWidth、但第1列本身在数据上是空列的场景：
        // 用一个全新的空白workbook（不复用make_workbook的默认数据），
        // 第0列customWidth=30pt，第1列customWidth=90pt且无数据（应被累加进avail_w），
        // 第2列有数据（应作为停止边界）。
        let mut wb2 = make_workbook();
        wb2.sheets[0].cells.cols.remove(&0); // 清空第0列数据
        wb2.sheets[0].cells.cols.remove(&1); // 清空第1列数据，模拟"右侧空列"
        // 用第2列的数据作为非空停止点：make_workbook本身没有col2数据，需要手动插入
        wb2.sheets[0].cells.cols.insert(2, roe_model::calc::SparseColumn {
            blocks: smallvec::smallvec![roe_model::calc::CellBlock {
                start_row: 0,
                values: smallvec::smallvec![roe_model::calc::CellValue::Number(1.0)],
            }],
        });
        wb2.sheets[0].col_widths = vec![30.0, 90.0, 999.0]; // 第2列宽度不影响本次断言
        let avail_w2 = compute_available_width_uniform(&wb2.sheets[0], 0, 3, &cfg);
        assert_eq!(
            avail_w2, 30.0 + 90.0,
            "第0列(30pt)+第1列(90pt，空列应被累加)应等于120pt，不应是\
             30.0 + cfg.default_col_width这种假设等宽的硬编累加结果"
        );
    }

    #[test]
    fn test_col_width_at_and_row_height_at_hidden_returns_zero() {
        let mut wb = make_workbook();
        wb.sheets[0].col_widths = vec![50.0, 200.0];
        wb.sheets[0].row_heights = vec![roe_model::calc::RowHeight::Manual(30.0), roe_model::calc::RowHeight::Manual(40.0)];
        wb.sheets[0].col_hidden = vec![true, false]; // 第0列隐藏，第1列不隐藏
        wb.sheets[0].row_hidden = vec![false, true];  // 第0行不隐藏，第1行隐藏
        let cfg = CalcRenderConfig::default();

        assert_eq!(col_width_at(&wb.sheets[0], 0, &cfg), 0.0, "隐藏列宽度应为0");
        assert_eq!(col_width_at(&wb.sheets[0], 1, &cfg), 200.0, "未隐藏列应保持真实宽度");
        assert_eq!(row_height_at(&wb.sheets[0], 0, &cfg), 30.0, "未隐藏行应保持真实高度");
        assert_eq!(row_height_at(&wb.sheets[0], 1, &cfg), 0.0, "隐藏行高度应为0");
    }

    #[test]
    fn test_col_x_skips_hidden_column_width() {
        let mut wb = make_workbook();
        wb.sheets[0].col_widths = vec![50.0, 200.0, 30.0];
        wb.sheets[0].col_hidden = vec![false, true, false]; // 第1列隐藏
        let cfg = CalcRenderConfig::default();
        // col_x(2) 应该只累加 col0(50) + col1(隐藏=0) = 50，而不是 50+200=250
        assert_eq!(col_x(&wb.sheets[0], 2, &cfg), 50.0, "隐藏列不应占用坐标空间");
        assert_eq!(col_x(&wb.sheets[0], 3, &cfg), 80.0, "隐藏列之后的列坐标应正确前移");
    }

    #[test]
    fn test_col_x_accumulates_real_widths() {
        let mut wb = make_workbook();
        wb.sheets[0].col_widths = vec![50.0, 200.0, 30.0];
        let cfg = CalcRenderConfig::default();
        assert_eq!(col_x(&wb.sheets[0], 0, &cfg), 0.0);
        assert_eq!(col_x(&wb.sheets[0], 1, &cfg), 50.0);
        assert_eq!(col_x(&wb.sheets[0], 2, &cfg), 250.0);
        assert_eq!(col_x(&wb.sheets[0], 3, &cfg), 280.0);
    }

    #[test]
    fn test_col_at_x_roundtrip_with_col_x() {
        let mut wb = make_workbook();
        wb.sheets[0].col_widths = vec![50.0, 200.0, 30.0];
        let cfg = CalcRenderConfig::default();
        // col_x(2) = 250.0，刚好落在第2列起点，col_at_x(250.0) 应该命中第2列
        let x = col_x(&wb.sheets[0], 2, &cfg);
        assert_eq!(col_at_x(&wb.sheets[0], x, &cfg), 2);
        // 落在第1列中间
        assert_eq!(col_at_x(&wb.sheets[0], 60.0, &cfg), 1);
        // x=0 -> 第0列
        assert_eq!(col_at_x(&wb.sheets[0], 0.0, &cfg), 0);
    }

    fn count_non_bg_pixels(data: &[u8], bg: [u8; 4]) -> usize {
        data.chunks(4)
            .filter(|px| px != &[bg[0], bg[1], bg[2], bg[3]].as_slice())
            .count()
    }

    #[test]
    fn test_cell_value_to_text_basic() {
        assert_eq!(cell_value_to_text(&CellValue::Empty), "");
        assert_eq!(cell_value_to_text(&CellValue::Number(42.0)), "42");
        assert_eq!(cell_value_to_text(&CellValue::Number(3.5)), "3.5");
        assert_eq!(cell_value_to_text(&CellValue::Text("hello".into())), "hello");
        assert_eq!(cell_value_to_text(&CellValue::Bool(true)), "TRUE");
        assert_eq!(cell_value_to_text(&CellValue::Bool(false)), "FALSE");
        assert_eq!(cell_value_to_text(&CellValue::Error(CellError::Div0)), "#DIV/0!");
        assert_eq!(cell_value_to_text(&CellValue::Error(CellError::Ref)), "#REF!");
        assert_eq!(cell_value_to_text(&CellValue::Error(CellError::Na)), "#N/A");
    }

    #[test]
    fn test_render_sheet_preview_draws_text() {
        // v115迁移：改用 render_viewport_into 模拟整表渲染。
        let mut wb_empty = make_workbook();
        wb_empty.sheets[0].cells = SparseGrid::default();

        let wb_data = make_workbook();
        let mut r_data = CalcTileRenderer::new(&wb_data);
        let mut r_empty = CalcTileRenderer::new(&wb_empty);

        let sheet_data = &wb_data.sheets[0];
        let cfg_data = &r_data.config;
        let w = crate::tile_geometry::col_x_px(sheet_data, 5, cfg_data) as u32 + 1;
        let h = crate::tile_geometry::row_y_px(sheet_data, 5, cfg_data) as u32 + 1;
        let mut data_with = vec![0u8; (w as usize) * (h as usize) * 4];
        let ok_with = r_data.render_viewport_into(0.0, 0.0, w, h, 100, 0.0, 0.0, &mut data_with);
        assert!(ok_with);

        let sheet_empty = &wb_empty.sheets[0];
        let cfg_empty = &r_empty.config;
        let w2 = crate::tile_geometry::col_x_px(sheet_empty, 5, cfg_empty) as u32 + 1;
        let h2 = crate::tile_geometry::row_y_px(sheet_empty, 5, cfg_empty) as u32 + 1;
        let mut data_without = vec![0u8; (w2 as usize) * (h2 as usize) * 4];
        let ok_without = r_empty.render_viewport_into(0.0, 0.0, w2, h2, 100, 0.0, 0.0, &mut data_without);
        assert!(ok_without);

        assert_ne!(data_with, data_without, "render_viewport_into 应该把单元格文字画出来，有无数据结果应不同");
    }

    #[test]
    fn test_error_cell_has_red_background() {
        // v115迁移：改用 render_viewport_into 模拟整表渲染。采样坐标换算
        // 同样需要用 col_x_px/row_y_px 保证与实际画布尺寸一致，不能再用
        // 旧的 "5*cw+1" 手工公式（那是 render_sheet_preview 专属的画布宽度
        // 计算方式，render_viewport_into 用的是 tile_geometry 里的换算）。
        let wb = make_workbook();
        let mut renderer = CalcTileRenderer::new(&wb);
        let sheet = &wb.sheets[0];
        let cfg = &renderer.config;
        let w = crate::tile_geometry::col_x_px(sheet, 5, cfg) as u32 + 1;
        let h = crate::tile_geometry::row_y_px(sheet, 5, cfg) as u32 + 1;
        let mut data = vec![0u8; (w as usize) * (h as usize) * 4];
        let ok = renderer.render_viewport_into(0.0, 0.0, w, h, 100, 0.0, 0.0, &mut data);
        assert!(ok);

        let default_cfg = CalcRenderConfig::default();
        let cw = (default_cfg.default_col_width * PT_TO_PX) as i32;
        let ch = (default_cfg.default_row_height * PT_TO_PX) as i32;
        let width = w as i32;

        let cx = cw + cw / 2;
        let cy = ch + ch / 2;
        let idx = ((cy * width + cx) * 4) as usize;

        assert!(idx + 3 < data.len(), "像素索引越界，渲染尺寸可能不对");
        let (r, g, b) = (data[idx], data[idx + 1], data[idx + 2]);
        assert!(r > g && r > b, "错误单元格区域应该有红色调背景打底, got rgb=({},{},{})", r, g, b);
    }

    /// 图表样式面板 bug 回归测试：验证 border_width/border_color 真正被读取并画出可见差异。
    /// 之前 render_chart_model_to_rgba 完全没读这几个字段，无论前台怎么调都不会变化，
    /// 这个测试直接对比两张不同 border_width 的渲染结果，防止再次回归成"字段读了但没用上"。
    #[test]
    fn test_chart_border_width_affects_render() {
        let mut model = roe_model::impress::ChartModel {
            id: 1,
            kind: roe_model::impress::ChartKind::Bar,
            title: Some("T".to_string()),
            x_labels: vec!["A".to_string(), "B".to_string()],
            series: vec![roe_model::impress::ChartSeries {
                name: "S1".to_string(),
                data: vec![10.0, 20.0],
                color: None,
                as_line: false, trendline: false,
            }],
            x_pt: 0.0,
            y_pt: 0.0,
            width_pt: 100.0,
            height_pt: 100.0,
            show_legend: true,
            show_grid: true,
            x_axis_label: String::new(),
            y_axis_label: String::new(),
            border_color: roe_model::common::Color { r: 0, g: 0, b: 0, a: 255 },
            border_width: 0.0,
            gap_width_pct: None,
            overlap_pct: None,
            grouping: roe_model::impress::ChartGrouping::Standard,
            hole_size_pct: None,
            data_labels: Default::default(),
            slice_border_color: None,
        };
        let no_border = render_chart_model_to_rgba(&model, 200, 150)
            .expect("render_chart_model_to_rgba should succeed");

        model.border_width = 8.0;
        let with_border = render_chart_model_to_rgba(&model, 200, 150)
            .expect("render_chart_model_to_rgba should succeed");

        assert_ne!(
            no_border, with_border,
            "border_width 从 0 改为 8.0 后渲染结果应不同，说明外边框真正被画出来了"
        );
    }

    /// 同上思路：验证 show_legend=false 时渲染结果与 true 时不同（legend_show 真正被接入 charts-rs）。
    #[test]
    fn test_chart_show_legend_affects_render() {
        let mut model = roe_model::impress::ChartModel {
            id: 2,
            kind: roe_model::impress::ChartKind::Bar,
            title: Some("T".to_string()),
            x_labels: vec!["A".to_string(), "B".to_string()],
            series: vec![roe_model::impress::ChartSeries {
                name: "S1".to_string(),
                data: vec![10.0, 20.0],
                color: None,
                as_line: false, trendline: false,
            }],
            x_pt: 0.0,
            y_pt: 0.0,
            width_pt: 100.0,
            height_pt: 100.0,
            show_legend: true,
            show_grid: true,
            x_axis_label: String::new(),
            y_axis_label: String::new(),
            border_color: roe_model::common::Color { r: 0xcc, g: 0xcc, b: 0xcc, a: 255 },
            border_width: 0.0,
            gap_width_pct: None,
            overlap_pct: None,
            grouping: roe_model::impress::ChartGrouping::Standard,
            hole_size_pct: None,
            data_labels: Default::default(),
            slice_border_color: None,
        };
        let legend_on = render_chart_model_to_rgba(&model, 200, 150)
            .expect("render_chart_model_to_rgba should succeed");

        model.show_legend = false;
        let legend_off = render_chart_model_to_rgba(&model, 200, 150)
            .expect("render_chart_model_to_rgba should succeed");

        assert_ne!(
            legend_on, legend_off,
            "show_legend 从 true 改为 false 后渲染结果应不同，说明 legend_show 真正被接入 charts-rs"
        );
    }
}


#[cfg(test)]
mod pivot_render_tests {
    use super::*;

    #[allow(dead_code)]
    /// v153技术债3修复：原实现直接解析xlsx文件里的静态透视表快照
    /// （roe-parser::ooxml::pivot，对应t="grand"/"default"标记），这条路径
    /// 与ROE架构原则（v151确认："ROE深思熟虑地忽略xlsx静态透视表快照，
    /// 一律从明细数据用pivot_sheet_into_workbook_v2重新计算"）不一致，
    /// 也是本函数此前"默认不开小计"的根因——不是开关没打开，是压根没走
    /// v152新增的小计插入逻辑。现改为：读明细sheet("pivot_test") + 显式
    /// PivotConfig，调用pivot_sheet_into_workbook_v2现场计算，与
    /// render_pivot_subtotal_corner.rs（已验证正确）和C-ABI层
    /// roe_pivot_sheet（roe-api/src/pivot.rs，前端实际调用路径）保持一致。
    fn render_pivot_to_png_impl(
        xlsx_path: &str,
        out_png: &str,
        rows: &[&str],
        cols: &[&str],
        values: &[&str],
    ) {
        let bytes = match std::fs::read(xlsx_path) {
            Ok(b) => b,
            Err(_) => { eprintln!("skip: {} not found", xlsx_path); return; }
        };
        let doc = roe_parser::RoeDocument::open_xlsx_bytes(&bytes).expect("parse");
        let mut wb = match doc { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        let source_idx = match wb.sheets.iter().position(|s| s.name == "pivot_test") {
            Some(i) => i,
            None => { eprintln!("!! no pivot_test detail sheet in {}", xlsx_path); return; }
        };
        let cfg = roe_polars_bridge::PivotConfig {
            rows: rows.iter().map(|s| s.to_string()).collect(),
            cols: cols.iter().map(|s| s.to_string()).collect(),
            values: values.iter().map(|s| s.to_string()).collect(),
            agg: "sum".into(),
            filters: Default::default(),
        };
        let pivot_idx = roe_polars_bridge::pivot_sheet_into_workbook_v2(
            &mut wb, source_idx, &cfg, "PivotResult".into(),
        ).expect("pivot_sheet_into_workbook_v2 failed");

        // v153修复：此前遗漏设置sheet_idx，且误将pivot_idx传进了zoom_pct参数位
        // （render_viewport_into第5个参数是缩放百分比，不是sheet索引；渲染哪个
        // sheet由renderer.sheet_idx字段决定，不是viewport调用的参数）。这是本函数
        // 迁移到v2路径后"仍然看不到小计"的直接原因——渲染的根本不是新生成的透视
        // 表sheet。修复参照render_pivot_subtotal_corner.rs的正确调用方式。
        let mut renderer = CalcTileRenderer::new(&wb);
        renderer.sheet_idx = pivot_idx;
        let vw = 1200u32;
        let vh = 800u32;
        let mut buf = vec![0u8; (vw * vh * 4) as usize];
        let ok = renderer.render_viewport_into(
            0.0, 0.0, vw, vh,
            100,
            80.0, 20.0,
            &mut buf,
        );
        assert!(ok, "render failed for {}", xlsx_path);
        let pixmap = tiny_skia::Pixmap::from_vec(
            buf,
            tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        pixmap.save_png(out_png).unwrap();
        println!("saved {}", out_png);
    }

    fn render_sparkline_sheet_to_png_impl(xlsx_path: &str, sheet_idx: usize, out_png: &str) {
        let bytes = match std::fs::read(xlsx_path) {
            Ok(b) => b,
            Err(_) => { eprintln!("skip: {} not found", xlsx_path); return; }
        };
        let doc = roe_parser::RoeDocument::open_xlsx_bytes(&bytes).expect("parse");
        let wb = match doc { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        assert!(sheet_idx < wb.sheets.len(), "sheet_idx out of range for {}", xlsx_path);
        let mut renderer = CalcTileRenderer::new(&wb);
        renderer.sheet_idx = sheet_idx;
        let vw = 1600u32;
        let vh = 500u32;
        let mut buf = vec![0u8; (vw * vh * 4) as usize];
        let ok = renderer.render_viewport_into(
            0.0, 0.0, vw, vh,
            100,
            80.0, 20.0,
            &mut buf,
        );
        assert!(ok, "render failed for {} sheet {}", xlsx_path, sheet_idx);
        let pixmap = tiny_skia::Pixmap::from_vec(
            buf,
            tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        pixmap.save_png(out_png).unwrap();
        println!("saved {}", out_png);
    }

    fn render_sparkline_fods_sheet_to_png_impl(fods_path: &str, sheet_idx: usize, out_png: &str) {
        let bytes = match std::fs::read(fods_path) {
            Ok(b) => b,
            Err(_) => { eprintln!("skip: {} not found", fods_path); return; }
        };
        let doc = roe_parser::RoeDocument::open_fods_bytes(&bytes).expect("parse");
        let wb = match doc { roe_parser::RoeDocument::Calc(wb) => wb, _ => panic!("not calc") };
        assert!(sheet_idx < wb.sheets.len(), "sheet_idx out of range for {}", fods_path);
        let mut renderer = CalcTileRenderer::new(&wb);
        renderer.sheet_idx = sheet_idx;
        let vw = 1600u32;
        let vh = 500u32;
        let mut buf = vec![0u8; (vw * vh * 4) as usize];
        let ok = renderer.render_viewport_into(
            0.0, 0.0, vw, vh,
            100,
            80.0, 20.0,
            &mut buf,
        );
        assert!(ok, "render failed for {} sheet {}", fods_path, sheet_idx);
        let pixmap = tiny_skia::Pixmap::from_vec(
            buf,
            tiny_skia::IntSize::from_wh(vw, vh).unwrap(),
        ).unwrap();
        pixmap.save_png(out_png).unwrap();
        println!("saved {}", out_png);
    }

    #[test]
    fn test_render_sparklines_real_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../test_data/Sparklines.xlsx");
        render_sparkline_sheet_to_png_impl(path, 0, "/tmp/sparkline_sheet1.png");
        render_sparkline_sheet_to_png_impl(path, 1, "/tmp/sparkline_sheet2.png");
        render_sparkline_sheet_to_png_impl(path, 2, "/tmp/sparkline_sheet3.png");
    }

    #[test]
    fn test_render_sparklines_fods_real_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../test_data/Sparklines.fods");
        render_sparkline_fods_sheet_to_png_impl(path, 0, "/tmp/sparkline_fods_sheet1.png");
    }

    #[test]
    fn test_render_pivot_matrix() {
        // v153：全部改为走pivot_sheet_into_workbook_v2现场计算（含小计），
        // 而非解析xlsx里的静态快照。rows/cols/values对照探测结果（探测脚本
        // roe-api/examples/probe_pivot_files.rs，已在会话中确认后删除）。
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_1row.xlsx", "/tmp/pivot_1row.png",
            &["Category"], &[], &["Sales"]);
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_1col.xlsx", "/tmp/pivot_1col.png",
            &[], &["Color"], &["Sales"]);
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_1row1col.xlsx", "/tmp/pivot_1row1col.png",
            &["Category"], &["Color"], &["Sales"]);
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_2row.xlsx", "/tmp/pivot_2row.png",
            &["Category", "SubCategory"], &[], &["Sales"]);
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_2col.xlsx", "/tmp/pivot_2col.png",
            &[], &["Color", "Size"], &["Sales"]);
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_2row2col.xlsx", "/tmp/pivot_2row2col.png",
            &["Category", "SubCategory"], &["Color", "Size"], &["Sales"]);
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_1row1col1filter.xlsx", "/tmp/pivot_1row1col1filter.png",
            &["Category"], &["Color"], &["Sales"]);
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_test.xlsx", "/tmp/pivot_full.png",
            &["Category", "SubCategory"], &["Color", "Size"], &["Sales"]);
        render_pivot_to_png_impl("/home/yong/pivot_tests/pivot_1row1col_2val.xlsx", "/tmp/pivot_1row1col_2val.png",
            &["Category"], &["Color"], &["Sales", "Quantity"]);
    }
}

#[cfg(test)]
mod scroll_consistency_tests {
    use super::*;

    const HW: f32 = 40.0;
    const HH: f32 = 20.0;

    fn render(scroll_x: f32, scroll_y: f32, vw: u32, vh: u32) -> Vec<u8> {
        let wb = super::tests::make_workbook();
        let mut r = CalcTileRenderer::new(&wb);
        let mut buf = vec![0u8; (vw * vh * 4) as usize];
        assert!(r.render_viewport_into(scroll_x, scroll_y, vw, vh, 100, HW, HH, &mut buf));
        buf
    }

    fn px(buf: &[u8], vw: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * vw + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    // 把连续为 true 的下标合并成一簇，返回每簇中心
    fn clusters(flags: &[bool], offset: u32) -> Vec<f32> {
        let mut out = vec![];
        let mut start: Option<usize> = None;
        for (i, f) in flags.iter().enumerate() {
            match (f, start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => { out.push(offset as f32 + (s + i) as f32 / 2.0); start = None; }
                _ => {}
            }
        }
        if let Some(s) = start { out.push(offset as f32 + (s + flags.len()) as f32 / 2.0); }
        out
    }

    // 数据区里横向贯穿的网格线 y（>=70% 的像素非白）
    fn data_h_lines(buf: &[u8], vw: u32, vh: u32) -> Vec<f32> {
        let x0 = HW as u32 + 1;
        let y0 = HH as u32;
        let flags: Vec<bool> = (y0..vh).map(|y| {
            let n = (x0..vw).filter(|&x| { let p = px(buf, vw, x, y); p[0] < 250 || p[1] < 250 || p[2] < 250 }).count();
            n as f32 >= 0.7 * (vw - x0) as f32
        }).collect();
        clusters(&flags, y0)
    }

    // 数据区里纵向贯穿的网格线 x
    fn data_v_lines(buf: &[u8], vw: u32, vh: u32) -> Vec<f32> {
        let x0 = HW as u32 + 1;
        let y0 = HH as u32 + 1;
        let flags: Vec<bool> = (x0..vw).map(|x| {
            let n = (y0..vh).filter(|&y| { let p = px(buf, vw, x, y); p[0] < 250 || p[1] < 250 || p[2] < 250 }).count();
            n as f32 >= 0.7 * (vh - y0) as f32
        }).collect();
        clusters(&flags, x0)
    }

    // 行号标题栏里的分隔线 y（x=1 处颜色比标题栏底色 242 更深）
    fn header_row_seps(buf: &[u8], vw: u32, vh: u32) -> Vec<f32> {
        let y0 = HH as u32 + 1;
        let flags: Vec<bool> = (y0..vh).map(|y| px(buf, vw, 1, y)[0] < 230).collect();
        clusters(&flags, y0)
    }

    fn max_mismatch(hdr: &[f32], data: &[f32]) -> (f32, String) {
        let mut worst = 0.0f32;
        let mut log = String::new();
        for h in hdr {
            let d = data.iter().cloned().fold(f32::MAX, |a, b| if (b - h).abs() < (a - h).abs() { b } else { a });
            let diff = (d - h).abs();
            log.push_str(&format!("  标题栏线 y={:.1}  最近数据网格线 y={:.1}  差 {:.1}\n", h, d, diff));
            if diff > worst { worst = diff; }
        }
        (worst, log)
    }

    /// 测试1：scroll_y=700，标题栏分隔线与数据区网格线应落在同一行位置
    #[test]
    fn scroll_consistency_header_vs_grid_at_700() {
        let (vw, vh) = (600u32, 400u32);
        let buf = render(0.0, 700.0, vw, vh);
        let hdr = header_row_seps(&buf, vw, vh);
        let data = data_h_lines(&buf, vw, vh);
        let (worst, log) = max_mismatch(&hdr, &data);
        eprintln!("[T1] scroll_y=700 标题栏线 {} 条, 数据网格线 {} 条, 最大偏差 {:.1}px\n{}", hdr.len(), data.len(), worst, log);
        assert!(!hdr.is_empty() && !data.is_empty(), "没量到线: hdr={:?} data={:?}", hdr, data);
        assert!(worst <= 2.5, "标题栏与数据区行线不在同一位置，最大偏差 {:.1}px", worst);
    }

    /// 测试2：scroll 取 0/100/700/3000，网格线不能出现断层
    #[test]
    fn scroll_consistency_grid_no_gap() {
        let (vw, vh) = (600u32, 400u32);
        let wb = super::tests::make_workbook();
        let cfg = CalcRenderConfig::default();
        let rowh = row_height_at_px(&wb.sheets[0], 0, &cfg);
        let colw = col_width_at_px(&wb.sheets[0], 0, &cfg);
        for s in [0.0f32, 100.0, 700.0, 3000.0] {
            let buf = render(s, s, vw, vh);
            let h = data_h_lines(&buf, vw, vh);
            let v = data_v_lines(&buf, vw, vh);
            eprintln!("[T2] scroll={} 行高{:.1} 列宽{:.1}\n  横线: {:?}\n  竖线: {:?}", s, rowh, colw, h, v);
            assert!(h.len() >= 2 && v.len() >= 2, "scroll={} 线太少 h={:?} v={:?}", s, h, v);
            assert!(h[0] - HH <= rowh + 2.0, "scroll={} 顶部缺线 首条 y={}", s, h[0]);
            assert!(vh as f32 - h[h.len() - 1] <= rowh + 2.0, "scroll={} 底部缺线 末条 y={}", s, h[h.len() - 1]);
            for w in h.windows(2) { assert!(w[1] - w[0] <= rowh + 2.0, "scroll={} 横线断层 {:?}", s, w); }
            assert!(v[0] - HW <= colw + 2.0, "scroll={} 左侧缺线 首条 x={}", s, v[0]);
            assert!(vw as f32 - v[v.len() - 1] <= colw + 2.0, "scroll={} 右侧缺线 末条 x={}", s, v[v.len() - 1]);
            for w in v.windows(2) { assert!(w[1] - w[0] <= colw + 2.0, "scroll={} 竖线断层 {:?}", s, w); }
        }
    }

    /// 测试3：scroll=0 用很高的视口，看偏差是否随行号累积
    #[test]
    fn scroll_consistency_drift_by_row() {
        let (vw, vh) = (300u32, 1400u32);
        let buf = render(0.0, 0.0, vw, vh);
        let hdr = header_row_seps(&buf, vw, vh);
        let data = data_h_lines(&buf, vw, vh);
        let (worst, log) = max_mismatch(&hdr, &data);
        eprintln!("[T3] scroll=0 标题栏线 {} 条, 数据网格线 {} 条, 最大偏差 {:.1}px\n{}", hdr.len(), data.len(), worst, log);
        assert!(!hdr.is_empty() && !data.is_empty());
        assert!(worst <= 2.5, "scroll=0 下标题栏线与网格线仍有偏差 {:.1}px，可能是逐行累计", worst);
    }
}

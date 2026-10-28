// ──────────────────────────────────────────────
// CalcTileRenderer：把 Workbook 渲染成网格 tile
// ──────────────────────────────────────────────


/// Calc 网格渲染配置
#[derive(Debug, Clone)]
pub struct CalcRenderConfig {
    /// 默认列宽（pt，ROOT-049修复：此前误标"像素"且硬编码96.0，
    /// 与 sheet.default_col_width/col_width_at 等其余全部列宽数值的
    /// pt 语义不一致，导致该 fallback 分支被 col_width_at_px 换算
    /// 时无中生有多套了一次 DPI 系数。现改为 Excel 标准默认列宽
    /// 8.43 字符换算后的 pt 值：(8.43*7+5)*0.75 ≈ 48.0pt，
    /// 与 xlsx.rs 里 defaultColWidth 解析使用的同一换算公式对齐）
    pub default_col_width:  f32,
    /// 默认行高（pt，ROOT-049修复：此前误标"像素"且硬编码24.0，
    /// 现改为 Excel 标准默认行高 15pt，与常见 xlsx
    /// sheetFormatPr defaultRowHeight="15" 一致）
    pub default_row_height: f32,
    /// tile 尺寸（像素）
    pub tile_size:          u32,
    /// 网格线颜色 RGBA
    pub grid_color:         [u8; 4],
    /// 背景色
    pub bg_color:           [u8; 4],
    /// 文字颜色
    pub text_color:         [u8; 4],
}

impl Default for CalcRenderConfig {
    fn default() -> Self {
        Self {
            default_col_width:  48.0,
            default_row_height: 15.0,
            tile_size:          256,
            grid_color:         [0xd0, 0xd0, 0xd0, 0xff],
            bg_color:           [0xff, 0xff, 0xff, 0xff],
            text_color:         [0x20, 0x20, 0x20, 0xff],
        }
    }
}

// ──────────────────────────────────────────────
// 坐标换算公共函数
//
// sheet.col_widths / row_heights 是"按列/行出现顺序"记录的真实宽高
// （parser 解析 table:number-columns-repeated 等展开后 push 进数组），
// 下标即列号/行号。为空或越界时 fallback 到 cfg 的默认值（兼容旧数据/
// 尚未解析列宽的格式，如当前的 xlsx.rs）。
//
// CalcTileRenderer（渲染）与 roe-api 几何查询接口共用这套函数，保证
// "画出来的格子"与"查询返回的坐标"永远一致。
// ──────────────────────────────────────────────

/// 第 col 列的宽度。隐藏列返回 0.0（不占用渲染/坐标空间）；
/// 越界或该 sheet 未提供列宽数据时返回默认列宽。
/// hidden 检查前置：所有坐标计算（col_x/col_at_x）与渲染都基于此函数，
/// 因此隐藏列会自动从布局中消失，不需要在每个渲染/查询调用点单独判断。
pub fn col_width_at(sheet: &roe_model::calc::Sheet, col: u32, cfg: &CalcRenderConfig) -> f32 {
    if sheet.col_hidden.get(col as usize).copied().unwrap_or(false) {
        return 0.0;
    }
    sheet
        .col_widths
        .get(col as usize)
        .copied()
        .filter(|w| *w > 0.0)
        .unwrap_or({
            if sheet.default_col_width > 0.0 { sheet.default_col_width }
            else { cfg.default_col_width }
        })
}

/// 第 row 行的高度。隐藏行返回 0.0（不占用渲染/坐标空间）；
/// 越界或该 sheet 未提供行高数据时返回默认行高。同 col_width_at 的
/// hidden 前置检查设计，见上方注释。
/// ROOT-082: 纯只读 Paint/Query 阶段函数（方案A两阶段设计的第二阶段）。
/// 不触发任何懒加载测量副作用，Auto 分支只读 current（可能是尚未测量
/// 的估算值）。真正的测量+回写只发生在 render_viewport_into 的
/// Layout 阶段，通过独立的 &mut Sheet 函数（ensure_row_height_measured，
/// 待实现）触发，本函数调用前应确保该行已经过 Layout 阶段处理，否则
/// 拿到的是估算值而非精确测量值——这是符合预期的（不可见/未渲染过的
/// 行本就没有理由做精确测量）。
pub fn row_height_at(sheet: &roe_model::calc::Sheet, row: u32, cfg: &CalcRenderConfig) -> f32 {
    if sheet.row_hidden.get(row as usize).copied().unwrap_or(false) {
        return 0.0;
    }
    sheet
        .row_heights
        .get(row as usize)
        .map(|rh| rh.current())
        .filter(|h| *h > 0.0)
        .unwrap_or({
            if sheet.default_row_height > 0.0 { sheet.default_row_height }
            else { cfg.default_row_height }
        })
}

/// 第 col 列左边界的 x 坐标（从第 0 列累加宽度到 col）。
/// O(col) 复杂度：viewport 范围内调用没有性能问题；如果未来发现是
/// 热点（比如超大表格的随机访问查询），可以加前缀和缓存，接口签名
/// 不需要变。
pub fn col_x(sheet: &roe_model::calc::Sheet, col: u32, cfg: &CalcRenderConfig) -> f32 {
    (0..col).map(|c| col_width_at(sheet, c, cfg)).sum()
}

/// 第 row 行上边界的 y 坐标（从第 0 行累加高度到 row）。
pub fn row_y(sheet: &roe_model::calc::Sheet, row: u32, cfg: &CalcRenderConfig) -> f32 {
    (0..row).map(|r| row_height_at(sheet, r, cfg)).sum()
}

// ──────────────────────────────────────────────
// ROOT-049修复：pt → px 渲染换算
//
// 背景：col_width_at/row_height_at/col_x/row_y 返回值语义是 pt
// （与 roe-model 内存模型、roe-api 几何查询接口 C-ABI 保持一致，
// office-rs2 前端依赖这个 pt 语义，不能改）。但 CalcTileRenderer
// 的实际画布渲染此前直接把这些 pt 数值当 px 使用，从未做 DPI 换算，
// 导致任何带非默认行高/列宽（如 xlsx <row ht="28.35">）的文件渲染
// 出来的格子比例系统性偏小（约为正确值的 72/96 ≈ 75%）。
//
// Writer（roe-writer/tile.rs）和 Impress（roe-impress/slide_renderer.rs）
// 渲染路径已有各自的 scale 概念做了这一层换算，只有 Calc 这条线遗漏。
// 此处新增的 _px 系列函数仅供 CalcTileRenderer 渲染画布坐标使用，
// C-ABI 几何查询接口（roe-api/calc_geometry.rs）继续调用不带 _px
// 后缀的原始 pt 函数，两者语义不混淆。
//
// 换算系数固定为 96.0/72.0（标准 96 DPI 屏幕 vs. 72 DPI 印刷点），
// 与 zoom/HiDPI 缩放（paint_tile 里的 scale，来自 TileId.zoom）是
// 两个独立维度，不可合并成同一个变量。
// ──────────────────────────────────────────────

/// pt → px 换算系数（96 DPI / 72 DPI）
pub const PT_TO_PX: f32 = 96.0 / 72.0;

/// 第 col 列的宽度（像素，供渲染画布使用）。
pub fn col_width_at_px(sheet: &roe_model::calc::Sheet, col: u32, cfg: &CalcRenderConfig) -> f32 {
    col_width_at(sheet, col, cfg) * PT_TO_PX
}

/// 第 row 行的高度（像素，供渲染画布使用）。
pub fn row_height_at_px(sheet: &roe_model::calc::Sheet, row: u32, cfg: &CalcRenderConfig) -> f32 {
    row_height_at(sheet, row, cfg) * PT_TO_PX
}

/// 第 col 列左边界的 x 坐标（像素，供渲染画布使用）。
pub fn col_x_px(sheet: &roe_model::calc::Sheet, col: u32, cfg: &CalcRenderConfig) -> f32 {
    col_x(sheet, col, cfg) * PT_TO_PX
}

/// 第 row 行上边界的 y 坐标（像素，供渲染画布使用）。
pub fn row_y_px(sheet: &roe_model::calc::Sheet, row: u32, cfg: &CalcRenderConfig) -> f32 {
    row_y(sheet, row, cfg) * PT_TO_PX
}

/// 给定 x 坐标，反查落在第几列（用于 hit_test）。
/// x 落在内容区域之外（负数）时钳制到第 0 列；超出已知范围按默认宽度
/// 持续推算（用于大表格虚拟渲染场景的远端查询）。
pub fn col_at_x(sheet: &roe_model::calc::Sheet, x: f32, cfg: &CalcRenderConfig) -> u32 {
    if x <= 0.0 {
        return 0;
    }
    let mut acc = 0.0f32;
    let mut col = 0u32;
    loop {
        let w = col_width_at(sheet, col, cfg);
        if acc + w > x {
            return col;
        }
        acc += w;
        col += 1;
        // 安全阀：防止异常数据导致死循环
        if col > 1_000_000 {
            return col;
        }
    }
}

/// 给定 y 坐标，反查落在第几行（用于 hit_test）。逻辑同 col_at_x。
pub fn row_at_y(sheet: &roe_model::calc::Sheet, y: f32, cfg: &CalcRenderConfig) -> u32 {
    if y <= 0.0 {
        return 0;
    }
    let mut acc = 0.0f32;
    let mut row = 0u32;
    loop {
        let h = row_height_at(sheet, row, cfg);
        if acc + h > y {
            return row;
        }
        acc += h;
        row += 1;
        if row > 1_000_000 {
            return row;
        }
    }
}


// ──────────────────────────────────────────────
// 浮动对象(图表等)几何:单元格 <-> pt、矩形重叠
// 单位 pt,原点为数据区左上角(不含行列头),与渲染/命中测试同一套换算。
// ──────────────────────────────────────────────

/// 单元格左上角的 pt 坐标 (x, y)。隐藏行列宽高为 0,与渲染一致。
pub fn cell_to_pt(sheet: &roe_model::calc::Sheet, row: u32, col: u32, cfg: &CalcRenderConfig) -> (f32, f32) {
    (col_x(sheet, col, cfg), row_y(sheet, row, cfg))
}

/// pt 坐标落在哪个单元格,返回 (row, col)。
pub fn pt_to_cell(sheet: &roe_model::calc::Sheet, x: f32, y: f32, cfg: &CalcRenderConfig) -> (u32, u32) {
    (row_at_y(sheet, y, cfg), col_at_x(sheet, x, cfg))
}

/// 两个矩形 (x, y, w, h) 是否有面积重叠。仅边相贴不算重叠;宽或高 <= 0 的不算。
pub fn rect_overlap(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    const EPS: f32 = 0.01;
    let ox = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
    let oy = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
    ox > EPS && oy > EPS
}

#[cfg(test)]
mod chart_geom_tests {
    use super::*;
    use roe_model::calc::Sheet;

    #[test]
    fn rect_overlap_basic() {
        let a = (0.0, 0.0, 100.0, 100.0);
        assert!(rect_overlap(a, (50.0, 50.0, 100.0, 100.0)));      // 部分相交
        assert!(rect_overlap(a, (10.0, 10.0, 20.0, 20.0)));        // 包含
        assert!(!rect_overlap(a, (100.0, 0.0, 50.0, 50.0)));       // 右边相贴
        assert!(!rect_overlap(a, (0.0, 100.0, 50.0, 50.0)));       // 下边相贴
        assert!(!rect_overlap(a, (200.0, 200.0, 10.0, 10.0)));     // 分离
        assert!(!rect_overlap(a, (10.0, 10.0, 0.0, 20.0)));        // 零宽
    }

    #[test]
    fn cell_to_pt_uses_defaults_and_roundtrips() {
        let sh = Sheet::default(); // default_* = 0 -> 回退 cfg 的 48 / 15
        let cfg = CalcRenderConfig::default();
        assert_eq!(cell_to_pt(&sh, 2, 3, &cfg), (144.0, 30.0));
        let (x, y) = cell_to_pt(&sh, 2, 3, &cfg);
        assert_eq!(pt_to_cell(&sh, x + 1.0, y + 1.0, &cfg), (2, 3));
    }

    #[test]
    fn cell_to_pt_respects_custom_and_hidden_cols() {
        let mut sh = Sheet::default();
        let cfg = CalcRenderConfig::default();
        sh.col_widths = vec![10.0, 20.0, 30.0];
        assert_eq!(cell_to_pt(&sh, 0, 3, &cfg).0, 60.0);
        sh.col_hidden = vec![false, true, false];
        assert_eq!(cell_to_pt(&sh, 0, 3, &cfg).0, 40.0); // 第 1 列隐藏,宽 0
    }
}

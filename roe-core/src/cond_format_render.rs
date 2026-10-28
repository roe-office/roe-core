//! 条件格式可视化渲染：数据条（DataBar）+ 色阶（ColorScale）
//!
//! 不放进 tile.rs（已 3000+ 行技术债），独立文件实现。
//! IconSet 渲染实现见 icon_set_render.rs（v105，17 种 Excel 预设图标，
//! 纯 tiny-skia PathBuilder 矢量绘制，不依赖外部图标资源）。
//!
//! 算法思路参考 excelize（Go）`drawCondFmtDataBar`/`drawCondFmtColorScale`
//! 的阈值解析+线性插值逻辑（未抄代码，纯逻辑借鉴，用 Rust 重新实现）。

use tiny_skia::{Paint, PathBuilder, PixmapMut, Rect, Transform};

use crate::tile::{col_width_at_px, col_x_px, row_height_at_px, row_y_px, CalcRenderConfig};
use roe_model::calc::{CellValue, CfValue, ConditionalFormatKind, Sheet};
use roe_model::calc::Workbook;
use crate::formula_hook::{eval_as_f64, FormulaEvaluator};

/// 从 CellValue 提取数值，非数值返回 None（dataBar/colorScale 只对数值生效）
fn cell_as_f64(val: &CellValue) -> Option<f64> {
    match val {
        CellValue::Number(n) => Some(*n),
        _ => None,
    }
}

/// 解析十六进制颜色 "#RRGGBB" → [u8;3]，解析失败返回灰色兜底
fn parse_rgb(s: &str) -> [u8; 3] {
    let s = s.trim_start_matches('#');
    if s.len() >= 6 {
        let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0x80);
        let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0x80);
        let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0x80);
        [r, g, b]
    } else {
        [0x80, 0x80, 0x80]
    }
}

/// 收集 range 内所有数值单元格的值（用于 min/max/percentile 阈值解析）
fn collect_range_values(sheet: &Sheet, r1: u32, c1: u32, r2: u32, c2: u32) -> Vec<f64> {
    let mut vals = Vec::new();
    for row_idx in r1..=r2 {
        for col_idx in c1..=c2 {
            if let Some(n) = cell_as_f64(&sheet.cells.get(row_idx, col_idx)) {
                vals.push(n);
            }
        }
    }
    vals
}

/// 解析形如 "$A$1" / "A1" / "$A1" / "A$1" 的单个单元格引用字符串，
/// 返回 0-based (row, col)。不支持跨sheet引用（如 "Sheet2!A1"）、
/// range引用（如 "A1:B2"）、函数调用等复杂公式——这些会在此函数
/// 返回 None，调用方据此退化到现有的 data_min/data_max 字面量兜底。
///
/// ROOT-071(v131 TODO)：这是简化实现，仅覆盖"cfvo.val 恰好是纯单元格
/// 引用"这一种情形（本次修复的触发用例：colorscale_num_with_ref.xlsx
/// 的 cfvo type="num" val="$A$1"）。真正完整的语义应该是：cfvo的
/// num/formula类型的val本质上都是一段有待求值的公式（LOK
/// ScColorScaleEntry::GetValue()对两者一视同仁，统一构造
/// ScFormulaCell交给公式引擎求值），我们项目里已有的公式引擎
/// formualizer-eval/roe-calc具备这个能力，但roe-calc依赖roe-core
/// （编译期已确认，见Cargo.toml），若在roe-core里反向依赖roe-calc
/// 会形成循环依赖，编译不过。将来如果遇到更复杂的cfvo公式场景
/// （如 val="2*A1+3" 这种非纯引用的表达式），建议的重构方向是
/// "依赖注入"：给resolve_cfvo（或其调用方render_databar_and_colorscale）
/// 增加一个 `eval_hook: Option<&dyn Fn(&str) -> Option<f64>>` 参数，
/// 由roe-calc侧构造真正调用formula_engine::eval_formula的闭包
/// 传入，roe-core本身不需要知道roe-calc/formualizer-eval的存在，
/// 从而绕开循环依赖，同时具备完整公式求值能力。这是一次架构层面
/// 的改动，涉及调用链上多个函数签名，建议开专门会话讨论范围后
/// 再动手，不在本次修复范围内。
fn parse_cell_ref(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let bytes = s.as_bytes();
    let mut i = 0;
    // 跳过可选的列绝对引用符 $
    if i < bytes.len() && bytes[i] == b'$' {
        i += 1;
    }
    let col_start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == col_start {
        return None; // 没有列字母
    }
    let col_letters = &s[col_start..i];
    // 跳过可选的行绝对引用符 $
    if i < bytes.len() && bytes[i] == b'$' {
        i += 1;
    }
    let row_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == row_start || i != bytes.len() {
        return None; // 没有行数字，或者后面还有多余字符（如range/sheet引用）
    }
    let row_num: u32 = s[row_start..i].parse().ok()?;
    if row_num == 0 {
        return None;
    }
    let mut col: u32 = 0;
    for ch in col_letters.chars() {
        if !ch.is_ascii_alphabetic() {
            return None;
        }
        col = col.saturating_mul(26).saturating_add(ch.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
    }
    if col == 0 {
        return None;
    }
    // Excel是1-based，我们的sheet.cells索引是0-based
    Some((row_num - 1, col - 1))
}

/// 将 CfValue 阈值解析为具体数值，需要 range 内实际数据辅助 min/max/percent/percentile 计算
fn resolve_cfvo(
    cfvo: &CfValue,
    sorted_vals: &[f64],
    sheet: &Sheet,
    workbook: &Workbook,
    evaluator: Option<&dyn FormulaEvaluator>,
) -> f64 {
    if sorted_vals.is_empty() {
        return cfvo.val.parse::<f64>().unwrap_or(0.0);
    }
    let data_min = sorted_vals[0];
    let data_max = sorted_vals[sorted_vals.len() - 1];
    match cfvo.kind.as_str() {
        // ROOT-066(v128): x14扩展dataBar常用autoMin/autoMax（Excel"自动
        // 最小值/自动最大值"选项），语义上等价于legacy的min/max
        // （取range内数据实际最小/最大值），此前未处理时落入_=>data_min
        // 兜底分支，导致min_v==max_v==data_min，span退化为极小值，
        // 所有ratio被clamp到1.0，表现为"所有条形一样长"。
        "min" | "autoMin" => data_min,
        "max" | "autoMax" => data_max,
        // ROOT-078 (eval_hook)：cfvo.val 本质上是一段有待求值的公式
        // （纯数字字面量、纯单元格引用、真正的表达式如"2*A1+3"都是
        // 合法输入），优先交给公式引擎求值，能覆盖比此前手搓
        // parse_cell_ref（只认纯引用）更广的场景。evaluator 不可用
        // （None，如未接入 roe-api 的独立单元测试）或求值失败（非数值
        // 结果/公式出错）时，保留原有的分层兜底不变：先尝试直接
        // parse::<f64>()（cfvo.val 本身就是纯数字字符串的常见情形），
        // 再尝试 parse_cell_ref 识别纯单元格引用手动查值，最后退化到
        // data_min 字面量兜底（ROOT-071注释：空单元格/非数值/复杂公式
        // 场景）。
        "num" => {
            if let Some(ev) = evaluator {
                if let Some(n) = eval_as_f64(ev, &cfvo.val, workbook, sheet) {
                    return n;
                }
            }
            cfvo.val.parse::<f64>().unwrap_or_else(|_| {
                parse_cell_ref(&cfvo.val)
                    .and_then(|(r, c)| cell_as_f64(&sheet.cells.get(r, c)))
                    .unwrap_or(data_min)
            })
        }
        "percent" => {
            let p = cfvo.val.parse::<f64>().unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
            data_min + (data_max - data_min) * p
        }
        "percentile" => {
            let p = cfvo.val.parse::<f64>().unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
            let idx = ((sorted_vals.len() as f64 - 1.0) * p).round() as usize;
            sorted_vals[idx.min(sorted_vals.len() - 1)]
        }
        // ROOT-078 (eval_hook)：formula 阈值同样优先交给公式引擎求值。
        // evaluator 不可用/求值失败时，保留原有兜底：先尝试直接
        // parse::<f64>()，再退化到 data_max（ROOT-066第二轮注释：
        // formula通常表达最大阈值上限，用data_max兜底比data_min更
        // 合理，能让条形呈现正常相对比例，即便仍不是公式真实值）。
        "formula" => {
            if let Some(ev) = evaluator {
                if let Some(n) = eval_as_f64(ev, &cfvo.val, workbook, sheet) {
                    return n;
                }
            }
            cfvo.val.parse::<f64>().unwrap_or(data_max)
        }
        _ => data_min,
    }
}

/// ROOT-066(v128) 第二轮重写：完全照搬 LOK
/// `sc/source/core/data/colorscale.cxx::ScDataBarFormat::GetDataBarInfo`
/// 算法，替换此前基于"当前数据是否跨零"的错误猜测实现。
///
/// 关键点：是否留负值区域看的是**cfvo阈值 min_v/max_v**，不是看当前
/// range实际数据是否有负数（H组数据全正但min阈值=-2，仍应留负值区域）。
///
/// 返回 (zero_pct, length_pct)：
/// - zero_pct：0轴在单元格宽度上的百分比位置（0~100）
/// - length_pct：当前值对应条形的长度百分比（可正可负）。正值从zero_pct
///   往右画，负值从zero_pct往左画。
///
/// `min_is_auto`/`max_is_auto`：cfvo阈值类型是否为"自动"类型
/// （legacy min/max 或 x14 autoMin/autoMax 均视为auto，因为它们的语义
/// 都是"取数据实际最值"，对应LOK里 AUTO/MAX 阈值类型的钳位规则）。
fn databar_zero_and_length(
    min_is_auto: bool,
    mut min_v: f64,
    max_is_auto: bool,
    mut max_v: f64,
    value: f64,
    axis_position: &str,
) -> (f64, f64) {
    match axis_position {
        "none" => {
            // 0轴固定最左，不分正负，[min_v,max_v]线性映射到[0,100]
            let zero = 0.0;
            let length = if value <= min_v {
                0.0
            } else if value >= max_v {
                100.0
            } else if (max_v - min_v).abs() > 1e-12 {
                (value - min_v) / (max_v - min_v) * 100.0
            } else {
                0.0
            };
            (zero, length)
        }
        "middle" => {
            // 0轴固定单元格正中
            let zero = 50.0;
            let abs_max = min_v.abs().max(max_v.abs()).max(1e-9);
            let length = if value < 0.0 && min_v < 0.0 {
                if value < min_v {
                    100.0 * (min_v / abs_max)
                } else {
                    100.0 * (value / abs_max)
                }
            } else if value > max_v {
                100.0 * (max_v / abs_max)
            } else {
                100.0 * (value.max(min_v) / abs_max)
            };
            (zero, length)
        }
        _ => {
            // "automatic"（默认/最常见）
            // 边界修正：仅当阈值类型是"自动"时才钳位
            if min_is_auto && min_v > 0.0 {
                min_v = 0.0;
            }
            if max_is_auto && max_v < 0.0 {
                max_v = 0.0;
            }

            let zero = if min_v >= 0.0 {
                0.0
            } else if max_v < 0.0 {
                100.0
            } else {
                -100.0 * min_v / (max_v - min_v)
            };

            let min_non_negative = min_v.max(0.0);
            let max_non_positive = max_v.min(0.0);

            let length = if value < 0.0 && min_v < 0.0 {
                if value < min_v {
                    -100.0
                } else {
                    let denom = min_v - max_non_positive;
                    if denom.abs() > 1e-12 {
                        -100.0 * (value - max_non_positive) / denom
                    } else {
                        0.0
                    }
                }
            } else if value > max_v {
                100.0
            } else if value <= min_v {
                0.0
            } else {
                let denom = max_v - min_non_negative;
                if denom.abs() > 1e-12 {
                    100.0 * (value - min_non_negative) / denom
                } else {
                    0.0
                }
            };
            (zero, length)
        }
    }
}

/// ROOT-066(v128): 把颜色朝白色方向插值，t越大越接近白色（越浅）。
/// 用于data bar渐变的"浅端"起点色，t=0.55是经验值，参照OnlyOffice/Excel
/// 默认data bar渐变的视觉浅深对比度目测调校，非来自任何规范文档。
fn lerp_toward_white(c: [u8; 3], t: f64) -> [u8; 3] {
    lerp_rgb(c, [0xff, 0xff, 0xff], t)
}

/// 线性插值两个 RGB 颜色：t in [0,1]
fn lerp_rgb(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        (a[0] as f64 + (b[0] as f64 - a[0] as f64) * t).round() as u8,
        (a[1] as f64 + (b[1] as f64 - a[1] as f64) * t).round() as u8,
        (a[2] as f64 + (b[2] as f64 - a[2] as f64) * t).round() as u8,
    ]
}

/// ROOT-009部分修复(v123)：ColorScale颜色选段，移植自LOK
/// sc/source/core/data/colorscale.cxx::ScColorScaleFormat::GetColor。
///
/// entries 是按阈值升序排列的 (阈值, 颜色, 是否percentile类型) 列表
/// （2段=min/max，3段=min/mid/max）。逐段判断 n 落在哪个 [nValMin, nValMax]
/// 区间内，区间内两色线性插值。
///
/// 关键点（对应LOK的tdf#155321修复）：当某一段的上界是percentile类型，
/// 且 n 恰好等于该段上界值、也等于全局max（典型场景：数据全部相同/
/// 单一数据点，导致min==percentile==max三者退化为同一个值）时，
/// 不能停留在当前段（会得到"最低档"颜色，如全表最大值却显示成红色），
/// 而要继续前进到下一段，借用下一段的上界颜色（如三色阶的max绿色）。
/// 这是LOK为了修复"数据全部相同时颜色档位反直觉"这一具体bug
/// （tdf#155321）加的特殊处理，经与LibreOffice/OnlyOffice实测比对
/// （tdf156028.xlsx单值数据样本，三者一致显示为max颜色/绿色）确认
/// 是正确的预期行为，非LOK自身的偶然实现细节。
fn colorscale_pick_color(n: f64, entries: &[(f64, [u8; 3], bool)]) -> [u8; 3] {
    if entries.len() < 2 {
        return entries.first().map(|e| e.1).unwrap_or([0x80, 0x80, 0x80]);
    }
    let global_max = entries[entries.len() - 1].0;

    let mut idx = 0usize;
    let (mut val_min, mut col_min, _) = entries[idx];
    idx += 1;
    let (mut val_max, mut col_max, mut max_is_percentile) = entries[idx];

    // tdf#155321：当前段上界是percentile类型，且n等于全局max也等于该段
    // 上界值——数据全部相同/单点数据的退化场景，需要借用下一段颜色。
    let mut b_equal = max_is_percentile && n == global_max && n == val_max;

    // ROOT-072(v131)：部分退化场景——当前段本身长度为0（val_min==val_max，
    // 例如min阈值和mid阈值恰好算出同一个数值），且n已经达到这个重合值，
    // 但后面还有长度不为0的正常段。此前只有tdf#155321的b_equal分支会
    // 触发"借用下一段颜色"，但那个分支的触发条件更窄（要求mid是percentile
    // 类型且n等于全局max），不覆盖"n只是恰好等于min/mid重合值，不是
    // 全局max"这种更常见的场景（如colorscale.xlsx D3：min=percentile(10)=2.0，
    // mid=num(2)=2.0，n=2.0，但max=5.5不重合）。此前实现下，长度为0的段
    // 会因为span被max(1e-9)保护而始终算出t=0，错误取到该段的min端颜色。
    // 修复：只要当前段退化（val_min==val_max）且n达到该值，就应该无条件
    // 推进到下一段，直接反映"n已经跨过这个退化阈值点"的语义，不受
    // percentile/全局max限制。
    let is_degenerate_segment = val_min == val_max && n >= val_max;

    idx += 1;
    while idx < entries.len() && (n > val_max || b_equal || is_degenerate_segment) {
        col_min = col_max;
        val_min = if !b_equal { val_max } else { val_max - 1.0 };
        let (v, c, is_pct) = entries[idx];
        col_max = c;
        val_max = v;
        max_is_percentile = is_pct;
        b_equal = false; // 只在紧邻max那一段触发一次，避免多段重复借色
        let _ = max_is_percentile;
        idx += 1;
    }

    let span = (val_max - val_min).abs().max(1e-9);
    let t = ((n - val_min) / span).clamp(0.0, 1.0);
    lerp_rgb(col_min, col_max, t)
}

/// 渲染 sheet.cond_formats 中的 DataBar / ColorScale 规则（在指定 tile 可见范围内）。
/// IconSet 暂不处理。在网格线之上、文字之下调用（与 tile.rs 现有 CellFormat 背景色渲染同一时机）。
#[allow(clippy::too_many_arguments)]
pub fn render_databar_and_colorscale(
    pixmap: &mut PixmapMut,
    sheet: &Sheet,
    cfg: &CalcRenderConfig,
    tx: Transform,
    row_start: u32,
    row_end: u32,
    col_start: u32,
    col_end: u32,
    // ROOT-078 (eval_hook)：cfvo formula/num 类型阈值求值所需。见
    // resolve_cfvo 内部说明；evaluator=None 时退化为既有字面量/
    // 单元格引用兜底，不影响原有行为。
    workbook: &Workbook,
    evaluator: Option<&dyn FormulaEvaluator>,
) {
    let mut paint = Paint::default();
    paint.anti_alias = false;

    for rule in &sheet.cond_formats {
        let r1 = rule.range.start.0;
        let c1 = rule.range.start.1;
        let r2 = rule.range.end.0;
        let c2 = rule.range.end.1;
        // 只处理与本 tile 有交叉的区域
        if r2 < row_start || r1 >= row_end { continue; }
        if c2 < col_start || c1 >= col_end { continue; }

        match &rule.kind {
            ConditionalFormatKind::DataBar {
                min, max, bar_color, negative_fill_color, axis_color, axis_position, solid_fill, ..
            } => {
                // ROOT-066(v128)：此前min/max只来自legacy占位值(val=0/0)，
                // 现在read_worksheet阶段已用x14扩展的真实阈值patch过
                // （见xlsx.rs parse_x14_cond_formats_from_xml调用处），
                // 这里可以直接信任min/max是真实值。
                let sorted_vals = {
                    let mut v = collect_range_values(sheet, r1, c1, r2, c2);
                    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    v
                };
                // ROOT-066(v128) 第二轮：formula类型阈值不求值、退化为
                // data_min/data_max字面量兜底（见resolve_cfvo），语义上等价于
                // "自动"阈值，因此纳入is_auto判断，使automatic模式的边界钳位
                // 规则（如"AUTO类型且nMin>0时归零"）能正确应用到它身上。
                // ROOT-066(v128) 第四轮修正：普通"min"/"max"类型（用户固定阈值）
                // 不属于"自动"阈值，不应触发LOK automatic模式的边界钳位规则
                // （"AUTO类型且nMin>0时归零"）。只有"autoMin"/"autoMax"（Excel"自动
                // 最小值/自动最大值"选项）才是真正的自动阈值。此前把两者混同处理，
                // 导致D组（x14类型是普通min/max，阈值应为data_min=1/data_max=4，
                // D3的value=1恰好等于min_v不应画条）被错误钳位成min_v=0，画出了
                // LibreOffice/OnlyOffice都不会画的多余条形。
                // formula类型因不求值、退化为data_min/data_max字面量兜底，语义上
                // 更接近"自动"（本来就没有真实阈值可用），仍归入is_auto。
                let min_is_auto = matches!(min.kind.as_str(), "autoMin" | "formula");
                let max_is_auto = matches!(max.kind.as_str(), "autoMax" | "formula");
                let min_v = resolve_cfvo(min, &sorted_vals, sheet, workbook, evaluator);
                let max_v = resolve_cfvo(max, &sorted_vals, sheet, workbook, evaluator);

                let pos_color = parse_rgb(bar_color);
                let neg_color = negative_fill_color
                    .as_deref()
                    .map(parse_rgb)
                    .unwrap_or_else(|| {
                        // Excel默认：未显式指定负值颜色时用正色的反色系（红），
                        // 这里退化为纯红兜底，比continue跳过负值更接近真实效果。
                        [0xff, 0x00, 0x00]
                    });

                for row_idx in r1.max(row_start)..=r2.min(row_end.saturating_sub(1)) {
                    for col_idx in c1.max(col_start)..=c2.min(col_end.saturating_sub(1)) {
                        let n = match cell_as_f64(&sheet.cells.get(row_idx, col_idx)) {
                            Some(n) => n,
                            None => continue,
                        };

                        let cell_x = col_x_px(sheet, col_idx, cfg) + 1.0;
                        let cell_y = row_y_px(sheet, row_idx, cfg) + 1.0;
                        let cell_w = (col_width_at_px(sheet, col_idx, cfg) - 2.0).max(0.0);
                        let cell_h = (row_height_at_px(sheet, row_idx, cfg) - 2.0).max(0.0);
                        if cell_w <= 0.0 || cell_h <= 0.0 { continue; }

                        // ROOT-066(v128) 第二轮：改用LOK GetDataBarInfo算法算出
                        // zero_pct（0轴位置百分比）+ length_pct（条形长度百分比，
                        // 可正可负）。是否留负值区域看的是cfvo阈值min_v/max_v，
                        // 不是当前数据本身是否有负数。
                        let (zero_pct, length_pct) = databar_zero_and_length(
                            min_is_auto, min_v, max_is_auto, max_v, n, axis_position.as_str(),
                        );

                        let axis_x = cell_x + cell_w * (zero_pct / 100.0) as f32;
                        let length_ratio = (length_pct / 100.0) as f32;

                        let (color, bar_w, bar_x) = if length_ratio < 0.0 {
                            // 负值：从axis_x向左延伸
                            let avail_w = (axis_x - cell_x).max(0.0);
                            let bw = avail_w * (-length_ratio).min(1.0);
                            (neg_color, bw, axis_x - bw)
                        } else {
                            // 正值：从axis_x向右延伸
                            let avail_w = (cell_x + cell_w - axis_x).max(0.0);
                            let bw = avail_w * length_ratio.min(1.0);
                            (pos_color, bw, axis_x)
                        };

                        if bar_w <= 0.0 { continue; }

                        // ROOT-066(v128) 第三轮：参照LOK sc/source/ui/view/output.cxx
                        // drawDataBars实现，渐变是"实色(轴心端) -> 透明(远端)"，不是
                        // "实色->白色"（用透明比白色更通用，不依赖单元格背景色是纯白）。
                        // 方向：轴心附近保持实色，向远端过渡到透明，与Google Sheets/
                        // Excel视觉参考一致（此前实现方向反了：远端却变深）。
                        let (grad_start_x, grad_end_x) = if length_ratio < 0.0 {
                            (bar_x + bar_w, bar_x) // 负值条：右端(靠近轴心)实色，左端(远端)透明
                        } else {
                            (bar_x, bar_x + bar_w) // 正值条：左端(靠近轴心)实色，右端(远端)透明
                        };

                        // ROOT-066(v128) 第五轮修复：此前从未检查solid_fill字段，
                        // 恒用渐变，导致xlsx里显式设置"实心条"(gradient="0")的databar规则
                        // 也被画成渐变（cond_format_theme_color2/3等用例回归）。
                        if *solid_fill {
                            paint.shader = tiny_skia::Shader::SolidColor(
                                tiny_skia::Color::from_rgba8(color[0], color[1], color[2], 0xff)
                            );
                        } else {
                            let shader = tiny_skia::LinearGradient::new(
                                tiny_skia::Point::from_xy(grad_start_x, cell_y),
                                tiny_skia::Point::from_xy(grad_end_x, cell_y),
                                vec![
                                    tiny_skia::GradientStop::new(0.0, tiny_skia::Color::from_rgba8(color[0], color[1], color[2], 0xff)),
                                    tiny_skia::GradientStop::new(1.0, tiny_skia::Color::from_rgba8(color[0], color[1], color[2], 0x00)),
                                ],
                                tiny_skia::SpreadMode::Pad,
                                Transform::identity(),
                            );

                            if let Some(sh) = shader {
                                paint.shader = sh;
                            } else {
                                paint.set_color_rgba8(color[0], color[1], color[2], 0xff);
                            }
                        }

                        if let Some(rect) = Rect::from_xywh(bar_x, cell_y, bar_w, cell_h) {
                            let path = PathBuilder::from_rect(rect);
                            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tx, None);
                        }

                        // ROOT-066(v128) 第三轮：参照LOK drawDataBars，仅当0轴不在
                        // 单元格两端（即zero_pct既不是0也不是100，说明该单元格视觉上
                        // 需要区分正负两侧）时才画轴线，退化为单色矩形（细竖线），
                        // 颜色用axis_color，未提供时兜底黑色。
                        if zero_pct > 0.0 && zero_pct < 100.0 {
                            let ac = axis_color.as_deref().map(parse_rgb).unwrap_or([0x00, 0x00, 0x00]);
                            paint.shader = tiny_skia::Shader::SolidColor(
                                tiny_skia::Color::from_rgba8(ac[0], ac[1], ac[2], 0xff)
                            );
                            let axis_line_w: f32 = 1.0;
                            if let Some(axis_rect) = Rect::from_xywh(
                                axis_x - axis_line_w / 2.0, cell_y, axis_line_w, cell_h,
                            ) {
                                let axis_path = PathBuilder::from_rect(axis_rect);
                                pixmap.fill_path(&axis_path, &paint, tiny_skia::FillRule::Winding, tx, None);
                            }
                        }
                    }
                }
                // 恢复纯色shader，避免影响后续ColorScale等分支复用同一个paint对象
                paint.shader = tiny_skia::Shader::SolidColor(tiny_skia::Color::BLACK);
            }

            ConditionalFormatKind::ColorScale {
                min_type, min_value, min_color,
                mid, max_type, max_value, max_color,
            } => {
                let sorted_vals = {
                    let mut v = collect_range_values(sheet, r1, c1, r2, c2);
                    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    v
                };
                let min_cfvo = CfValue { kind: min_type.clone(), val: min_value.clone().unwrap_or_default(), gte: true };
                let max_cfvo = CfValue { kind: max_type.clone(), val: max_value.clone().unwrap_or_default(), gte: true };
                let min_v = resolve_cfvo(&min_cfvo, &sorted_vals, sheet, workbook, evaluator);
                let max_v = resolve_cfvo(&max_cfvo, &sorted_vals, sheet, workbook, evaluator);
                let min_rgb = parse_rgb(min_color);
                let max_rgb = parse_rgb(max_color);

                // ROOT-009部分修复(v123)：三段（min/mid/max）表示为统一的
                // (阈值, 颜色, 是否percentile类型) 列表，按LOK
                // sc/source/core/data/colorscale.cxx::ScColorScaleFormat::GetColor
                // 的逐段迭代逻辑重写（原实现是简化的"固定两段插值"，无法
                // 处理阈值退化重合的边界情况，如min==mid==max）。
                let mid_point: Option<(f64, [u8; 3], bool)> = mid.as_ref().map(|(mk, mv, mc)| {
                    let mid_cfvo = CfValue { kind: mk.clone(), val: mv.clone(), gte: true };
                    (resolve_cfvo(&mid_cfvo, &sorted_vals, sheet, workbook, evaluator), parse_rgb(mc), mk == "percentile")
                });
                let entries: Vec<(f64, [u8; 3], bool)> = match mid_point {
                    Some(mp) => vec![(min_v, min_rgb, min_type == "percentile"), mp, (max_v, max_rgb, max_type == "percentile")],
                    None => vec![(min_v, min_rgb, min_type == "percentile"), (max_v, max_rgb, max_type == "percentile")],
                };

                for row_idx in r1.max(row_start)..=r2.min(row_end.saturating_sub(1)) {
                    for col_idx in c1.max(col_start)..=c2.min(col_end.saturating_sub(1)) {
                        let n = match cell_as_f64(&sheet.cells.get(row_idx, col_idx)) {
                            Some(n) => n,
                            None => continue,
                        };

                        let color = colorscale_pick_color(n, &entries);

                        paint.set_color_rgba8(color[0], color[1], color[2], 0xff);
                        let x = col_x_px(sheet, col_idx, cfg) + 1.0;
                        let y = row_y_px(sheet, row_idx, cfg) + 1.0;
                        let w = (col_width_at_px(sheet, col_idx, cfg) - 2.0).max(0.0);
                        let h = (row_height_at_px(sheet, row_idx, cfg) - 2.0).max(0.0);
                        if let Some(rect) = Rect::from_xywh(x, y, w, h) {
                            let path = PathBuilder::from_rect(rect);
                            pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tx, None);
                        }
                    }
                }
            }

            ConditionalFormatKind::CellFormat { .. } => {
                // CellFormat 已在 tile.rs 处理
            }

            ConditionalFormatKind::IconSet {
                icon_style,
                thresholds,
                percent,
                reverse,
                icons_only,
                tier_overrides,
            } => {
                crate::icon_set_render::render_icon_set(
                    pixmap,
                    sheet,
                    cfg,
                    tx,
                    row_start,
                    row_end,
                    col_start,
                    col_end,
                    icon_style,
                    thresholds,
                    *percent,
                    *reverse,
                    *icons_only,
                    tier_overrides,
                    r1,
                    c1,
                    r2,
                    c2,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ROOT-009部分修复(v123)：验证colorscale_pick_color对退化场景
    // （min==mid==max，单一数据点/数据全部相同）的处理，对齐LOK
    // tdf#155321修复行为：应选中max颜色（最高档），而不是min颜色。
    #[test]
    fn test_colorscale_pick_color_degenerate_single_value() {
        let red = [0xF8, 0x69, 0x6B];
        let yellow = [0xFF, 0xEB, 0x84];
        let green = [0x63, 0xBE, 0x7B];
        // min=1(非percentile), mid=percentile50=1, max=1(非percentile)
        let entries = vec![
            (1.0, red, false),
            (1.0, yellow, true),
            (1.0, green, false),
        ];
        let color = colorscale_pick_color(1.0, &entries);
        assert_eq!(color, green, "单点数据(min=mid=max)应借用max颜色(绿色)，对齐LOK tdf#155321行为");
    }

    // 正常场景回归：三档分布均匀时，min/mid/max分别取端点颜色，不应
    // 触发tdf#155321借色分支（因为mid不是percentile类型触发条件，或
    // 各阈值本身不重合）。
    #[test]
    fn test_colorscale_pick_color_normal_distribution() {
        let red = [0xF8, 0x69, 0x6B];
        let yellow = [0xFF, 0xEB, 0x84];
        let green = [0x63, 0xBE, 0x7B];
        let entries = vec![
            (0.0, red, false),
            (50.0, yellow, false),
            (100.0, green, false),
        ];
        assert_eq!(colorscale_pick_color(0.0, &entries), red);
        assert_eq!(colorscale_pick_color(100.0, &entries), green);
        // 中点应该是mid颜色本身（t=0在该段边界）
        assert_eq!(colorscale_pick_color(50.0, &entries), yellow);
    }

    #[test]
    fn test_colorscale_pick_color_two_stop() {
        // 二色（无mid）场景回归，min/max插值
        let red = [0xF8, 0x69, 0x6B];
        let green = [0x63, 0xBE, 0x7B];
        let entries = vec![(0.0, red, false), (10.0, green, false)];
        assert_eq!(colorscale_pick_color(0.0, &entries), red);
        assert_eq!(colorscale_pick_color(10.0, &entries), green);
    }

    // ROOT-072(v131)：复现 sc_data_xlsx__colorscale.xlsx Sheet2 D3 bug。
    // min=percentile(10)=2.0 与 mid=num(2)=2.0 恰好重合（部分退化，
    // 不同于ROOT-009覆盖的min==mid==max三重全等退化），max=5.5不重合。
    // n=2.0 应该显示mid颜色（暗红），因为min/mid共同定义了"值为2.0时
    // 已经达到mid档位"，不应该因为第一段(min->mid)长度为0就被clamp
    // 在min端颜色（黄）。此前实现在此场景下错误返回黄色。
    #[test]
    fn test_colorscale_pick_color_partial_degenerate_min_eq_mid() {
        let yellow = [0xFF, 0xFF, 0x00];
        let maroon = [0x80, 0x00, 0x00];
        let green = [0x00, 0xFF, 0x00];
        let entries = vec![
            (2.0, yellow, true),
            (2.0, maroon, false),
            (5.5, green, false),
        ];
        assert_eq!(
            colorscale_pick_color(2.0, &entries),
            maroon,
            "min==mid重合时，n等于该重合值应取mid颜色而非min颜色"
        );
        // 顺带验证正常段仍然按预期插值，不受本次修复影响
        assert_eq!(colorscale_pick_color(5.5, &entries), green);
    }
}

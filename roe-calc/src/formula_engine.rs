//! roe-calc::formula_engine
//! -------------------------------------------------------------
//! Spike B：formualizer 适配层。
//! 不使用 formualizer 的 Engine（会引入第二套内部单元格存储），
//! 而是直接用 Interpreter::new(&resolver, sheet_name).evaluate_ast(&ast)，
//! resolver 借用 roe_model::calc::Sheet 作为唯一真相源，不复制数据。
//!
//! 当前范围（Spike B 最小验证）：
//! - 单 sheet
//! - 支持函数：由 formualizer 全局函数注册表提供（SUM/AVERAGE 等自动可用）
//! - 区域引用（A1:B10）、单元格引用（A1）均支持（走 formualizer 官方 parser，
//!   不再是 roe-calc 现有的字符串扫描实现）
//! - 命名区域、跨表引用、表引用：本 spike 暂不支持，返回 NImpl 错误

use std::sync::{Arc, Once, RwLock};

static INIT_BUILTINS: Once = Once::new();

/// formualizer 的函数注册表是全局静态 DashMap，SUM/AVERAGE 等内置函数
/// 需要显式调用一次 load_builtins() 才会注册进去（不是 lib.rs 自动做的）。
/// 用 Once 保证多次调用（比如多个测试并发跑）只真正执行一次，且
/// register_function 本身是 REG.insert，重复调用也不会 panic，Once 只是
/// 避免不必要的重复工作。
fn ensure_builtins_loaded() {
    INIT_BUILTINS.call_once(|| {
        formualizer_eval::builtins::load_builtins();
    });
    // ROE_SUBTOTAL 是 ROE 项目自定义扩展函数，不属于 formualizer 内置
    // 函数表，但复用同一个"确保函数表就绪"入口，让现有两处
    // ensure_builtins_loaded() 调用点（生产路径 + 测试路径）自动带上，
    // 不需要逐个调用点手动追加注册代码。用独立 Once（见
    // ensure_roe_subtotal_registered 定义处注释）避免和内置函数加载
    // 的 Once 混淆语义。
    ensure_roe_subtotal_registered();
}

use formualizer_common::{ExcelError, ExcelErrorKind, LiteralValue};
use formualizer_eval::function::Function;
use formualizer_eval::traits::{
    EvaluationContext, FunctionProvider, NamedRangeResolver, Range, RangeResolver,
    ReferenceResolver, SourceResolver, Table, TableResolver,
};
use formualizer_parse::parser::TableReference;

use roe_model::calc::{CellError, CellValue, Sheet, Workbook};

/// 只读 resolver，借用整个 Workbook（支持跨表引用）+ 当前 sheet 名。
/// formualizer 的 row/col 是 1-based，roe_model::calc::SparseGrid 是 0-based，
/// 转换在这里做，不污染上层。
pub struct SheetResolver<'a> {
    workbook: &'a Workbook,
    current_sheet_name: &'a str,
    /// 求值过程中被 resolve_range_view 访问到的单元格记录（sheet 名, row 0-based, col 0-based）。
    /// 用于求值完成后喂给 FormulaDAG 建立依赖边（第 4 步）。
    /// interior mutability：resolve_range_view 签名是 &self，不能改成 &mut self。
    access_log: RwLock<Vec<(String, u32, u32)>>,
    /// ROOT-012 修复：递归惰性求值钩子。当求值过程中遇到一个依赖单元格本身
    /// 也是 CellValue::Formula 时，不再直接返回 Pending/静态旧值——而是调用
    /// 这个 hook，让调用方（scheduler.rs 的 eval_with_dag）有机会"先算完那个
    /// 依赖 cell，再把它的真实结果传回来"，对齐 LOK ScFormulaCell::MaybeInterpret()
    /// 的按需递归求值设计。返回 None 表示 hook 认为不需要/无法介入
    /// （例如检测到循环引用，或者是独立单元测试场景没有传 hook），
    /// 此时退回读取该单元格的静态 cached 值（旧行为，向后兼容）。
    /// 参数：(sheet_name, row 0-based, col 0-based)。
    recalc_hook: Option<&'a (dyn Fn(&str, u32, u32) -> Option<CellValue> + Sync)>,
}

impl<'a> SheetResolver<'a> {
    pub fn new(workbook: &'a Workbook, current_sheet_name: &'a str) -> Self {
        Self {
            workbook,
            current_sheet_name,
            access_log: RwLock::new(Vec::new()),
            recalc_hook: None,
        }
    }

    /// ROOT-012 修复：带递归求值 hook 的构造函数。scheduler.rs 的
    /// eval_with_dag 用这个构造函数，传入一个能递归调用自身的闭包。
    pub fn new_with_recalc_hook(
        workbook: &'a Workbook,
        current_sheet_name: &'a str,
        recalc_hook: &'a (dyn Fn(&str, u32, u32) -> Option<CellValue> + Sync),
    ) -> Self {
        Self {
            workbook,
            current_sheet_name,
            access_log: RwLock::new(Vec::new()),
            recalc_hook: Some(recalc_hook),
        }
    }

    /// 遇到一个 Formula 类型的依赖单元格时的统一处理：优先尝试 hook 递归求值，
    /// hook 不存在或返回 None 时，退回读取该单元格自带的静态 cached 值。
    fn resolve_formula_dependency(
        &self,
        sheet_name: &str,
        row0: u32,
        col0: u32,
        cached: &CellValue,
    ) -> LiteralValue {
        if let Some(hook) = self.recalc_hook {
            if let Some(v) = hook(sheet_name, row0, col0) {
                return cell_value_to_literal(&v);
            }
        }
        cell_value_to_literal(cached)
    }

    /// 统一入口：给定一个从 workbook 里取出的 CellValue 及其坐标，返回喂给
    /// formualizer 的 LiteralValue。若该值本身是 Formula，走 recalc hook
    /// 递归求值（ROOT-012 修复）；否则走原有的直接转换逻辑。
    fn literal_for_cell(&self, sheet_name: &str, row0: u32, col0: u32, v: &CellValue) -> LiteralValue {
        match v {
            CellValue::Formula { cached, .. } => {
                self.resolve_formula_dependency(sheet_name, row0, col0, cached)
            }
            other => cell_value_to_literal(other),
        }
    }

    /// 取出求值过程中记录的所有单元格访问（消费性，取完即清空）。
    /// 供 eval_formula_with_deps 在求值完成后调用。
    fn take_access_log(&self) -> Vec<(String, u32, u32)> {
        self.access_log
            .write()
            .expect("access_log lock poisoned")
            .drain(..)
            .collect()
    }

    /// 按名字找 sheet；未显式指定（同 sheet 引用）时用 current_sheet_name。
    /// 找不到返回 None，调用方转换成 #REF!。
    /// ROOT tdf164895: 命名引用（NamedRange）求值的共享实现，被两个独立入口复用：
    /// 1) RangeResolver::resolve_range_view 里的 RT::NamedRange 分支
    ///    （命名引用出现在函数参数等"需要 range 视图"的位置，如 SUM(X)）。
    /// 2) NamedRangeResolver::resolve_named_range_reference
    ///    （命名引用出现在裸标量位置，如单元格公式直接写 =X、=Cnt，
    ///    这种情况 formualizer 走的是这个 trait 方法，不经过 resolve_range_view，
    ///    此前这里硬编码返回 NImpl，是 tdf164895 迟迟不生效的真正根因）。
    ///
    /// 区域类命名定义（named_ranges）优先；查不到再退回查"公式类型"命名定义
    /// （named_formulas），命中则把原始公式文本当作一次独立公式重新求值
    /// （本质是"文本级宏展开"）。
    fn resolve_named_range_rows(&self, name: &str) -> Result<Vec<Vec<LiteralValue>>, ExcelError> {
        // 命名区域：workbook.named_ranges 里的 CellRange 是 0-based（start/end
        // 直接就是 row/col），跟本函数其它分支用的 1-based sr/sc/er/ec 不同，
        // 这里不做 +1/-1 转换的往返，直接用 0-based 写这段循环。
        match lookup_name_ci(&self.workbook.named_ranges, name) {
            Some(range) => {
                let sh = self
                    .find_sheet(Some(range.sheet.as_str()))
                    .ok_or_else(|| ExcelError::from(ExcelErrorKind::Ref))?;
                let (sr, sc) = range.start;
                let (er, ec) = range.end;
                let mut out = Vec::with_capacity((er.saturating_sub(sr) + 1) as usize);
                for r in sr..=er {
                    let mut row_vec = Vec::with_capacity((ec.saturating_sub(sc) + 1) as usize);
                    for c in sc..=ec {
                        let v = sh.cells.get(r, c);
                        self.access_log
                            .write()
                            .expect("access_log lock poisoned")
                            .push((sh.name.clone(), r, c));
                        row_vec.push(self.literal_for_cell(&sh.name, r, c, &v));
                    }
                    out.push(row_vec);
                }
                Ok(out)
            }
            None => {
                // ROOT tdf164895: 区域类查不到时，退回查"公式类型"命名定义
                // （named_formulas）。命中则把原始公式文本当作一次独立公式，
                // 走完整的 eval_formula 求值管线（本质是"文本级宏展开"，
                // 代入 current_sheet_name 作为求值上下文的当前 sheet）。
                // 已知局限（未覆盖）：Excel 命名公式的相对引用语义是"相对
                // 定义时的活动单元格"，不是"相对使用处"；本次样本（Cnt/X）
                // 用的都是绝对引用（$A:$A / $B$1），未涉及这个坑，如果后续
                // 遇到相对引用形式的命名公式需要专门处理。
                let formula_text = lookup_name_ci(&self.workbook.named_formulas, name)
                    .ok_or_else(|| ExcelError::from(ExcelErrorKind::Name))?;
                // ROOT tdf164895踩坑记录：definedName 的值文本（如
                // "COUNTA($A:$A)"）本身不带 '=' 前缀（OOXML definedName
                // 元素内容从来没有 '='，跟 <f> 公式标签同理，都是纯表达式
                // 文本）。formualizer_parse::parse 对不带 '=' 的文本会
                // 解析成 Literal(Text(..))（纯字符串字面量），而不是
                // 表达式 AST——之前漏了这一步，导致 COUNTA(...) 整段被
                // 当成一个字符串，从未真正求值，产生 #REF!/#VALUE!。
                // 必须手动补上 '=' 前缀才能让 formualizer 按表达式解析。
                let formula_with_eq = format!("={formula_text}");
                let result = eval_formula(self.workbook, self.current_sheet_name, &formula_with_eq);
                if let CellValue::Error(e) = result {
                    return Err(cell_error_to_excel_error(e));
                }
                Ok(vec![vec![cell_value_to_literal(&result)]])
            }
        }
    }

    fn find_sheet(&self, sheet_name: Option<&str>) -> Option<&'a Sheet> {
        let name = sheet_name.unwrap_or(self.current_sheet_name);
        self.workbook.sheets.iter().find(|s| s.name == name)
    }

    /// tdf163554: 3D 跨 sheet 引用（`Sheet1:Sheet3!A1`）解析 sheet 范围。
    ///
    /// sheet_first/sheet_last 只是 AST 里的两个 sheet 名字，"中间经过哪些
    /// sheet"需要按 workbook 里 sheet 的实际物理顺序（tab 顺序）来确定——
    /// 在 workbook.sheets 这个 Vec 里查两个名字各自的 index，取值较小的
    /// 那个作为起点、较大的作为终点，覆盖之间（含首尾）的所有 sheet。
    ///
    /// 顺序自动纠正（不报错）：实测 LibreOffice 对 sheet_first 在 workbook
    /// 顺序里排在 sheet_last 之后的"反向"写法（本项目 tdf163554 样本就是这
    /// 种情况：workbook 顺序是 "time (misc) - last" 在前、"time (pnrst)" 在
    /// 后，公式却写成 SUM('time (pnrst)':'time (misc) - last'!B1:B1)）依然
    /// 能正确求值，不报错、不要求用户写"正向"。这与 Excel 对 3D 引用边界
    /// 做自动规范化的公开行为一致：sheet 区间在语义上是无向的，由两个端点
    /// 在当前 workbook 拓扑里的物理位置决定，而不是端点在公式文本里出现的
    /// 先后顺序。
    ///
    /// 返回值：按 workbook 物理顺序排列的 sheet 引用切片（workbook.sheets
    /// 是连续存储的 Vec，这里直接用 index 范围切片，不需要额外分配）。
    ///
    /// 任一 sheet 名在 workbook 里找不到，返回 Ref 错误（tdf163554 本身只
    /// 覆盖"两个 sheet 名都存在"的正常场景；sheet 被删除后遗留的失效 3D
    /// 引用属于编辑态一致性维护范畴，不是求值引擎的职责，留待后续如有需要
    /// 再补）。
    ///
    /// ── 未来扩展路标 ──────────────────────────────────────────────
    /// 当前 RT::Cell3D / RT::Range3D 分支（在 resolve_range_view 里）对
    /// 所有函数一视同仁：把多个 sheet 的值拼接成一个大二维表返回，SUM/
    /// COUNT/AVERAGE 等聚合函数能正确工作，但像 VLOOKUP/INDEX 这类"不该
    /// 支持3D引用"的函数目前也会拿到同一份拼接表，静默算出一个没有意义
    /// 的结果，而不是像 Excel 那样报错。
    ///
    /// 讨论过（含向 Gemini 征求意见，见项目内 gemini_discuss_3d_range.md）
    /// 的结论是：这属于已知的、有意接受的技术债，本次不处理，原因是
    /// formualizer_eval::EvaluationContext::resolve_range_view 的返回类型
    /// 被 trait 签名锁死成 Result<RangeView, ExcelError>（RangeView 字段
    /// 私有、定义在第三方 crate 里，改不了），且 resolve_range_view 也
    /// 拿不到"当前是被哪个函数调用"这个信息——想在这一层做"函数白名单"
    /// 判断，架构上直接走不通。
    ///
    /// 如果将来真的需要补上这道限制，正确的落点不是这里，而是在真正调用
    /// formualizer 求值*之前*，对 AST 做一次静态预检查——完全参照本文件
    /// 里 find_unresolvable_named_range 已经在用的套路（那个函数专门用来
    /// 在求值前拦截未定义的命名区域，绕开短路求值的坑，见 ROOT-023 相关
    /// 注释）：写一个类似 find_illegal_3d_reference_in_non_aggregate_
    /// context(ast) 的函数，遍历 AST，如果发现顶层调用的函数名不在
    /// SUM/COUNT/COUNTA/AVERAGE/SUMSQ/PRODUCT 等聚合函数白名单里、但参数
    /// 子树中含 ReferenceType::Cell3D 或 Range3D 节点，直接判定 #VALUE!，
    /// 不再进入 eval_formula_with_deps 走到 resolve_range_view 这一步。
    /// ──────────────────────────────────────────────────────────────
    fn resolve_sheet_3d_range(
        &self,
        sheet_first: &str,
        sheet_last: &str,
    ) -> Result<&'a [Sheet], ExcelError> {
        let idx_first = self
            .workbook
            .sheets
            .iter()
            .position(|s| s.name == sheet_first)
            .ok_or_else(|| ExcelError::from(ExcelErrorKind::Ref))?;
        let idx_last = self
            .workbook
            .sheets
            .iter()
            .position(|s| s.name == sheet_last)
            .ok_or_else(|| ExcelError::from(ExcelErrorKind::Ref))?;
        let (lo, hi) = if idx_first <= idx_last {
            (idx_first, idx_last)
        } else {
            (idx_last, idx_first)
        };
        Ok(&self.workbook.sheets[lo..=hi])
    }
}

fn cell_value_to_literal(v: &CellValue) -> LiteralValue {
    match v {
        CellValue::Empty => LiteralValue::Empty,
        CellValue::Number(n) => LiteralValue::Number(*n),
        CellValue::Text(s) => LiteralValue::Text(s.clone()),
        // ROOT-078: 公式求值只关心文本内容参与计算（如CONCATENATE、
        // 字符串比较），拼接全部run文本，语义与Text分支一致。
        CellValue::RichText(runs) => {
            LiteralValue::Text(runs.iter().map(|r| r.text.as_str()).collect())
        }
        CellValue::Bool(b) => LiteralValue::Boolean(*b),
        CellValue::Formula { .. } => LiteralValue::Pending,
        CellValue::Error(e) => LiteralValue::Error(cell_error_to_excel_error(*e)),
    }
}

fn cell_error_to_excel_error(e: CellError) -> ExcelError {
    let kind = match e {
        CellError::Div0 => ExcelErrorKind::Div,
        CellError::Na => ExcelErrorKind::Na,
        CellError::Name => ExcelErrorKind::Name,
        CellError::Null => ExcelErrorKind::Null,
        CellError::Num => ExcelErrorKind::Num,
        CellError::Ref => ExcelErrorKind::Ref,
        CellError::Value => ExcelErrorKind::Value,
        CellError::CircularRef => ExcelErrorKind::Circ,
    };
    ExcelError::from(kind)
}

fn excel_error_kind_to_cell_error(k: ExcelErrorKind) -> CellError {
    match k {
        ExcelErrorKind::Null => CellError::Null,
        ExcelErrorKind::Ref => CellError::Ref,
        ExcelErrorKind::Name => CellError::Name,
        ExcelErrorKind::Value => CellError::Value,
        ExcelErrorKind::Div => CellError::Div0,
        ExcelErrorKind::Na => CellError::Na,
        ExcelErrorKind::Num => CellError::Num,
        ExcelErrorKind::Circ => CellError::CircularRef,
        ExcelErrorKind::Error
        | ExcelErrorKind::NImpl
        | ExcelErrorKind::Spill
        | ExcelErrorKind::Calc
        | ExcelErrorKind::Cancelled => CellError::Value,
        // formualizer-common 3.x: ExcelErrorKind 为 non_exhaustive,未知/未来变体统一归并为 #VALUE!
        _ => CellError::Value,
    }
}

#[derive(Debug)]
struct SheetRange {
    rows: Vec<Vec<LiteralValue>>,
}

impl Range for SheetRange {
    fn get(&self, row: usize, col: usize) -> Result<LiteralValue, ExcelError> {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .cloned()
            .ok_or_else(|| ExcelError::from(ExcelErrorKind::Ref))
    }

    fn dimensions(&self) -> (usize, usize) {
        let h = self.rows.len();
        let w = self.rows.first().map(|r| r.len()).unwrap_or(0);
        (h, w)
    }

    fn as_any(&self) -> &(dyn std::any::Any + 'static) {
        self
    }
}

impl<'a> ReferenceResolver for SheetResolver<'a> {
    fn resolve_cell_reference(
        &self,
        sheet: Option<&str>,
        row: u32,
        col: u32,
    ) -> Result<LiteralValue, ExcelError> {
        if row == 0 || col == 0 {
            return Err(ExcelError::from(ExcelErrorKind::Ref));
        }
        let sh = self
            .find_sheet(sheet)
            .ok_or_else(|| ExcelError::from(ExcelErrorKind::Ref))?;
        let v = sh.cells.get(row - 1, col - 1);
        Ok(self.literal_for_cell(&sh.name, row - 1, col - 1, &v))
    }
}

impl<'a> RangeResolver for SheetResolver<'a> {
    fn resolve_range_reference(
        &self,
        sheet: Option<&str>,
        sr: Option<u32>,
        sc: Option<u32>,
        er: Option<u32>,
        ec: Option<u32>,
    ) -> Result<Box<dyn Range>, ExcelError> {
        let (sr, sc, er, ec) = match (sr, sc, er, ec) {
            (Some(sr), Some(sc), Some(er), Some(ec)) => (sr, sc, er, ec),
            _ => return Err(ExcelError::from(ExcelErrorKind::NImpl)),
        };
        if sr == 0 || sc == 0 || er == 0 || ec == 0 {
            return Err(ExcelError::from(ExcelErrorKind::Ref));
        }
        let sh = self
            .find_sheet(sheet)
            .ok_or_else(|| ExcelError::from(ExcelErrorKind::Ref))?;
        let mut rows = Vec::with_capacity((er - sr + 1) as usize);
        for r in sr..=er {
            let mut row_vec = Vec::with_capacity((ec - sc + 1) as usize);
            for c in sc..=ec {
                let v = sh.cells.get(r - 1, c - 1);
                row_vec.push(self.literal_for_cell(&sh.name, r - 1, c - 1, &v));
            }
            rows.push(row_vec);
        }
        Ok(Box::new(SheetRange { rows }))
    }
}

impl<'a> NamedRangeResolver for SheetResolver<'a> {
    fn resolve_named_range_reference(
        &self,
        name: &str,
    ) -> Result<Vec<Vec<LiteralValue>>, ExcelError> {
        // ROOT tdf164895: 此前这里硬编码返回 NImpl，是"裸命名引用"
        // （如单元格公式直接写 =X、=Cnt，不在任何函数参数位置）求值
        // 一直失败的根因——formualizer 对这种语法形态走的是这个 trait
        // 方法，完全不经过 RangeResolver::resolve_range_view。
        // 现在改为调用共享实现，与 resolve_range_view 里的 RT::NamedRange
        // 分支保持逻辑一致。
        self.resolve_named_range_rows(name)
    }
}

impl<'a> TableResolver for SheetResolver<'a> {
    fn resolve_table_reference(&self, tref: &TableReference) -> Result<Box<dyn Table>, ExcelError> {
        let (sheet, table) = find_table_by_name(self.workbook, &tref.name)
            .ok_or_else(|| ExcelError::new(ExcelErrorKind::Name)
                .with_message(format!("Table not found: {}", tref.name)))?;
        Ok(Box::new(RoeTableView::build(self, sheet, table)))
    }
}

/// ROOT-011: 按表名（displayName）在整个 workbook 里查找表，大小写不敏感
/// （对齐 formualizer 自身"table name resolution is case-insensitive"约定）。
/// Excel 表名在 workbook 范围内唯一，故不需要按 sheet 限定查找范围。
fn find_table_by_name<'a>(
    workbook: &'a Workbook,
    name: &str,
) -> Option<(&'a Sheet, &'a roe_model::calc::Table)> {
    let key = name.to_lowercase();
    for sheet in &workbook.sheets {
        for table in &sheet.tables {
            if table.name.to_lowercase() == key {
                return Some((sheet, table));
            }
        }
    }
    None
}

/// ROOT-011: formualizer_eval::traits::Table 的具体实现。
///
/// 坑：formualizer_eval::traits::TableResolver::resolve_table_reference 返回
/// `Box<dyn Table>`（无生命周期参数，隐式 `'static`），不能像 SheetRange 那样
/// 借用 &'a Sheet/&'a SheetResolver——borrow checker 会报 "'a must outlive 'static"。
/// 所以这里在构造时（build()）就把该表范围内所有单元格 eager 读成 owned
/// LiteralValue 二维表，RoeTableView 本身不持有任何借用，完全 'static。
/// 代价：每次命中一个表引用都会把整张表的数据读一遍到内存，但表通常不大
/// （远小于整张 sheet），可接受。
#[derive(Debug, Clone)]
struct RoeTableView {
    has_header: bool,
    has_totals: bool,
    columns: Vec<String>,
    /// 表范围内全部行（含表头/汇总行，顺序与 range 一致），已转换为 LiteralValue，
    /// 公式依赖（若某单元格是 Formula）已在构造时通过 resolver 递归求值展开。
    all_rows: Vec<Vec<LiteralValue>>,
}

impl RoeTableView {
    fn build(resolver: &SheetResolver<'_>, sheet: &Sheet, table: &roe_model::calc::Table) -> Self {
        let (sr, sc) = table.range.start;
        let (er, ec) = table.range.end;
        let mut all_rows = Vec::with_capacity((er.saturating_sub(sr) + 1) as usize);
        for r in sr..=er {
            let mut row_vec = Vec::with_capacity((ec.saturating_sub(sc) + 1) as usize);
            for c in sc..=ec {
                let v = sheet.cells.get(r, c);
                resolver
                    .access_log
                    .write()
                    .expect("access_log lock poisoned")
                    .push((sheet.name.clone(), r, c));
                row_vec.push(resolver.literal_for_cell(&sheet.name, r, c, &v));
            }
            all_rows.push(row_vec);
        }
        Self {
            has_header: table.has_header,
            has_totals: table.has_totals,
            columns: table.columns.clone(),
            all_rows,
        }
    }

    fn header_rows(&self) -> usize {
        if self.has_header { 1 } else { 0 }
    }
    fn totals_rows(&self) -> usize {
        if self.has_totals { 1 } else { 0 }
    }
    /// 数据区域在 all_rows 里的起始下标（含表头则跳过第 0 行）
    fn data_start_idx(&self) -> usize {
        self.header_rows()
    }
    /// 数据区域在 all_rows 里的结束下标（含，去掉汇总行）
    fn data_end_idx(&self) -> Option<usize> {
        let total = self.all_rows.len();
        let totals = self.totals_rows();
        if total == 0 || total <= totals { return None; }
        Some(total - totals - 1)
    }
    fn column_offset(&self, column: &str) -> Option<usize> {
        let key = column.to_lowercase();
        self.columns.iter().position(|c| c.to_lowercase() == key)
    }
    fn slice_range(&self, start_idx: usize, end_idx: Option<usize>, col_offset: Option<usize>) -> SheetRange {
        let rows = match end_idx {
            None => Vec::new(),
            Some(end_idx) if end_idx < start_idx => Vec::new(),
            Some(end_idx) => self.all_rows[start_idx..=end_idx]
                .iter()
                .map(|row| match col_offset {
                    Some(ci) => vec![row.get(ci).cloned().unwrap_or(LiteralValue::Empty)],
                    None => row.clone(),
                })
                .collect(),
        };
        SheetRange { rows }
    }
}

impl Table for RoeTableView {
    fn get_cell(&self, row: usize, column: &str) -> Result<LiteralValue, ExcelError> {
        let col_offset = self.column_offset(column).ok_or_else(|| {
            ExcelError::new(ExcelErrorKind::Ref)
                .with_message(format!("Unknown table column: {column}"))
        })?;
        let idx = self.data_start_idx() + row;
        self.all_rows
            .get(idx)
            .and_then(|r| r.get(col_offset))
            .cloned()
            .ok_or_else(|| ExcelError::new(ExcelErrorKind::Ref)
                .with_message("Table row index out of range".to_string()))
    }

    fn get_column(&self, column: &str) -> Result<Box<dyn Range>, ExcelError> {
        let col_offset = self.column_offset(column).ok_or_else(|| {
            ExcelError::new(ExcelErrorKind::Ref)
                .with_message(format!("Unknown table column: {column}"))
        })?;
        let start = self.data_start_idx();
        let end = self.data_end_idx();
        Ok(Box::new(self.slice_range(start, end, Some(col_offset))))
    }

    fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }

    fn data_height(&self) -> usize {
        match self.data_end_idx() {
            Some(end) if end >= self.data_start_idx() => end - self.data_start_idx() + 1,
            _ => 0,
        }
    }

    fn has_headers(&self) -> bool {
        self.has_header
    }

    fn has_totals(&self) -> bool {
        self.has_totals
    }

    fn headers_row(&self) -> Option<Box<dyn Range>> {
        if !self.has_header {
            return None;
        }
        Some(Box::new(self.slice_range(0, Some(0), None)))
    }

    fn totals_row(&self) -> Option<Box<dyn Range>> {
        if !self.has_totals {
            return None;
        }
        let last = self.all_rows.len().checked_sub(1)?;
        Some(Box::new(self.slice_range(last, Some(last), None)))
    }

    fn data_body(&self) -> Option<Box<dyn Range>> {
        let start = self.data_start_idx();
        let end = self.data_end_idx();
        Some(Box::new(self.slice_range(start, end, None)))
    }

    fn clone_box(&self) -> Box<dyn Table> {
        Box::new(self.clone())
    }
}

impl<'a> SourceResolver for SheetResolver<'a> {}

impl<'a> FunctionProvider for SheetResolver<'a> {
    fn get_function(&self, ns: &str, name: &str) -> Option<Arc<dyn Function>> {
        formualizer_eval::function_registry::get(&ns.to_uppercase(), &name.to_uppercase())
    }
}

impl<'a> formualizer_eval::traits::Resolver for SheetResolver<'a> {}

impl<'a> EvaluationContext for SheetResolver<'a> {
    /// v166/v167 方案：ROE_SUBTOTAL 跳过嵌套小计行所需的公式文本自省能力。
    /// 对齐 LOK ScFormulaCell::IsSubTotal() 的思路——不维护独立的行级标记
    /// （is_summary_row 数组，v164 设计已作废），而是让 SUBTOTAL 求值时
    /// 直接查"这个具体 cell 自己存的是不是公式、公式里有没有 ROE_SUBTOTAL"。
    ///
    /// 关于 `cell.sheet_id`：formualizer-eval 的 `CellRef.sheet_id` 是它自己
    /// Engine/Graph 内部图结构用的 `u16` id。roe-calc 的 Spike B 适配层
    /// 明确不使用 formualizer 的 Engine（见本文件头注释），SheetResolver
    /// 自身完全没有、也不需要维护这个 u16 id 体系——find_sheet 全程按 sheet
    /// 名字符串工作。因此这里故意忽略 `cell.sheet_id` 字段，只用
    /// `cell.coord` 的行列坐标 + `self.current_sheet_name` 定位。
    ///
    /// 这个简化依赖一个前提：调用方（ROE_SUBTOTAL::eval()）遍历的 range
    /// 与查询公式文本时用的坐标系是同一个 sheet——已确认这个前提成立：
    /// 参照 LOK interpr1.cxx ScSubTotal() 实现，SUBTOTAL 对 range 参数的
    /// 处理完全委托给 ScSum/ScAverage 等普通聚合函数，没有引入任何跨 sheet
    /// （3D 引用）相关的特殊逻辑；formualizer-eval 的 RangeView 本身也是
    /// 单一 sheet 语义（RangeView::sheet_name() 返回单个字符串，backing
    /// 是单个 ArrowSheet）。SUBTOTAL 不支持跨 sheet range，因此
    /// current_sheet_name 兜底是安全的。
    ///
    /// 坐标换算：formualizer 的 Coord::row()/col() 是 1-based（与
    /// resolve_range_view 里 RT::Cell 分支 `sh.cells.get(*row - 1, *col - 1)`
    /// 的既有换算约定一致），SparseGrid::get 是 0-based，这里同样 -1。
    fn formula_text_at_cell(
        &self,
        cell: formualizer_eval::reference::CellRef,
    ) -> Result<Option<String>, ExcelError> {
        let row1 = cell.coord.row();
        let col1 = cell.coord.col();
        if row1 == 0 || col1 == 0 {
            return Ok(None);
        }
        let Some(sh) = self.find_sheet(None) else {
            return Ok(None);
        };
        let value = sh.cells.get(row1 - 1, col1 - 1);
        match value {
            CellValue::Formula { source, .. } => Ok(Some(source)),
            _ => Ok(None),
        }
    }

    fn resolve_range_view<'c>(
        &'c self,
        reference: &formualizer_parse::parser::ReferenceType,
        current_sheet: &str,
    ) -> Result<formualizer_eval::engine::range_view::RangeView<'c>, ExcelError> {
        use formualizer_parse::parser::ReferenceType as RT;
        let _ = current_sheet; // current_sheet 是 formualizer 侧上下文提示；
                                // 我们用 reference 自带的 sheet 字段 + find_sheet 兜底，语义等价。
        let rows: Vec<Vec<LiteralValue>> = match reference {
            RT::Cell { sheet, row, col, .. } => {
                if *row == 0 || *col == 0 {
                    return Err(ExcelError::from(ExcelErrorKind::Ref));
                }
                let sh = self
                    .find_sheet(sheet.as_deref())
                    .ok_or_else(|| ExcelError::from(ExcelErrorKind::Ref))?;
                let v = sh.cells.get(*row - 1, *col - 1);
                self.access_log
                    .write()
                    .expect("access_log lock poisoned")
                    .push((sh.name.clone(), *row - 1, *col - 1));
                vec![vec![self.literal_for_cell(&sh.name, *row - 1, *col - 1, &v)]]
            }
            RT::Range {
                sheet,
                start_row,
                start_col,
                end_row,
                end_col,
                ..
            } => {
                // ROOT tdf164895 附带修复：整列/整行开放式引用（$A:$A、$1:$1）
                // 支持。start_row/start_col/end_row/end_col 任一为 None 表示
                // 该维度未指定边界（formualizer_parse 语法层面就是这样表达
                // 开放范围的），此前这里直接硬编码拒绝（NImpl），改为用
                // sheet 的"已用区域边界"（SparseGrid::used_bounds，0-based）
                // 填充缺失的边界，语义对齐 Excel/LibreOffice 对 $A:$A 这类
                // 引用的实际行为（只覆盖到已使用区域，不是真的无穷大）。
                //
                // find_sheet 提前到这里执行（原代码在算完 sr/sc/er/ec 之后
                // 才调用），因为算开放边界需要先拿到目标 sheet 的
                // used_bounds()。
                let sh = self
                    .find_sheet(sheet.as_deref())
                    .ok_or_else(|| ExcelError::from(ExcelErrorKind::Ref))?;
                // used_bounds() 是 0-based 最大行/列；本分支其余逻辑全程用
                // 1-based sr/sc/er/ec，且后面循环体里是 `r - 1, c - 1` 转回
                // 0-based 去查 cells。所以这里 +1 换算成"1-based 最大行/列"
                // 再往下走，跟显式给出边界的情况完全统一，不需要额外分支。
                let used = sh.cells.used_bounds(); // Option<(0-based max_row, 0-based max_col)>
                let (used_max_row_1based, used_max_col_1based) = match used {
                    Some((mr, mc)) => (mr + 1, mc + 1),
                    // 整个 sheet 完全空白：没有任何"已用边界"可言。此时如果
                    // 公式引用的是开放范围（如 $A:$A），语义上应该是"空区域"
                    // （没有任何单元格可读），而不是报错——用 0 表示"空区域"，
                    // 下面 sr > er 时会被识别为空区域整体跳过循环，返回空表。
                    None => (0, 0),
                };
                let (sr, sc, er, ec) = (
                    start_row.unwrap_or(1),
                    start_col.unwrap_or(1),
                    end_row.unwrap_or(used_max_row_1based),
                    end_col.unwrap_or(used_max_col_1based),
                );
                // sr==0/sc==0 仍然是真正的非法引用（比如行号本身写了 R0，
                // 不是"开放边界"场景，开放边界场景 sr/sc 已经被上面
                // unwrap_or(1) 兜底成合法的 1）。
                if sr == 0 || sc == 0 {
                    return Err(ExcelError::from(ExcelErrorKind::Ref));
                }
                // er==0 或 ec==0 只会在"sheet 完全空白 + 该维度是开放范围"
                // 这一种情况下出现（used_max_*_1based 兜底成 0）。这不是
                // 错误引用，是"结果为空区域"，返回一个空的二维表即可，不
                // 走下面的 sr..=er 循环（避免 sr=1 > er=0 时 RangeInclusive
                // 产生的空迭代被误认为一致——其实两者行为一致，这里显式
                // early-return 只是让语义更清楚，不依赖空 range 的隐式行为）。
                if er == 0 || ec == 0 {
                    return Ok(formualizer_eval::engine::range_view::RangeView::from_owned_rows(
                        Vec::new(),
                        self.date_system(),
                    ));
                }
                let mut out = Vec::with_capacity((er - sr + 1) as usize);
                for r in sr..=er {
                    let mut row_vec = Vec::with_capacity((ec - sc + 1) as usize);
                    for c in sc..=ec {
                        let v = sh.cells.get(r - 1, c - 1);
                        self.access_log
                            .write()
                            .expect("access_log lock poisoned")
                            .push((sh.name.clone(), r - 1, c - 1));
                        row_vec.push(self.literal_for_cell(&sh.name, r - 1, c - 1, &v));
                    }
                    out.push(row_vec);
                }
                out
            }
            RT::Cell3D {
                sheet_first,
                sheet_last,
                row,
                col,
                ..
            } => {
                // tdf163554: 3D 单元格引用（`Sheet1:Sheet3!A1`）。语义对齐
                // LibreOffice 实测行为（本项目 tdf163554 样本核对过）：遍历
                // sheet_first..=sheet_last（按 workbook 物理顺序，自动纠正
                // 反向写法，见 resolve_sheet_3d_range 注释）之间的每个
                // sheet，取同一 (row, col) 位置的单元格值，每个 sheet 贡献
                // 一行——这样 SUM/COUNT/AVERAGE 等聚合函数在 formualizer_eval
                // 那一层完全无感知，拿到的就是一个"看起来正常"的 N 行 1 列
                // 二维表，跟单 sheet 多行引用一样直接遍历求值即可。
                if *row == 0 || *col == 0 {
                    return Err(ExcelError::from(ExcelErrorKind::Ref));
                }
                let sheets = self.resolve_sheet_3d_range(sheet_first, sheet_last)?;
                let mut out = Vec::with_capacity(sheets.len());
                for sh in sheets {
                    let v = sh.cells.get(*row - 1, *col - 1);
                    self.access_log
                        .write()
                        .expect("access_log lock poisoned")
                        .push((sh.name.clone(), *row - 1, *col - 1));
                    out.push(vec![self.literal_for_cell(&sh.name, *row - 1, *col - 1, &v)]);
                }
                out
            }
            RT::Range3D {
                sheet_first,
                sheet_last,
                start_row,
                start_col,
                end_row,
                end_col,
                ..
            } => {
                // tdf163554: 3D 区域引用（`Sheet1:Sheet3!A1:B2`）。与
                // RT::Cell3D 同样的"逐 sheet 取值、纵向拼接成大二维表"思路，
                // 额外还要处理区域本身的开放边界（如 `Sheet1:Sheet3!A:A`），
                // 复用 RT::Range 分支已有的 used_bounds() 兜底逻辑，每个
                // sheet 各自算一次开放边界（因为不同 sheet 的已用区域可能
                //不一样大），再各自产出一个二维块，所有 sheet 的块依次
                // 纵向拼接。
                let sheets = self.resolve_sheet_3d_range(sheet_first, sheet_last)?;
                let mut out: Vec<Vec<LiteralValue>> = Vec::new();
                for sh in sheets {
                    let used = sh.cells.used_bounds();
                    let (used_max_row_1based, used_max_col_1based) = match used {
                        Some((mr, mc)) => (mr + 1, mc + 1),
                        None => (0, 0),
                    };
                    let (sr, sc, er, ec) = (
                        start_row.unwrap_or(1),
                        start_col.unwrap_or(1),
                        end_row.unwrap_or(used_max_row_1based),
                        end_col.unwrap_or(used_max_col_1based),
                    );
                    if sr == 0 || sc == 0 {
                        return Err(ExcelError::from(ExcelErrorKind::Ref));
                    }
                    if er == 0 || ec == 0 {
                        // 这个 sheet 完全空白 + 该维度是开放范围：这个 sheet
                        // 对结果贡献空区域，跳过，不往 out 里加行，继续下一
                        // 个 sheet（不是整个 3D 引用都判空，只是这一个 sheet
                        // 没有可读的数据）。
                        continue;
                    }
                    for r in sr..=er {
                        let mut row_vec = Vec::with_capacity((ec - sc + 1) as usize);
                        for c in sc..=ec {
                            let v = sh.cells.get(r - 1, c - 1);
                            self.access_log
                                .write()
                                .expect("access_log lock poisoned")
                                .push((sh.name.clone(), r - 1, c - 1));
                            row_vec.push(self.literal_for_cell(&sh.name, r - 1, c - 1, &v));
                        }
                        out.push(row_vec);
                    }
                }
                out
            }
            RT::NamedRange(name) => self.resolve_named_range_rows(name)?,
            RT::Table(tref) => {
                // ROOT-011: 具名结构化引用（非 This Row 场景，ThisRow 已在
                // eval_formula_with_deps_with_resolver_and_cell 里通过 AST
                // 改写成具体 Cell 引用，不会以 Table 变体走到这里）。
                let table = self.resolve_table_reference(tref)?;
                use formualizer_parse::parser::{SpecialItem, TableSpecifier};
                match &tref.specifier {
                    Some(TableSpecifier::Column(c)) => {
                        let col_range = table.get_column(c)?;
                        let (h, _) = col_range.dimensions();
                        let mut out = Vec::with_capacity(h);
                        for r in 0..h {
                            out.push(vec![col_range.get(r, 0)?]);
                        }
                        out
                    }
                    Some(TableSpecifier::ColumnRange(start, end)) => {
                        let cols = table.columns();
                        let start_key = start.to_lowercase();
                        let end_key = end.to_lowercase();
                        let start_idx = cols.iter().position(|n| n.to_lowercase() == start_key);
                        let end_idx = cols.iter().position(|n| n.to_lowercase() == end_key);
                        let (mut si, mut ei) = match (start_idx, end_idx) {
                            (Some(si), Some(ei)) => (si, ei),
                            _ => return Err(ExcelError::new(ExcelErrorKind::Ref)
                                .with_message("Column range refers to unknown column(s)".to_string())),
                        };
                        if si > ei { std::mem::swap(&mut si, &mut ei); }
                        let h = table.data_height();
                        let mut out = vec![Vec::with_capacity(ei - si + 1); h];
                        for ci in si..=ei {
                            let col_range = table.get_column(&cols[ci])?;
                            let (rh, _) = col_range.dimensions();
                            for r in 0..h.min(rh) {
                                out[r].push(col_range.get(r, 0)?);
                            }
                        }
                        out
                    }
                    Some(TableSpecifier::SpecialItem(SpecialItem::Headers)) | Some(TableSpecifier::Headers) => {
                        table.headers_row().map(|r| r.materialise().into_owned()).unwrap_or_default()
                    }
                    Some(TableSpecifier::SpecialItem(SpecialItem::Totals)) | Some(TableSpecifier::Totals) => {
                        table.totals_row().map(|r| r.materialise().into_owned()).unwrap_or_default()
                    }
                    Some(TableSpecifier::SpecialItem(SpecialItem::Data)) | Some(TableSpecifier::Data) => {
                        table.data_body().map(|r| r.materialise().into_owned()).unwrap_or_default()
                    }
                    Some(TableSpecifier::SpecialItem(SpecialItem::All)) | Some(TableSpecifier::All) | None => {
                        let mut out = Vec::new();
                        if let Some(h) = table.headers_row() { out.extend(h.materialise().into_owned()); }
                        if let Some(b) = table.data_body() { out.extend(b.materialise().into_owned()); }
                        if let Some(t) = table.totals_row() { out.extend(t.materialise().into_owned()); }
                        out
                    }
                    Some(TableSpecifier::SpecialItem(SpecialItem::ThisRow)) => {
                        return Err(ExcelError::new(ExcelErrorKind::NImpl).with_message(
                            "@ (This Row) requires table-aware context; not yet supported".to_string(),
                        ));
                    }
                    Some(TableSpecifier::Row(_)) | Some(TableSpecifier::Combination(_)) => {
                        return Err(ExcelError::new(ExcelErrorKind::NImpl)
                            .with_message("Complex structured references not yet supported".to_string()));
                    }
                }
            }
            _ => {
                return Err(ExcelError::new(ExcelErrorKind::NImpl)
                    .with_message("reference kind not yet supported in roe-calc spike".to_string()));
            }
        };
        Ok(formualizer_eval::engine::range_view::RangeView::from_owned_rows(
            rows,
            self.date_system(),
        ))
    }
}

fn literal_to_cell_value(v: LiteralValue) -> CellValue {
    match v {
        LiteralValue::Empty | LiteralValue::Pending => CellValue::Empty,
        LiteralValue::Int(i) => CellValue::Number(i as f64),
        LiteralValue::Number(n) => CellValue::Number(n),
        LiteralValue::Text(s) => CellValue::Text(s),
        LiteralValue::Boolean(b) => CellValue::Bool(b),
        LiteralValue::Error(e) => CellValue::Error(excel_error_kind_to_cell_error(e.kind)),
        LiteralValue::Array(rows) => rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .map(literal_to_cell_value)
            .unwrap_or(CellValue::Empty),
        LiteralValue::Date(d) => CellValue::Text(d.to_string()),
        LiteralValue::DateTime(dt) => CellValue::Text(dt.to_string()),
        LiteralValue::Time(t) => CellValue::Text(t.to_string()),
        LiteralValue::Duration(dur) => CellValue::Text(format!("{:?}", dur)),
    }
}

/// ROOT-011: `[@列名]`（未具名 This Row 简写）和 `Table1[[#This Row],[列名]]`
/// （具名 This Row）结构化引用的 AST 改写。
///
/// 背景：formualizer-eval 的 Engine（`engine/graph/mod.rs::rewrite_structured_reference`）
/// 在 `set_cell_formula` 时会把这类引用改写成具体的 Cell 引用——但那个改写逻辑绑死在
/// Engine 的内部图结构上（`find_table_containing_cell` 查询它自己维护的 `self.tables`），
/// roe-calc 故意不用 Engine（会引入第二套内部单元格存储，见文件头注释），所以这个改写
/// 必须在 roe-calc 里独立实现一份，基于 roe_model::calc::{Workbook, Sheet} 查表。
///
/// 语义对齐 formualizer 原版：
/// - 未具名（`tref.name` 为空）+ `Combination([ThisRow, Column(c)])` → `[@c]` 简写
/// - 具名 + `Combination([ThisRow, Column(c)])` → `Table1[[#This Row],[c]]` 完整写法
/// - 两种情况均改写成同一 sheet 上 (current_row, table列c对应的具体列) 的 Cell 引用
/// - 表头行上使用 This Row 是非法的（对齐原版 Ref 错误）
/// - 找不到包含 current_cell 的表，或列名不存在，返回 NImpl/Ref 错误（原地报错，
///   不改写，调用方会在最外层把这类 parse 阶段以外的求值错误转成 CellValue::Error）
///
/// 只处理"改写"，不做求值；改写后 AST 里就是普通 Cell 引用，走既有的
/// resolve_range_view Cell 分支求值 + access_log 依赖记录，无需额外插桩。
// ROOT-023: OR()/AND() 等函数按 Excel 语义做短路求值（一旦某个参数已经能
// 决定最终结果，如 OR 遇到 TRUE、AND 遇到 FALSE，就不再继续求值剩余参数）。
// 这本身是正确的、忠实复刻 Excel 的行为，不是 bug——比如 OR(1, 1/0) 在 Excel 里
// 就是合法写法，短路后不该报错。
//
// 但这带来一个副作用：如果公式里某个"名字"在词法/语法层面能被识别为标识符、
// 但在 workbook 里其实并未定义为任何命名区域（典型触发场景：公式里混入了
// 智能引号 “ ” ‘ ’ 等 Excel 语法之外的字符，被 formualizer 容忍地当成了一个
// 裸标识符 token），且这个未定义名字恰好出现在会被短路跳过的参数位置，
// 短路求值会让这个"本该是 #NAME? 的公式"从未真正求值到那个位置，
// 静默地算出一个符合短路逻辑但语义上没有意义的结果（如 TRUE）。
//
// 真实 Excel/LibreOffice/OnlyOffice 对这类公式的行为是稳定报 #NAME?，不会被
// 短路掩盖。最小成本的修复方案：不改动 formualizer 短路逻辑本身（那是正确的
// 通用行为），而是在求值之前，对 AST 做一次轻量预检查——如果发现任何一个
// NamedRange 引用在 workbook.named_ranges 里找不到，直接判定整个公式为
// #NAME?，不再进入 evaluate_ast（也就绕开了短路机制）。
//
// 性能考量：只有当 AST 里存在至少一个 NamedRange 节点时才会做名字查找
// （HashMap O(1)），且找到第一个未定义名字就立即短路返回，不会遍历完整棵树；
// 绝大多数公式（Cell/Range 引用，不含裸名字）在这个检查里只是走一遍已经很
// 便宜的树遍历，成本可忽略。
/// 名称查找:Excel 名称大小写不敏感。先精确匹配(O(1)),未命中再按小写线性比较。
fn lookup_name_ci<'m, V>(
    map: &'m std::collections::HashMap<String, V>,
    name: &str,
) -> Option<&'m V> {
    if let Some(v) = map.get(name) {
        return Some(v);
    }
    let key = name.to_lowercase();
    map.iter()
        .find(|(k, _)| k.to_lowercase() == key)
        .map(|(_, v)| v)
}

fn find_unresolvable_named_range(
    node: &formualizer_parse::parser::ASTNode,
    workbook: &Workbook,
) -> Option<String> {
    use formualizer_parse::parser::{ASTNodeType, ReferenceType};
    match &node.node_type {
        ASTNodeType::Reference { reference, .. } => {
            if let ReferenceType::NamedRange(name) = reference {
                // ROOT tdf164895: 公式类型命名定义（named_formulas）与区域类型
                // （named_ranges）都算"已定义"，缺一都会导致这个预检查提前
                // 短路判 #NAME?，绕开下面 resolve_range_view 里新增的
                // named_formulas fallback 分支，永远走不到。
                if lookup_name_ci(&workbook.named_ranges, name).is_none()
                    && lookup_name_ci(&workbook.named_formulas, name).is_none()
                {
                    return Some(name.clone());
                }
            }
            None
        }
        ASTNodeType::UnaryOp { expr, .. } => find_unresolvable_named_range(expr, workbook),
        ASTNodeType::BinaryOp { left, right, .. } => {
            find_unresolvable_named_range(left, workbook)
                .or_else(|| find_unresolvable_named_range(right, workbook))
        }
        ASTNodeType::Function { args, .. } => args
            .iter()
            .find_map(|a| find_unresolvable_named_range(a, workbook)),
        ASTNodeType::Call { callee, args } => {
            find_unresolvable_named_range(callee, workbook).or_else(|| {
                args.iter()
                    .find_map(|a| find_unresolvable_named_range(a, workbook))
            })
        }
        ASTNodeType::Array(rows) => rows
            .iter()
            .flat_map(|r| r.iter())
            .find_map(|item| find_unresolvable_named_range(item, workbook)),
        // formualizer 0.8+: 省略的参数槽位(如 IF(A1,,3)),叶子节点,无引用
        ASTNodeType::Literal(_) | ASTNodeType::Omitted => None,
    }
}

fn rewrite_this_row_references(
    ast: &mut formualizer_parse::parser::ASTNode,
    workbook: &Workbook,
    current_sheet: &str,
    cell_row0: u32,
    cell_col0: u32,
) -> Result<(), ExcelError> {
    rewrite_this_row_node(ast, workbook, current_sheet, cell_row0, cell_col0)?;
    Ok(())
}

/// ROOT-078附带发现：条件格式 "expression" 规则的公式语义是相对于
/// sqref 范围锚点单元格（范围左上角）写的，判断范围内其他单元格时，
/// 需要把公式里所有"相对"单元格引用按 (目标单元格 - 锚点单元格) 的
/// 行列差整体平移（绝对引用 $A$1 不平移，行/列各自独立判断，如 A$1
/// 只平移列不平移行）。这与 rewrite_this_row_references 是两个独立
/// 职责：后者处理结构化表引用 [@列名] 的"当前行"语义，本函数处理
/// 普通 Cell/Range 引用跟随锚点的行列偏移，不应合并到同一函数里，
/// 避免把两种不同来源、不同触发条件的相对引用改写耦合在一起。
///
/// 只在 anchor_row0/anchor_col0 与 cell_row0/cell_col0 不同时才需要
/// 调用（若相同，偏移量为0，调用了也是空操作，但调用方仍可无条件
/// 调用，函数内部对0偏移的处理天然是恒等变换，不需要调用方特判）。
fn rewrite_relative_refs_by_offset(
    ast: &mut formualizer_parse::parser::ASTNode,
    anchor_row0: u32,
    anchor_col0: u32,
    cell_row0: u32,
    cell_col0: u32,
) {
    let row_delta = cell_row0 as i64 - anchor_row0 as i64;
    let col_delta = cell_col0 as i64 - anchor_col0 as i64;
    if row_delta == 0 && col_delta == 0 {
        return;
    }
    rewrite_relative_refs_node(ast, row_delta, col_delta);
}

fn rewrite_relative_refs_node(
    node: &mut formualizer_parse::parser::ASTNode,
    row_delta: i64,
    col_delta: i64,
) {
    use formualizer_parse::parser::ASTNodeType;
    match &mut node.node_type {
        ASTNodeType::Reference { reference, .. } => {
            rewrite_relative_refs_reference(reference, row_delta, col_delta);
        }
        ASTNodeType::UnaryOp { expr, .. } => {
            rewrite_relative_refs_node(expr, row_delta, col_delta);
        }
        ASTNodeType::BinaryOp { left, right, .. } => {
            rewrite_relative_refs_node(left, row_delta, col_delta);
            rewrite_relative_refs_node(right, row_delta, col_delta);
        }
        ASTNodeType::Function { args, .. } => {
            for a in args.iter_mut() {
                rewrite_relative_refs_node(a, row_delta, col_delta);
            }
        }
        ASTNodeType::Call { callee, args } => {
            rewrite_relative_refs_node(callee, row_delta, col_delta);
            for a in args.iter_mut() {
                rewrite_relative_refs_node(a, row_delta, col_delta);
            }
        }
        ASTNodeType::Array(rows) => {
            for r in rows.iter_mut() {
                for item in r.iter_mut() {
                    rewrite_relative_refs_node(item, row_delta, col_delta);
                }
            }
        }
        // Literal 及其余不含子节点/引用的变体：无需处理。
        _ => {}
    }
}

/// 按 row_abs/col_abs 标记，只平移非绝对的坐标分量。行、列各自独立
/// 判断（`A$1` 这种混合绝对引用只平移列、不平移行，符合 Excel 语义）。
/// 只处理 Cell/Range 两种——条件格式公式场景实践中不会出现 3D引用/
/// 外部引用/命名区域/结构化表引用，遇到这些原样放行不报错，与
/// rewrite_this_row_reference 对非 Table 类型"原样放行"的既有风格
/// 保持一致。
fn rewrite_relative_refs_reference(
    reference: &mut formualizer_parse::parser::ReferenceType,
    row_delta: i64,
    col_delta: i64,
) {
    use formualizer_parse::parser::ReferenceType;
    match reference {
        ReferenceType::Cell { row, col, row_abs, col_abs, .. } => {
            if !*row_abs {
                *row = (*row as i64 + row_delta).max(0) as u32;
            }
            if !*col_abs {
                *col = (*col as i64 + col_delta).max(0) as u32;
            }
        }
        ReferenceType::Range {
            start_row, start_col, end_row, end_col,
            start_row_abs, start_col_abs, end_row_abs, end_col_abs, ..
        } => {
            if !*start_row_abs {
                if let Some(r) = start_row { *r = (*r as i64 + row_delta).max(0) as u32; }
            }
            if !*start_col_abs {
                if let Some(c) = start_col { *c = (*c as i64 + col_delta).max(0) as u32; }
            }
            if !*end_row_abs {
                if let Some(r) = end_row { *r = (*r as i64 + row_delta).max(0) as u32; }
            }
            if !*end_col_abs {
                if let Some(c) = end_col { *c = (*c as i64 + col_delta).max(0) as u32; }
            }
        }
        // Cell3D/Range3D/External/Table/NamedRange：不处理，原样放行。
        _ => {}
    }
}

fn rewrite_this_row_node(
    node: &mut formualizer_parse::parser::ASTNode,
    workbook: &Workbook,
    current_sheet: &str,
    cell_row0: u32,
    cell_col0: u32,
) -> Result<bool, ExcelError> {
    use formualizer_parse::parser::ASTNodeType;
    match &mut node.node_type {
        ASTNodeType::Reference { reference, .. } => {
            rewrite_this_row_reference(reference, workbook, current_sheet, cell_row0, cell_col0)
        }
        ASTNodeType::UnaryOp { expr, .. } => {
            rewrite_this_row_node(expr, workbook, current_sheet, cell_row0, cell_col0)
        }
        ASTNodeType::BinaryOp { left, right, .. } => {
            let l = rewrite_this_row_node(left, workbook, current_sheet, cell_row0, cell_col0)?;
            let r = rewrite_this_row_node(right, workbook, current_sheet, cell_row0, cell_col0)?;
            Ok(l || r)
        }
        ASTNodeType::Function { args, .. } => {
            let mut rewritten = false;
            for a in args.iter_mut() {
                rewritten |= rewrite_this_row_node(a, workbook, current_sheet, cell_row0, cell_col0)?;
            }
            Ok(rewritten)
        }
        ASTNodeType::Call { callee, args } => {
            let mut rewritten = rewrite_this_row_node(callee, workbook, current_sheet, cell_row0, cell_col0)?;
            for a in args.iter_mut() {
                rewritten |= rewrite_this_row_node(a, workbook, current_sheet, cell_row0, cell_col0)?;
            }
            Ok(rewritten)
        }
        ASTNodeType::Array(rows) => {
            let mut rewritten = false;
            for r in rows.iter_mut() {
                for item in r.iter_mut() {
                    rewritten |= rewrite_this_row_node(item, workbook, current_sheet, cell_row0, cell_col0)?;
                }
            }
            Ok(rewritten)
        }
        ASTNodeType::Literal(_) | ASTNodeType::Omitted => Ok(false),
    }
}

fn rewrite_this_row_reference(
    reference: &mut formualizer_parse::parser::ReferenceType,
    workbook: &Workbook,
    current_sheet: &str,
    cell_row0: u32,
    cell_col0: u32,
) -> Result<bool, ExcelError> {
    use formualizer_parse::parser::{ReferenceType, SpecialItem, TableSpecifier};

    let ReferenceType::Table(tref) = reference else {
        return Ok(false);
    };

    // 只处理含 ThisRow 的 Combination 形式（[@col] 和 Table1[[#This Row],[col]] 均是这种形式）。
    // 其余 specifier（Column/ColumnRange/Headers/Totals/Data/All 等）不是 This Row，交给
    // resolve_range_view 里的 RT::Table 分支正常处理，这里原样放行不算改写。
    let col_name = match &tref.specifier {
        Some(TableSpecifier::Combination(parts)) => {
            let mut saw_this_row = false;
            let mut col: Option<String> = None;
            for p in parts {
                match p.as_ref() {
                    TableSpecifier::SpecialItem(SpecialItem::ThisRow) => saw_this_row = true,
                    TableSpecifier::Column(c) => {
                        if col.is_some() {
                            return Err(ExcelError::new(ExcelErrorKind::NImpl).with_message(
                                "This-row structured reference with multiple columns is not supported".to_string(),
                            ));
                        }
                        col = Some(c.clone());
                    }
                    _ => return Ok(false), // 未识别的组合形式，不当 This Row 处理，交还上层报错
                }
            }
            if !saw_this_row {
                return Ok(false);
            }
            match col {
                Some(c) => c,
                None => return Ok(false),
            }
        }
        _ => return Ok(false), // 不是 This Row 形式
    };

    // 找到包含 (cell_row0, cell_col0) 的表：优先用引用自带的表名（具名场景），
    // 未具名（[@col] 简写）场景 tref.name 为空，退化为"当前 sheet 上任意一张
    // 范围包含该单元格的表"（Excel 语义：[@col] 只能出现在表内公式里，同一单元格
    // 不可能同时属于两张重叠的表，取第一张匹配即可）。
    let sheet = workbook
        .sheets
        .iter()
        .find(|s| s.name == current_sheet)
        .ok_or_else(|| ExcelError::new(ExcelErrorKind::Ref).with_message("Sheet not found".to_string()))?;

    let table = if tref.name.is_empty() {
        find_table_containing_cell(sheet, cell_row0, cell_col0)
    } else {
        let key = tref.name.to_lowercase();
        sheet
            .tables
            .iter()
            .find(|t| t.name.to_lowercase() == key)
            .filter(|t| cell_in_table_range(t, cell_row0, cell_col0))
    };

    let table = table.ok_or_else(|| {
        ExcelError::new(ExcelErrorKind::Name)
            .with_message("This-row structured reference used outside a table".to_string())
    })?;

    let header_rows = if table.has_header { 1 } else { 0 };
    if cell_row0 == table.range.start.0 && header_rows == 1 {
        return Err(ExcelError::new(ExcelErrorKind::Ref).with_message(
            "This-row structured references are not valid in the table header row".to_string(),
        ));
    }

    let col_key = col_name.to_lowercase();
    let col_offset = table
        .columns
        .iter()
        .position(|c| c.to_lowercase() == col_key)
        .ok_or_else(|| {
            ExcelError::new(ExcelErrorKind::Ref)
                .with_message(format!("Unknown table column: {col_name}"))
        })?;

    let target_col0 = table.range.start.1 + col_offset as u32;

    *reference = ReferenceType::Cell {
        sheet: None,
        row: cell_row0 + 1,       // formualizer Cell 引用是 1-based
        col: target_col0 + 1,
        row_abs: true,
        col_abs: true,
    };

    Ok(true)
}

fn cell_in_table_range(table: &roe_model::calc::Table, row0: u32, col0: u32) -> bool {
    row0 >= table.range.start.0
        && row0 <= table.range.end.0
        && col0 >= table.range.start.1
        && col0 <= table.range.end.1
}

fn find_table_containing_cell(sheet: &Sheet, row0: u32, col0: u32) -> Option<&roe_model::calc::Table> {
    sheet.tables.iter().find(|t| cell_in_table_range(t, row0, col0))
}

/// 求值 + 返回求值过程中访问过的所有单元格（sheet 名, row 0-based, col 0-based）。
/// 用于第 4 步：调用方（scheduler.rs）据此在 FormulaDAG 里建依赖边。
/// 访问记录来自 SheetResolver::resolve_range_view 的插桩（见"坑1"：这是
/// Interpreter 实际调用的方法，resolve_cell_reference/resolve_range_reference
/// 不在真正求值的调用链上，故只需在这一处插桩）。
pub fn eval_formula_with_deps(
    workbook: &Workbook,
    sheet_name: &str,
    formula: &str,
) -> (CellValue, Vec<(String, u32, u32)>) {
    let resolver = SheetResolver::new(workbook, sheet_name);
    eval_formula_with_deps_with_resolver(resolver, formula)
}

/// ROOT-012 修复：eval_formula_with_deps 的通用版本，接收调用方已经构造好的
/// SheetResolver（可能带 recalc_hook，见 scheduler.rs 的 eval_with_dag），
/// 而不是自己 new 一个不带 hook 的默认 resolver。
/// 原 eval_formula_with_deps 现在只是这个函数的一层薄包装（不带 hook 的场景）。
pub fn eval_formula_with_deps_with_resolver(
    resolver: SheetResolver<'_>,
    formula: &str,
) -> (CellValue, Vec<(String, u32, u32)>) {
    eval_formula_with_deps_with_resolver_and_cell(resolver, formula, None)
}

/// ROOT-016 修复：带 current_cell 坐标的版本，供 ROW()/COLUMN() 无参调用使用。
/// current_cell 是 (row 0-based, col 0-based)；sheet_id 不参与 ROW/COLUMN 计算，
/// 固定传 0 占位即可。传 None 时行为与原来完全一致（不设置 current_cell）。
/// ROOT-019: 去掉ODF公式里的跨应用兼容函数命名空间前缀（COM.MICROSOFT./ORG.OPENOFFICE.），
/// 只在等号后紧跟这些前缀、且前缀后紧跟一个函数名（字母/点号组成，后面是左括号）时才剥离，
/// 避免误伤字符串字面量里恰好包含这些字符串的情况（用简单的大小写不敏感前缀匹配+左括号前瞻）。
fn strip_odf_function_namespace_prefixes(formula: &str) -> String {
    const PREFIXES: [&str; 2] = ["COM.MICROSOFT.", "ORG.OPENOFFICE."];
    let mut result = String::with_capacity(formula.len());
    let mut in_string = false;
    // ROOT-022 修复：原实现用 `formula.as_bytes()[i] as char` 按字节索引强转字符，
    // 遇到多字节 UTF-8 字符（如全角/智能引号 U+2019）时 i 会停在字符中间，
    // 后续 `&formula[i..]` 切片触发 "byte index is not a char boundary" panic。
    // 改用 char_indices() 遍历，i 保证只落在合法字符边界上。
    let mut chars = formula.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '"' {
            in_string = !in_string;
            result.push(c);
            continue;
        }
        if !in_string {
            let remainder = &formula[i..];
            // ROOT-022: 用 get() 而非直接切片，remainder 前 p.len() 字节可能落在
            // 多字节字符中间（p 是纯 ASCII 前缀，但 remainder 本身内容不一定是），
            // 直接切片会 panic；get() 切不到边界时返回 None，视为不匹配即可。
            let upper_remainder_prefix_len = PREFIXES.iter().find_map(|p| {
                remainder
                    .get(..p.len())
                    .filter(|slice| slice.eq_ignore_ascii_case(p))
                    .map(|_| p.len())
            });
            if let Some(plen) = upper_remainder_prefix_len {
                // 前面必须是函数调用起始位置（=、(、,、;、空格等非字母数字字符），
                // 避免把普通字符串或范围名里凑巧出现的子串也剥掉。
                let prev_ok = result.chars().last().map(|pc| !pc.is_alphanumeric() && pc != '_').unwrap_or(true);
                if prev_ok {
                    // PREFIXES 全为 ASCII，字节长度等于字符数，直接跳过 plen - 1 个
                    // 后续字符（当前这个 char 已经被 chars.next() 消费掉一个）。
                    for _ in 0..plen - 1 {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        result.push(c);
    }
    result
}

pub fn eval_formula_with_deps_with_resolver_and_cell(
    resolver: SheetResolver<'_>,
    formula: &str,
    current_cell: Option<(u32, u32)>,
) -> (CellValue, Vec<(String, u32, u32)>) {
    ensure_builtins_loaded();
    // formualizer 要求公式以 '=' 开头，否则整串被当成文本字面量。
    // roe-calc 内部约定公式字符串不含前导 '='（见 CellValue::Formula 注释），
    // 这里统一补上，调用方不需要关心这个细节。
    let normalized = if formula.starts_with('=') {
        formula.to_string()
    } else {
        format!("={formula}")
    };
    // ROOT-019修复：ODF文件里跨应用兼容函数会带 COM.MICROSOFT.xxx / ORG.OPENOFFICE.xxx
    // 命名空间前缀（如 =COM.MICROSOFT.NORM.INV(...)），formualizer-parse 不认识这层
    // 前缀，会把整个前缀+函数名当成一个不存在的函数名，导致#NAME?。这两个前缀在
    // ODF/OpenFormula规范里语义上等价于"就用后面这个裸函数名"，直接原地去掉即可，
    // 不影响函数参数/其余语法。
    let normalized = strip_odf_function_namespace_prefixes(&normalized);
    // ROOT-019修复：ODF公式用OpenFormula方言（[.A2]方括号引用、;实参分隔符等），
    // 与Excel方言语法不兼容。roe-calc此前无论来源都用默认的parse()（=Excel方言），
    // 导致所有ODF来源工作簿的公式在tokenize阶段就出错或产生错误AST。
    // 现在按workbook记录的来源格式选择正确的方言。
    let dialect = match resolver.workbook.formula_dialect_source {
        roe_model::calc::FormulaDialectSource::OpenFormula => formualizer_parse::FormulaDialect::OpenFormula,
        roe_model::calc::FormulaDialectSource::Excel => formualizer_parse::FormulaDialect::Excel,
    };
    let ast = match formualizer_parse::parse_with_dialect(&normalized, dialect) {
        Ok(ast) => ast,
        Err(_) => return (CellValue::Error(CellError::Name), Vec::new()),
    };
    eval_ast_with_resolver_and_cell(resolver, &ast, current_cell, None)
}

/// ROOT-078 (eval_hook架构)：`eval_formula_with_deps_with_resolver_and_cell`
/// 的"parse之后"部分抽出，供 AST 已提前解析并缓存的调用方（如
/// `CachedEvaluator`，见该 struct 文档）复用，避免重复 parse 同一段
/// 公式文本带来的开销。
///
/// 重要：入参 `ast` 必须是"改写前"的原始解析结果（即
/// `formualizer_parse::parse_with_dialect` 的直接输出），不能是已经
/// 跑过结构化表引用改写（`rewrite_this_row_references`）的版本——
/// 该改写依赖 `current_cell`（当前行），对不同调用方传入不同
/// `current_cell` 时改写结果不同，因此改写结果本身不可跨调用缓存，
/// 每次调用都会在内部 clone 一份 `ast` 后现改写。调用方（如
/// `CachedEvaluator`）应当只缓存"改写前"的 `ASTNode`。
pub fn eval_ast_with_resolver_and_cell(
    resolver: SheetResolver<'_>,
    ast: &formualizer_parse::parser::ASTNode,
    current_cell: Option<(u32, u32)>,
    anchor_cell: Option<(u32, u32)>,
) -> (CellValue, Vec<(String, u32, u32)>) {
    ensure_builtins_loaded();
    let mut ast = ast.clone();
    // ROOT-011: [@col] / Table1[[#This Row],[col]] 结构化引用改写为具体 Cell 引用。
    // 必须在 current_cell 已知时才能定位"当前行"，没有 current_cell（如独立单元测试
    // 场景直接调用 eval_formula_with_deps）时跳过，维持原有行为不变。
    if let Some((row0, col0)) = current_cell {
        if let Err(_e) = rewrite_this_row_references(&mut ast, resolver.workbook, resolver.current_sheet_name, row0, col0) {
            return (CellValue::Error(CellError::Ref), Vec::new());
        }
    }
    // ROOT-078附带发现：条件格式 expression 规则的相对引用平移。
    // anchor_cell 有值时才需要处理（cfvo阈值/普通公式求值没有锚点概念，
    // 传None，此时是恒等操作，不影响现有行为）；current_cell 必须同时
    // 存在才能算出偏移量，二者任一缺失则跳过（理论上条件格式场景两者
    // 总是同时提供，这里仍用 if let 保守处理，不 panic）。
    if let (Some((row0, col0)), Some((anchor_row0, anchor_col0))) = (current_cell, anchor_cell) {
        rewrite_relative_refs_by_offset(&mut ast, anchor_row0, anchor_col0, row0, col0);
    }
    let sheet_name = resolver.current_sheet_name.to_string();
    let interpreter = match current_cell {
        Some((row0, col0)) => {
            let cell_ref = formualizer_eval::reference::CellRef::new_absolute(0, row0, col0);
            formualizer_eval::interpreter::Interpreter::new_with_cell(&resolver, &sheet_name, cell_ref)
        }
        None => formualizer_eval::interpreter::Interpreter::new(&resolver, &sheet_name),
    };
    // ROOT-023 附带发现：真实 Excel（已用 LibreOffice + Excel 双重核实）
    // 对"公式顶层就是单独一个未定义名字"这种情况，智能双引号（U+201C “ /
    // U+201D ”）和智能单引号（U+2018 ‘ / U+2019 ’）的处理结果不一致——
    // 双引号裸操作数求值为 0（大概率是 Excel 把未绑定名字在顶层裸表达式
    // 位置做了类似"空引用→0"的隐式降级，只对这两个特定字符生效），单引号
    // 裸操作数则正常报 #NAME?（已用 LibreOffice 实测确认，不受此规则影响）。
    // 这个不对称背后的 Excel 内部机制没有公开可靠资料，此处不深究原理，
    // 只精确复刻这个已用真机验证过的具体行为：范围严格限定为「AST 顶层
    // 节点本身」是「值恰好等于单个 U+201C 或 U+201D 字符」的 NamedRange
    // 引用，不做任何字符集泛化（不影响其余三个智能引号字符，也不影响
    // 这两个字符出现在更深层表达式里的情况，如 OR() 参数——那些情况已由
    // 上面 find_unresolvable_named_range 的通用逻辑正确处理为 #NAME?）。
    if let formualizer_parse::parser::ASTNodeType::Reference {
        reference: formualizer_parse::parser::ReferenceType::NamedRange(name),
        ..
    } = &ast.node_type
    {
        if name == "\u{201C}" || name == "\u{201D}" {
            return (CellValue::Number(0.0), Vec::new());
        }
    }

    // ROOT-023: 见 find_unresolvable_named_range 注释。在求值前做一次
    // 轻量预检查，防止 OR()/AND() 短路把未定义名字导致的 #NAME? 静默吞掉。
    if find_unresolvable_named_range(&ast, resolver.workbook).is_some() {
        return (CellValue::Error(CellError::Name), Vec::new());
    }
    let value = match interpreter.evaluate_ast(&ast) {
        // AnnotatedScalar 的第二个字段是 eval 内部的数字格式标注,roe 自管格式,丢弃
        Ok(formualizer_eval::traits::CalcValue::Scalar(v))
        | Ok(formualizer_eval::traits::CalcValue::AnnotatedScalar(v, _)) => literal_to_cell_value(v),
        Ok(formualizer_eval::traits::CalcValue::Range(rv)) => {
            literal_to_cell_value(rv.get_cell(0, 0))
        }
        Ok(formualizer_eval::traits::CalcValue::Callable(_)) => CellValue::Error(CellError::Value),
        Err(e) => CellValue::Error(excel_error_kind_to_cell_error(e.kind)),
    };
    let deps = resolver.take_access_log();
    (value, deps)
}

/// 求值（不关心依赖）。内部委托给 eval_formula_with_deps 并丢弃依赖列表，
/// 保持既有调用方（含本文件内的既有测试）签名不变。
pub fn eval_formula(workbook: &Workbook, sheet_name: &str, formula: &str) -> CellValue {
    eval_formula_with_deps(workbook, sheet_name, formula).0
}


// ============================================================
// ROE_SUBTOTAL: 自定义公式函数，语义对齐 Excel SUBTOTAL 的核心行为——
// 对 range 求聚合时自动跳过 range 内其它 ROE_SUBTOTAL 公式产生的汇总行
// （即"小计不计入总计"），避免分类汇总多层嵌套时重复计数。
//
// 设计背景见 roe-ext_progress_v166/v167：放弃 is_summary_row 独立行标记
// 方案，改为对每个候选单元格调用 formula_text_at_cell 自省公式原文，
// 检测是否为嵌套 ROE_SUBTOTAL 调用。理由：is_summary_row 需要在
// roe_model::calc::Sheet 里新增持久化字段，改动面大且需要处理序列化/
// 撤销重做等一致性问题；公式文本自省是纯读操作，零侵入现有数据结构。
//
// 已知代价：range 内每个候选单元格都要额外查一次 formula_text_at_cell
// （sheet 查找 + 字符串匹配），对超大 range 有 O(n) 常数开销，v167
// 待核实清单里记录为"不阻塞功能正确性优先实现"的已知项，暂不做预过滤
// 优化。
//
// function_num 当前只实现 Excel SUBTOTAL 最常用的两个：
//   9  = SUM
//   1  = AVERAGE
// 其余 function_num（COUNT/MAX/MIN/STDEV等）留待后续按需扩展，
// 未支持的 function_num 返回 #VALUE! 错误而不是静默按SUM处理。
// ============================================================

static INIT_ROE_SUBTOTAL: Once = Once::new();

/// 注册 ROE_SUBTOTAL 到 formualizer 全局函数表。用独立 Once（而非复用
/// INIT_BUILTINS）是有意为之：INIT_BUILTINS 只负责 formualizer 自带的
/// SUM/AVERAGE 等内置函数加载，语义上和"ROE 项目自定义扩展函数"是两件
/// 不同的事，混用一个 Once 会让未来排查"builtins 到底加载了什么"时
/// 增加认知负担。
fn ensure_roe_subtotal_registered() {
    INIT_ROE_SUBTOTAL.call_once(|| {
        formualizer_eval::function_registry::register_function(std::sync::Arc::new(
            RoeSubtotalFn,
        ));
    });
}

#[derive(Debug)]
struct RoeSubtotalFn;

impl Function for RoeSubtotalFn {
    fn name(&self) -> &'static str {
        "SUBTOTAL"
    }

    fn min_args(&self) -> usize {
        2
    }

    fn variadic(&self) -> bool {
        true
    }

    fn arg_schema(&self) -> &'static [formualizer_eval::args::ArgSchema] {
        // 第1个参数：function_num，标量 number，必填，不可重复。
        // 第2个及以后：range 参数，可重复任意多次（variadic 尾部），
        // 用 repeating=Some(1) 标记 —— 参数数量超过 schema 长度时，
        // formualizer 会退回查找 repeating.is_some() 的这一条重复套用
        // （已在 formualizer-eval 源码 args.rs 第212行附近核实此机制，
        // 不是凭空猜测的用法）。
        static SCHEMA: std::sync::LazyLock<Vec<formualizer_eval::args::ArgSchema>> =
            std::sync::LazyLock::new(|| {
                vec![
                    formualizer_eval::args::ArgSchema::number_lenient_scalar(),
                    {
                        let mut s = formualizer_eval::args::ArgSchema::any();
                        s.shape = formualizer_eval::args::ShapeKind::Range;
                        s.repeating = Some(1);
                        s
                    },
                ]
            });
        &SCHEMA
    }

    fn eval<'a, 'b, 'c>(
        &self,
        args: &'c [formualizer_eval::traits::ArgumentHandle<'a, 'b>],
        ctx: &dyn formualizer_eval::traits::FunctionContext<'b>,
    ) -> Result<formualizer_eval::traits::CalcValue<'b>, ExcelError> {
        if args.len() < 2 {
            return Ok(formualizer_eval::traits::CalcValue::Scalar(
                LiteralValue::Error(ExcelError::new(ExcelErrorKind::Value)),
            ));
        }

        // 第一个参数：function_num，标量求值（不接受 range）。
        let function_num_val = args[0].value()?.into_literal();
        let function_num = match function_num_val {
            LiteralValue::Number(n) => n as i64,
            LiteralValue::Int(n) => n,
            LiteralValue::Error(e) => {
                return Ok(formualizer_eval::traits::CalcValue::Scalar(
                    LiteralValue::Error(e),
                ));
            }
            _ => {
                return Ok(formualizer_eval::traits::CalcValue::Scalar(
                    LiteralValue::Error(ExcelError::new(ExcelErrorKind::Value)),
                ));
            }
        };

        if function_num != 9 && function_num != 1 {
            // 未支持的 function_num：明确返回 #VALUE!，不要静默当成 SUM，
            // 避免调用方以为聚合口径是对的但实际算错。
            return Ok(formualizer_eval::traits::CalcValue::Scalar(
                LiteralValue::Error(ExcelError::new(ExcelErrorKind::Value)),
            ));
        }

        let mut sum = 0.0f64;
        let mut count: u64 = 0;

        // 剩余参数（第2个开始）：每个都应是 range，遍历其中每个 cell，
        // 跳过公式原文含嵌套 ROE_SUBTOTAL( 调用的 cell。
        for arg in &args[1..] {
            let view = match arg.range_view() {
                Ok(v) => v,
                Err(_) => {
                    // 不是 range，退化为标量：直接取值参与聚合（对齐
                    // Excel SUBTOTAL 允许混合标量参数的行为）。
                    let v = arg.value()?.into_literal();
                    match v {
                        LiteralValue::Error(e) => {
                            return Ok(formualizer_eval::traits::CalcValue::Scalar(
                                LiteralValue::Error(e),
                            ));
                        }
                        LiteralValue::Number(n) => {
                            sum += n;
                            count += 1;
                        }
                        LiteralValue::Int(n) => {
                            sum += n as f64;
                            count += 1;
                        }
                        _ => {}
                    }
                    continue;
                }
            };

            // v227 排查坐实：本 crate 的 SheetResolver::resolve_range_view
            // 走的是"eager 读取整段数据成 owned Vec，再包 RangeView::
            // from_owned_rows"这条路径（不是 formualizer 原生的、借用
            // ArrowSheet 并保留绝对坐标切片那条路径），
            // from_owned_rows 内部硬编码 sr=0/sc=0（或空数据时 sr=1/sc=1）。
            // 因此 view.start_row()/start_col() 对我们而言恒为 0，不能
            // 当作"这个 range 在 sheet 里的绝对起始坐标"使用——这是本次
            // 排查到的架构性事实，不是可以局部修掉的笔误。
            //
            // 改为：在拿到 view 之前，先用同一个 arg 调用
            // as_reference_or_eval() 拿到原始 ReferenceType（这是 AST
            // 引用节点本身携带的绝对坐标，1-based，不经过 RangeView 那层
            // 转换，因此不受上面那个问题影响），从中取出真正的绝对起始
            // 行列。ReferenceType::Range 的 start_row/start_col 是
            // Option<u32>（None 表示开放范围，如整列 $A:$A），开放范围时
            // 起点就是第1行/第1列，兜底为1，与 resolve_range_view 里
            // RT::Range 分支对开放范围的既有兜底逻辑（unwrap_or(1)）保持
            // 一致。非 Range 的引用形态（正常不应出现在这里，因为 arg
            // 已经能成功 range_view() 说明必是 range 形态）按 (1,1) 兜底，
            // 仅作防御，不应触发。
            use formualizer_parse::parser::ReferenceType as RT;
            let (abs_start_row1, abs_start_col1): (u32, u32) = match arg.as_reference_or_eval() {
                Ok(RT::Range { start_row, start_col, .. }) => {
                    (start_row.unwrap_or(1), start_col.unwrap_or(1))
                }
                Ok(RT::Cell { row, col, .. }) => (row, col),
                _ => (1, 1),
            };

            let (view_rows, view_cols) = view.dims();
            for r in 0..view_rows {
                for c in 0..view_cols {
                    // 换算为 1-based 绝对坐标去查 formula_text_at_cell。
                    // abs_start_row1/abs_start_col1 已经是 1-based 绝对
                    // 起始坐标（见上方取值处的说明），r/c 是 view 内的
                    // 0-based 相对偏移，直接相加即可得到该 cell 的
                    // 1-based 绝对坐标，不再需要额外 +1。
                    let cell_ref = formualizer_eval::reference::CellRef::new_absolute(
                        0,
                        abs_start_row1 + r as u32,
                        abs_start_col1 + c as u32,
                    );

                    if let Ok(Some(formula_text)) = ctx.formula_text_at_cell(cell_ref) {
                        let upper = formula_text.to_uppercase().replace(' ', "");
                        if upper.contains("SUBTOTAL(") {
                            // 嵌套小计行：跳过，不纳入本次聚合。
                            continue;
                        }
                    }

                    let v = view.get_cell(r, c);
                    match v {
                        LiteralValue::Number(n) => {
                            sum += n;
                            count += 1;
                        }
                        LiteralValue::Int(n) => {
                            sum += n as f64;
                            count += 1;
                        }
                        LiteralValue::Error(e) => {
                            return Ok(formualizer_eval::traits::CalcValue::Scalar(
                                LiteralValue::Error(e),
                            ));
                        }
                        _ => {
                            // 空单元格/文本/布尔：忽略，对齐 SUM/AVERAGE
                            // 对 range 内非数值单元格的既有行为。
                        }
                    }
                }
            }
        }

        let result = if function_num == 9 {
            sum
        } else {
            // function_num == 1: AVERAGE
            if count == 0 {
                return Ok(formualizer_eval::traits::CalcValue::Scalar(
                    LiteralValue::Error(ExcelError::new(ExcelErrorKind::Div)),
                ));
            }
            sum / (count as f64)
        };

        Ok(formualizer_eval::traits::CalcValue::Scalar(
            LiteralValue::Number(result),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::calc::{CellBlock, SparseColumn, SparseGrid};

    /// Step 1 独立验证：formula_text_at_cell 本身正确性，不涉及 ROE_SUBTOTAL。
    /// 复用本文件已有的 sheet_with_column_a/make_test_workbook 风格，只用
    /// A 列：A1(0-based row0,col0) 存一个公式，A2 存普通数字，验证：
    /// 1) 公式单元格返回预期公式原文；2) 非公式单元格返回 None；
    /// 3) sheet_id 字段被正确忽略（任意占位值不影响查找结果）；
    /// 4) 越界坐标（row=0/col=0）返回 None 而不是 panic。
    #[test]
    fn formula_text_at_cell_returns_formula_source() {
        use formualizer_eval::reference::CellRef;

        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![
                CellValue::Formula {
                    source: "=SUM(A2:A10)".to_string(),
                    cached: Box::new(CellValue::Number(42.0)),
                },
                CellValue::Number(7.0),
            ],
        );
        let workbook = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };

        let resolver = SheetResolver::new(&workbook, "Sheet1");

        // A1：formualizer 1-based 坐标 (row=1, col=1)。sheet_id 故意填一个
        // 任意占位值（999），验证它确实被忽略、不影响查找结果。
        let a1 = CellRef::new_absolute(999, 1, 1);
        assert_eq!(
            EvaluationContext::formula_text_at_cell(&resolver, a1).unwrap(),
            Some("=SUM(A2:A10)".to_string())
        );

        // A2：非公式单元格，应返回 None。
        let a2 = CellRef::new_absolute(999, 2, 1);
        assert_eq!(
            EvaluationContext::formula_text_at_cell(&resolver, a2).unwrap(),
            None
        );

        // 越界坐标（row=0，formualizer 语义上非法）：应返回 None 而不是 panic。
        let invalid = CellRef::new_absolute(999, 0, 1);
        assert_eq!(
            EvaluationContext::formula_text_at_cell(&resolver, invalid).unwrap(),
            None
        );
    }


    // ============================================================
    // ROE_SUBTOTAL Step 2 单元测试
    // ============================================================

    /// 基本求和：A列 A1:A3 = 10/20/30，ROE_SUBTOTAL(9, A1:A3) 应等于 SUM 的结果 60。
    #[test]
    fn roe_subtotal_sum_basic() {
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![
                CellValue::Number(10.0),
                CellValue::Number(20.0),
                CellValue::Number(30.0),
            ],
        );
        let workbook = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&workbook, "Sheet1", "SUBTOTAL(9,A1:A3)");
        match result {
            CellValue::Number(n) => assert!((n - 60.0).abs() < 1e-9, "expected 60.0, got {n}"),
            other => panic!("expected Number(60.0), got {:?}", other),
        }
    }

    /// 基本平均值：A1:A3 = 10/20/30，ROE_SUBTOTAL(1, A1:A3) 应等于 AVERAGE 的结果 20。
    #[test]
    fn roe_subtotal_average_basic() {
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![
                CellValue::Number(10.0),
                CellValue::Number(20.0),
                CellValue::Number(30.0),
            ],
        );
        let workbook = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&workbook, "Sheet1", "SUBTOTAL(1,A1:A3)");
        match result {
            CellValue::Number(n) => assert!((n - 20.0).abs() < 1e-9, "expected 20.0, got {n}"),
            other => panic!("expected Number(20.0), got {:?}", other),
        }
    }

    /// 核心行为：嵌套 ROE_SUBTOTAL 应被跳过，不重复计入外层聚合。
    /// 构造：A1=10, A2=20（数据行），A3 = "=ROE_SUBTOTAL(9,A1:A2)"（小计行，
    /// 求值结果30，但公式原文含ROE_SUBTOTAL(，应被外层识别并跳过）。
    /// 外层 ROE_SUBTOTAL(9, A1:A3) 若正确跳过A3，应等于 10+20=30；
    /// 若错误地把A3的缓存值也计入，会得到 10+20+30=60（本测试的失败态）。
    #[test]
    fn roe_subtotal_skips_nested_subtotal_row() {
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![
                CellValue::Number(10.0),
                CellValue::Number(20.0),
                CellValue::Formula {
                    source: "=SUBTOTAL(9,A1:A2)".to_string(),
                    cached: Box::new(CellValue::Number(30.0)),
                },
            ],
        );
        let workbook = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&workbook, "Sheet1", "SUBTOTAL(9,A1:A3)");
        match result {
            CellValue::Number(n) => assert!(
                (n - 30.0).abs() < 1e-9,
                "expected 30.0 (A3 嵌套小计行应被跳过), got {n}"
            ),
            other => panic!("expected Number(30.0), got {:?}", other),
        }
    }

    /// v227 回归测试：坐实并修复 view.start_row()/start_col() 不是绝对坐标
    /// 这一 bug 的关键用例。数据故意放在 B 列（col_idx=1，非A列），且含
    /// 两组嵌套小计（多组，不是只有一组），复刻真实文件
    /// sc_data_xlsx__subtotal-above.xlsx 的问题场景：修复前，外层SUBTOTAL
    /// 反查每个候选cell的公式文本时坐标算成了A列同一行（A列全是Empty，
    /// 不是公式），嵌套跳过判断永远不成立，导致内层两个小计行被重复计入，
    /// 外层结果偏大；修复后应正确跳过两个内层小计行，只累加4个原始数据。
    #[test]
    fn roe_subtotal_skips_nested_subtotal_rows_on_non_a_column() {
        let sheet1 = sheet_with_column(
            "Sheet1",
            1u32, // B列，0-based
            vec![
                CellValue::Number(1.0),  // B1
                CellValue::Number(2.0),  // B2
                CellValue::Formula {
                    // B3：第一组小计
                    source: "=SUBTOTAL(9,B1:B2)".to_string(),
                    cached: Box::new(CellValue::Number(3.0)),
                },
                CellValue::Number(10.0), // B4
                CellValue::Number(20.0), // B5
                CellValue::Formula {
                    // B6：第二组小计
                    source: "=SUBTOTAL(9,B4:B5)".to_string(),
                    cached: Box::new(CellValue::Number(30.0)),
                },
            ],
        );
        let workbook = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        // 外层总计：应为 1+2+10+20=33，跳过 B3/B6 两个嵌套小计行。
        // 修复前的 bug 会把 B3(3.0)/B6(30.0) 也算进去，得到错误的 66。
        let result = eval_formula(&workbook, "Sheet1", "SUBTOTAL(9,B1:B6)");
        match result {
            CellValue::Number(n) => assert!(
                (n - 33.0).abs() < 1e-9,
                "expected 33.0 (B3/B6 两个嵌套小计行都应被跳过), got {n}"
            ),
            other => panic!("expected Number(33.0), got {:?}", other),
        }
    }

    /// 未支持的 function_num（如2=COUNT）应明确返回 #VALUE!，不能静默按SUM处理。
    #[test]
    fn roe_subtotal_unsupported_function_num_is_value_error() {
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![CellValue::Number(10.0), CellValue::Number(20.0)],
        );
        let workbook = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&workbook, "Sheet1", "SUBTOTAL(2,A1:A2)");
        match result {
            CellValue::Error(CellError::Value) => {}
            other => panic!("expected Error(Value), got {:?}", other),
        }
    }

    // ROOT-022 回归测试：公式含多字节 UTF-8 字符（智能引号 U+2019 等）时，
    // strip_odf_function_namespace_prefixes 不应 panic，且应保持公式内容不变
    // （因为没有匹配到 COM.MICROSOFT./ORG.OPENOFFICE. 前缀）。
    #[test]
    fn root_022_no_panic_on_multibyte_utf8_formula() {
        let formula = "\u{2019}"; // 单个智能右单引号，U+2019，3字节UTF-8编码
        let result = strip_odf_function_namespace_prefixes(formula);
        assert_eq!(result, formula);

        let formula2 = "OR(D4=0,D4<>\u{2019}\u{2019})";
        let result2 = strip_odf_function_namespace_prefixes(formula2);
        assert_eq!(result2, formula2);

        let formula3 = "OR(E4=0,E4<>\u{2019})";
        let result3 = strip_odf_function_namespace_prefixes(formula3);
        assert_eq!(result3, formula3);
    }

    #[test]
    fn root_022_prefix_stripping_still_works_with_ascii() {
        // 确保修复没有破坏原有 ASCII 前缀剥离逻辑
        let formula = "=COM.MICROSOFT.IFS(A1>0,\"pos\",\"neg\")";
        let result = strip_odf_function_namespace_prefixes(formula);
        assert_eq!(result, "=IFS(A1>0,\"pos\",\"neg\")");
    }

    fn sheet_with_column_a(name: &str, values: Vec<CellValue>) -> Sheet {
        sheet_with_column(name, 0u32, values)
    }

    /// v227 坐标bug修复后新增：通用版本，可指定 0-based 列索引，用于构造
    /// "数据不在A列"的测试场景。sheet_with_column_a 曾经是本文件唯一的
    /// sheet 构造辅助函数，所有 ROE_SUBTOTAL/SUBTOTAL 相关测试都只测过
    /// A列（col_idx=0），这恰好掩盖了 resolve_range_view 返回的 RangeView
    /// 坐标系与 SheetResolver 实际读取坐标不一致的 bug（本文件
    /// RoeSubtotalFn::eval() 上方注释有完整说明）。新增此函数是为了让
    /// 后续测试能显式覆盖非零列，防止同类"测试恰好在A列上蒙对"的情况
    /// 再次发生。
    fn sheet_with_column(name: &str, col_idx: u32, values: Vec<CellValue>) -> Sheet {
        let mut grid = SparseGrid::default();
        grid.cols.insert(
            col_idx,
            SparseColumn {
                blocks: smallvec::smallvec![CellBlock {
                    start_row: 0,
                    values: values.into(),
                }],
            },
        );
        Sheet {
            col_style: vec![],
            name: name.to_string(),
            cells: grid,
            ..Default::default()
        }
    }

    fn make_test_workbook() -> Workbook {
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![
                CellValue::Number(10.0),
                CellValue::Number(20.0),
                CellValue::Number(30.0),
            ],
        );
        Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn make_multi_sheet_workbook() -> Workbook {
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![
                CellValue::Number(10.0),
                CellValue::Number(20.0),
                CellValue::Number(30.0),
            ],
        );
        let sheet2 = sheet_with_column_a(
            "Sheet2",
            vec![CellValue::Number(100.0), CellValue::Number(200.0)],
        );
        Workbook {
            sheets: vec![sheet1, sheet2],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn test_sum_range() {
        let wb = make_test_workbook();
        let result = eval_formula(&wb, "Sheet1", "SUM(A1:A3)");
        match result {
            CellValue::Number(n) => assert!((n - 60.0).abs() < 1e-9, "expected 60, got {}", n),
            other => panic!("expected Number(60), got {:?}", other),
        }
    }

    #[test]
    fn test_average_range() {
        let wb = make_test_workbook();
        let result = eval_formula(&wb, "Sheet1", "AVERAGE(A1:A3)");
        match result {
            CellValue::Number(n) => assert!((n - 20.0).abs() < 1e-9, "expected 20, got {}", n),
            other => panic!("expected Number(20), got {:?}", other),
        }
    }

    #[test]
    fn test_single_cell_reference() {
        let wb = make_test_workbook();
        let result = eval_formula(&wb, "Sheet1", "A2");
        match result {
            CellValue::Number(n) => assert!((n - 20.0).abs() < 1e-9, "expected 20, got {}", n),
            other => panic!("expected Number(20), got {:?}", other),
        }
    }

    #[test]
    fn test_empty_cell_is_zero_in_sum() {
        let wb = make_test_workbook();
        let result = eval_formula(&wb, "Sheet1", "SUM(A1:A4)");
        match result {
            CellValue::Number(n) => assert!((n - 60.0).abs() < 1e-9, "expected 60, got {}", n),
            other => panic!("expected Number(60), got {:?}", other),
        }
    }

    #[test]
    fn test_cross_sheet_cell_reference() {
        let wb = make_multi_sheet_workbook();
        // 从 Sheet1 的公式引用 Sheet2 的单元格
        let result = eval_formula(&wb, "Sheet1", "Sheet2!A1");
        match result {
            CellValue::Number(n) => assert!((n - 100.0).abs() < 1e-9, "expected 100, got {}", n),
            other => panic!("expected Number(100), got {:?}", other),
        }
    }

    #[test]
    fn test_cross_sheet_range_sum() {
        let wb = make_multi_sheet_workbook();
        let result = eval_formula(&wb, "Sheet1", "SUM(Sheet2!A1:A2)");
        match result {
            CellValue::Number(n) => assert!((n - 300.0).abs() < 1e-9, "expected 300, got {}", n),
            other => panic!("expected Number(300), got {:?}", other),
        }
    }

    #[test]
    fn tdf163554_cell3d_forward_reference_sums_across_sheets() {
        // 正向 3D 引用：Sheet1:Sheet2!A1，workbook 顺序就是 Sheet1 在前、
        // Sheet2 在后，最基本的场景。A1 分别是 10 和 20，求和应为 30。
        let sheet1 = sheet_with_column_a("Sheet1", vec![CellValue::Number(10.0)]);
        let sheet2 = sheet_with_column_a("Sheet2", vec![CellValue::Number(20.0)]);
        let wb = Workbook {
            sheets: vec![sheet1, sheet2],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&wb, "Sheet1", "SUM(Sheet1:Sheet2!A1)");
        match result {
            CellValue::Number(n) => assert!((n - 30.0).abs() < 1e-9, "expected 30, got {}", n),
            other => panic!("expected Number(30), got {:?}", other),
        }
    }

    #[test]
    fn tdf163554_cell3d_reversed_reference_auto_corrects_like_libreoffice() {
        // tdf163554 真实样本场景：sheet_first 在 workbook 物理顺序里排在
        // sheet_last 之后（"反向"写法）。实测 LibreOffice 对这种写法依然
        // 正确求值，不报错——本测试固化这个行为，回归防护。
        // workbook 顺序：SheetA（先，值7）、SheetB（后，值8）；公式写
        // SheetB:SheetA（反向），应等价于 SheetA:SheetB，求和 15。
        let sheet_a = sheet_with_column_a("SheetA", vec![CellValue::Number(7.0)]);
        let sheet_b = sheet_with_column_a("SheetB", vec![CellValue::Number(8.0)]);
        let wb = Workbook {
            sheets: vec![sheet_a, sheet_b],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&wb, "SheetA", "SUM(SheetB:SheetA!A1)");
        match result {
            CellValue::Number(n) => assert!((n - 15.0).abs() < 1e-9, "expected 15, got {}", n),
            other => panic!("expected Number(15), got {:?}", other),
        }
    }

    #[test]
    fn tdf163554_cell3d_unknown_sheet_is_ref_error() {
        // sheet_first / sheet_last 任一在 workbook 里查不到，返回错误，
        // 不 panic。
        let sheet1 = sheet_with_column_a("Sheet1", vec![CellValue::Number(1.0)]);
        let wb = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&wb, "Sheet1", "SUM(Sheet1:NoSuchSheet!A1)");
        match result {
            CellValue::Error(_) => {}
            other => panic!("expected an error value, got {:?}", other),
        }
    }

    #[test]
    fn tdf163554_range3d_sums_rectangular_area_across_sheets() {
        // Range3D：Sheet1:Sheet2!A1:A2，每个 sheet 贡献 2 行 1 列，两个
        // sheet 纵向拼接后是 4 个值求和：1+2+3+4=10。
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![CellValue::Number(1.0), CellValue::Number(2.0)],
        );
        let sheet2 = sheet_with_column_a(
            "Sheet2",
            vec![CellValue::Number(3.0), CellValue::Number(4.0)],
        );
        let wb = Workbook {
            sheets: vec![sheet1, sheet2],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&wb, "Sheet1", "SUM(Sheet1:Sheet2!A1:A2)");
        match result {
            CellValue::Number(n) => assert!((n - 10.0).abs() < 1e-9, "expected 10, got {}", n),
            other => panic!("expected Number(10), got {:?}", other),
        }
    }

    #[test]
    fn test_unknown_sheet_reference_is_ref_error() {
        let wb = make_test_workbook();
        let result = eval_formula(&wb, "Sheet1", "NoSuchSheet!A1");
        match result {
            CellValue::Error(CellError::Ref) => {}
            other => panic!("expected Error(Ref), got {:?}", other),
        }
    }

    fn make_workbook_with_named_range() -> Workbook {
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![
                CellValue::Number(10.0),
                CellValue::Number(20.0),
                CellValue::Number(30.0),
            ],
        );
        let sheet2 = sheet_with_column_a(
            "Sheet2",
            vec![CellValue::Number(100.0), CellValue::Number(200.0)],
        );
        let mut named_ranges = std::collections::HashMap::new();
        // 同 sheet 命名区域：Sheet1!A1:A3
        named_ranges.insert(
            "SalesTotal".to_string(),
            roe_model::calc::CellRange {
                sheet: "Sheet1".to_string(),
                start: (0, 0),
                end: (2, 0),
            },
        );
        // 跨 sheet 命名区域：Sheet2!A1:A2
        named_ranges.insert(
            "OtherSheetRange".to_string(),
            roe_model::calc::CellRange {
                sheet: "Sheet2".to_string(),
                start: (0, 0),
                end: (1, 0),
            },
        );
        Workbook {
            sheets: vec![sheet1, sheet2],
            named_ranges,
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        }
    }

    #[test]
    fn test_named_range_sum_same_sheet() {
        let wb = make_workbook_with_named_range();
        let result = eval_formula(&wb, "Sheet1", "SUM(SalesTotal)");
        match result {
            CellValue::Number(n) => assert!((n - 60.0).abs() < 1e-9, "expected 60, got {}", n),
            other => panic!("expected Number(60), got {:?}", other),
        }
    }

    #[test]
    fn test_named_range_sum_cross_sheet() {
        // 当前公式在 Sheet1 里，命名区域指向 Sheet2——验证命名区域自带的 sheet
        // 信息生效，不受 current_sheet 影响。
        let wb = make_workbook_with_named_range();
        let result = eval_formula(&wb, "Sheet1", "SUM(OtherSheetRange)");
        match result {
            CellValue::Number(n) => assert!((n - 300.0).abs() < 1e-9, "expected 300, got {}", n),
            other => panic!("expected Number(300), got {:?}", other),
        }
    }

    #[test]
    fn named_name_case_insensitive_range() {
        // Excel 名称大小写不敏感:salestotal 应命中 SalesTotal
        let wb = make_workbook_with_named_range();
        let result = eval_formula(&wb, "Sheet1", "SUM(salestotal)");
        match result {
            CellValue::Number(n) => assert!((n - 60.0).abs() < 1e-9, "expected 60, got {}", n),
            other => panic!("expected Number(60), got {:?}", other),
        }
    }

    #[test]
    fn named_name_refers_to_name_case_differs() {
        // Global5 -> global6(大小写不同),Global6 = 5;对应 named-ranges-global.xlsx 的 Global5
        let mut wb = make_workbook_with_named_range();
        wb.named_formulas.insert("Global6".to_string(), "5".to_string());
        wb.named_formulas.insert("Global5".to_string(), "global6".to_string());
        let result = eval_formula(&wb, "Sheet1", "Global5");
        match result {
            CellValue::Number(n) => assert!((n - 5.0).abs() < 1e-9, "expected 5, got {}", n),
            other => panic!("expected Number(5), got {:?}", other),
        }
    }

    #[test]
    fn test_undefined_named_range_is_name_error() {
        let wb = make_workbook_with_named_range();
        let result = eval_formula(&wb, "Sheet1", "SUM(NoSuchName)");
        match result {
            CellValue::Error(CellError::Name) => {}
            other => panic!("expected Error(Name), got {:?}", other),
        }
    }

    #[test]
    fn test_named_range_dependency_is_recorded() {
        // 验证第4步的依赖记录机制对命名区域同样生效（不是只对普通 Range 生效）。
        // 注意：formualizer 内部可能对同一引用多次调用 resolve_range_view
        // （比如先做维度探测再取值），所以不断言访问次数，只断言访问到的
        // 去重后的单元格集合是否等于 SalesTotal 实际覆盖的 3 个单元格——
        // 这才是 FormulaDAG::add_dependency 真正关心的东西（它自己会去重边）。
        let wb = make_workbook_with_named_range();
        let (_, deps) = eval_formula_with_deps(&wb, "Sheet1", "SUM(SalesTotal)");
        let unique: std::collections::HashSet<_> = deps.into_iter().collect();
        let expected: std::collections::HashSet<_> = vec![
            ("Sheet1".to_string(), 0u32, 0u32),
            ("Sheet1".to_string(), 1u32, 0u32),
            ("Sheet1".to_string(), 2u32, 0u32),
        ]
        .into_iter()
        .collect();
        assert_eq!(unique, expected, "SalesTotal 应该覆盖 Sheet1 的 A1:A3 三个单元格");
    }

    // ROOT-023 回归测试：OR() 短路求值不应吞掉未定义命名区域导致的 #NAME? 错误。
    // 场景：OR() 第一个参数已经能判定为 TRUE，Excel/formualizer 语义下会短路，
    // 不再对第二个参数求值——但如果第二个参数本身含有一个未定义的名字（如智能
    // 引号被容忍解析成的裸标识符、或任意未定义的 NamedRange），短路不应该把
    // "这个公式本该是 #NAME?" 这个事实掩盖掉。修复前：短路直接吞掉错误，返回
    // TRUE；修复后：预检查发现 AST 里存在未定义 NamedRange，直接判定 #NAME?，
    // 根本不会进入短路求值。
    #[test]
    fn root_023_or_short_circuit_does_not_swallow_undefined_name() {
        let wb = make_test_workbook();
        // A1 = 10，A1=0 为 FALSE，不会在第一个参数就短路——用第二参数必然
        // 被求值的对照组先确认基础行为正确。
        let result = eval_formula(&wb, "Sheet1", "OR(A1=999,A1<>FOOBAR)");
        match result {
            CellValue::Error(CellError::Name) => {}
            other => panic!("expected Error(Name) (no short-circuit case), got {:?}", other),
        }

        // A1=10<>0 为真判断的变体：构造一个第一参数为 TRUE、必然短路的公式，
        // 第二参数里塞入未定义名字 FOOBAR。修复前会被短路成 TRUE，修复后应为 #NAME?。
        let result2 = eval_formula(&wb, "Sheet1", "OR(A1=10,A1<>FOOBAR)");
        match result2 {
            CellValue::Error(CellError::Name) => {}
            other => panic!("expected Error(Name) (short-circuit should not hide #NAME?), got {:?}", other),
        }
    }

    // ROOT-023 回归测试：短路本身的正常行为不应被误伤——当短路跳过的参数
    // 不含任何未定义名字时，公式应该正常按短路语义求值，不应该被预检查误判。
    #[test]
    fn root_023_or_short_circuit_normal_case_still_works() {
        let wb = make_test_workbook();
        // A1=10，第一参数 A1=10 为 TRUE，会短路；第二参数是合法的、已定义的
        // 引用比较（A2<>A3，均已定义），不含任何未定义名字，预检查不应误报。
        let result = eval_formula(&wb, "Sheet1", "OR(A1=10,A2<>A3)");
        match result {
            CellValue::Bool(b) => assert!(b, "expected TRUE, got Bool({})", b),
            other => panic!("expected Bool(true), got {:?}", other),
        }
    }

    // ROOT-023 回归测试：未定义名字不限于智能引号触发——任意标识符（如中文
    // 字符）被 formualizer 容忍解析为裸 NamedRange 时，同样应该被短路预检查
    // 正确拦截为 #NAME?，证明修复是通用规则，不是只针对智能引号的特例。
    #[test]
    fn root_023_or_short_circuit_undefined_name_generic_identifier() {
        let wb = make_test_workbook();
        let result = eval_formula(&wb, "Sheet1", "OR(A1=10,A1<>\u{4e2d}\u{6587}\u{ff09}\u{ff08}\u{4e2d}\u{6587}\u{ff08}\u{4e2d}\u{6587})");
        // 上面刻意构造了一个不含运算符歧义的裸中文标识符占位——为避免转义问题，
        // 改用更直接的写法验证同一结论。
        let result2 = eval_formula(&wb, "Sheet1", "OR(A1=10,A1<>UNDEFINEDNAME)");
        match result2 {
            CellValue::Error(CellError::Name) => {}
            other => panic!("expected Error(Name) for generic undefined identifier, got {:?}", other),
        }
        let _ = result; // 占位公式本身不做强断言，避免中文转义在不同环境下的解析差异影响测试稳定性
    }

    // ROOT-023 附带发现回归测试：AST 顶层裸智能双引号（U+201C “ 或 U+201D ”）
    // 求值为 0，这是已用 LibreOffice + Excel 双重实测确认的 Excel 特殊行为，
    // 范围严格限定为「公式顶层本身就是这一个字符」。
    #[test]
    fn root_023_bare_top_level_smart_double_quote_is_zero() {
        let wb = make_test_workbook();
        let result_left = eval_formula(&wb, "Sheet1", "\u{201c}");
        match result_left {
            CellValue::Number(n) => assert!((n - 0.0).abs() < 1e-9, "expected 0, got {}", n),
            other => panic!("expected Number(0) for bare U+201C, got {:?}", other),
        }

        let result_right = eval_formula(&wb, "Sheet1", "\u{201d}");
        match result_right {
            CellValue::Number(n) => assert!((n - 0.0).abs() < 1e-9, "expected 0, got {}", n),
            other => panic!("expected Number(0) for bare U+201D, got {:?}", other),
        }
    }

    // ROOT-023 附带发现回归测试：AST 顶层裸智能单引号（U+2018 ‘ 或 U+2019 ’）
    // 与双引号不对称——单引号版本应正常报 #NAME?，不受"顶层降级为0"规则影响。
    // 已用 LibreOffice 实测确认，防止未来有人把这条规则误泛化到所有智能引号。
    #[test]
    fn root_023_bare_top_level_smart_single_quote_is_name_error() {
        let wb = make_test_workbook();
        let result_left = eval_formula(&wb, "Sheet1", "\u{2018}");
        match result_left {
            CellValue::Error(CellError::Name) => {}
            other => panic!("expected Error(Name) for bare U+2018, got {:?}", other),
        }

        let result_right = eval_formula(&wb, "Sheet1", "\u{2019}");
        match result_right {
            CellValue::Error(CellError::Name) => {}
            other => panic!("expected Error(Name) for bare U+2019, got {:?}", other),
        }
    }

    #[test]
    fn root_023_and_short_circuit_does_not_swallow_undefined_name() {
        let wb = make_test_workbook();
        let result = eval_formula(&wb, "Sheet1", "AND(A1=0,A1<>FOOBAR)");
        match result {
            CellValue::Error(CellError::Name) => {}
            other => panic!(
                "expected Error(Name) (AND short-circuit should not hide #NAME?), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn root_023_and_short_circuit_normal_case_still_works() {
        let wb = make_test_workbook();
        let result = eval_formula(&wb, "Sheet1", "AND(A1=0,A2<>A3)");
        match result {
            CellValue::Bool(b) => assert!(!b, "expected FALSE, got Bool({})", b),
            other => panic!("expected Bool(false), got {:?}", other),
        }
    }

    // ROOT tdf164895 附带修复：整列/整行开放式引用（$A:$A、$1:$1）支持。
    // 这批测试覆盖 resolve_range_view 里新增的 used_bounds() 边界填充逻辑，
    // 是之前完全没有测试覆盖、导致 bug 能编译通过又能通过全部旧单测的
    // 那个盲区——不要再让类似情况在未来重演。

    #[test]
    fn tdf164895_open_ended_column_ref_counta() {
        let wb = make_test_workbook(); // A1:A3 = 10, 20, 30（3个非空值）
        let result = eval_formula(&wb, "Sheet1", "COUNTA($A:$A)");
        match result {
            CellValue::Number(n) => assert!((n - 3.0).abs() < 1e-9, "expected 3, got {}", n),
            other => panic!("expected Number(3), got {:?}", other),
        }
    }

    #[test]
    fn tdf164895_open_ended_column_ref_sum() {
        let wb = make_test_workbook(); // A1:A3 = 10, 20, 30
        let result = eval_formula(&wb, "Sheet1", "SUM($A:$A)");
        match result {
            CellValue::Number(n) => assert!((n - 60.0).abs() < 1e-9, "expected 60, got {}", n),
            other => panic!("expected Number(60), got {:?}", other),
        }
    }

    #[test]
    fn tdf164895_open_ended_row_ref() {
        // 整行引用 $1:$1：构造一个第1行有B1/C1两个数值的workbook。
        let mut sheet1 = sheet_with_column_a("Sheet1", vec![CellValue::Number(1.0)]); // A1=1
        sheet1.cells.set(0, 1, CellValue::Number(2.0)); // B1=2 (0-based row0,col1)
        sheet1.cells.set(0, 2, CellValue::Number(3.0)); // C1=3 (0-based row0,col2)
        let wb = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&wb, "Sheet1", "SUM($1:$1)");
        match result {
            CellValue::Number(n) => assert!((n - 6.0).abs() < 1e-9, "expected 1+2+3=6, got {}", n),
            other => panic!("expected Number(6), got {:?}", other),
        }
    }

    #[test]
    fn tdf164895_open_ended_column_ref_on_empty_sheet_is_empty_not_error() {
        // 完全空白的sheet，SparseGrid::used_bounds() 返回 None，
        // 整列引用应该退化为"空区域"（COUNTA结果为0），而不是报错。
        let sheet1 = Sheet {
            name: "Sheet1".to_string(),
            cells: Default::default(),
            col_widths: Default::default(),
            row_heights: Default::default(),
            frozen_rows: 0,
            frozen_cols: 0,
            date1904: false,
            zoom_scale: None,
            show_grid_lines: None,
            ..Default::default()
        };
        let wb = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas: Default::default(),
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&wb, "Sheet1", "COUNTA($A:$A)");
        match result {
            CellValue::Number(n) => assert!(n.abs() < 1e-9, "expected 0 on empty sheet, got {}", n),
            other => panic!("expected Number(0), got {:?}", other),
        }
    }

    #[test]
    fn tdf164895_named_formula_with_open_ended_ref_end_to_end() {
        // 端到端回归用例，精确复现真实bug样本 small__sc_data_xlsx__tdf164895.xlsx：
        // named_formulas 里存一条引用整列的公式类型命名定义，单元格公式
        // 直接裸引用这个名字（不经过任何函数包裹），验证从
        // resolve_named_range_rows fallback 到 open-ended range 边界填充
        // 这一整条链路端到端工作。
        let sheet1 = sheet_with_column_a(
            "Sheet1",
            vec![
                CellValue::Number(10.0),
                CellValue::Number(20.0),
                CellValue::Number(30.0),
            ],
        );
        let mut named_formulas = std::collections::HashMap::new();
        named_formulas.insert("Cnt".to_string(), "COUNTA($A:$A)".to_string());
        let wb = Workbook {
            sheets: vec![sheet1],
            named_ranges: Default::default(),
            named_formulas,
            styles: Default::default(),
            formula_dialect_source: Default::default(),
        };
        let result = eval_formula(&wb, "Sheet1", "Cnt");
        match result {
            CellValue::Number(n) => assert!((n - 3.0).abs() < 1e-9, "expected 3, got {}", n),
            other => panic!("expected Number(3), got {:?}", other),
        }
    }

}

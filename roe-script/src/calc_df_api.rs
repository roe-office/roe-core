//! DataFrame Rhai API（calc_df_api）
//!
//! WORKING_DF：thread_local LazyFrame，所有链式变换只追加逻辑计划，
//! 只有 df_preview / df_to_sheet 时才隐式 collect()。
//!
//! 返回值统一为 String："ok" 或 "err:xxx"，Rhai 层可用 if 判断。
//! 不 panic，所有 polars/calamine 错误都包装为 "err:..." 字符串返回。

use polars::prelude::{
    col, DataFrame, Expr, IntoLazy, JoinArgs, JoinType, LazyFrame,
    SortMultipleOptions, UniqueKeepStrategy,
};
use rhai::Engine;
use roe_model::calc::Workbook;
use std::cell::RefCell;

thread_local! {
    static WORKING_DF: RefCell<Option<LazyFrame>> = const { RefCell::new(None) };
}

// 会话级数据集:按文档归属(存储在 roe-api 的 handle 侧表,随句柄释放)。
// 脚本执行期间由 run_calc_ds 绑定 DATASETS_PTR(与 WORKBOOK_PTR 同模式);
// 不随 unbind_workbook_df 清空(WORKING_DF 每次脚本结束都会清)。
// 未绑定时 df_save_as 等返回 "err:数据集容器未绑定",不做静默回退。
/// 数据集:内存物化(Mem)或惰性引用(Lazy,持 LazyFrame,不物化全表)。
#[derive(Clone)]
pub enum Dataset {
    Mem(DataFrame),
    Lazy(LazyFrame),
}

impl Dataset {
    pub fn to_lazy(&self) -> LazyFrame {
        match self {
            Dataset::Mem(df) => df.clone().lazy(),
            Dataset::Lazy(lf) => lf.clone(),
        }
    }
}

pub type DatasetStore = std::collections::BTreeMap<String, Dataset>;

thread_local! {
    static DATASETS_PTR: RefCell<Option<*mut DatasetStore>> = const { RefCell::new(None) };
}

/// 绑定数据集容器(脚本执行前调用,与 unbind_datasets 配套)。
pub fn bind_datasets(ds: &mut DatasetStore) {
    DATASETS_PTR.with(|c| *c.borrow_mut() = Some(ds as *mut DatasetStore));
}

/// 解绑数据集容器(不清空容器内容)。
pub fn unbind_datasets() {
    DATASETS_PTR.with(|c| *c.borrow_mut() = None);
}

fn with_ds<R, F: FnOnce(&mut DatasetStore) -> R>(f: F) -> Result<R, String> {
    DATASETS_PTR.with(|c| {
        let ptr = c.borrow().ok_or_else(|| "err:数据集容器未绑定".to_string())?;
        Ok(f(unsafe { &mut *ptr }))
    })
}

/// 下一个自动数据集名 df_N(N = 现有 df_数字 的最大值 + 1)。导入与 SQL 另存共用。
pub fn next_df_name(ds: &DatasetStore) -> String {
    next_name(ds, "df")
}

/// 带前缀的下一个自动名 prefix_N(N = 现有 prefix_数字 的最大值 + 1)。
/// 约定:df=导入的源数据,sql=SQL 查询结果,chain=DF 链结果。
pub fn next_name(ds: &DatasetStore, prefix: &str) -> String {
    let pre = format!("{prefix}_");
    let n = ds
        .keys()
        .filter_map(|k| k.strip_prefix(pre.as_str()).and_then(|x| x.parse::<u64>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    format!("{prefix}_{n}")
}

/// 把 DF 链代码包装成"每条语句失败即中止并报行号"。
/// 约定一行一条语句。只包装形如 `df_xxx(...);` 的整行语句;
/// 空行/注释行/let、if、for 等开头的行/不以 ; 结尾的行(含末行表达式)原样保留。
pub fn wrap_chain(code: &str) -> String {
    const KEEP: [&str; 11] = ["let ", "const ", "if ", "if(", "for ", "while ", "loop", "fn ", "throw ", "return ", "}"];
    let mut out = String::new();
    for (i, raw) in code.lines().enumerate() {
        let t = raw.trim();
        let keep = t.is_empty()
            || t.starts_with("//")
            || t.starts_with('{')
            || !t.ends_with(';')
            || KEEP.iter().any(|p| t.starts_with(p));
        if keep {
            out.push_str(raw);
            out.push('\n');
            continue;
        }
        let stmt = &t[..t.len() - 1];
        out.push_str(&format!(
            "{{ let __r = {stmt}; if type_of(__r) == \"string\" && __r.starts_with(\"err:\") {{ throw \"第{}行: \" + __r; }} }}\n",
            i + 1
        ));
    }
    out
}

fn datasets_snapshot() -> Result<Vec<(String, LazyFrame)>, String> {
    with_ds(|d| d.iter().map(|(k, v)| (k.clone(), v.to_lazy())).collect())
}

// CSV→IPC 缓存目录(进程级配置,不随文档变,故用 thread_local 可接受)。
// 未设置时回退 temp_dir/roe_csv_cache。不放 Agent workspace:
// 设计文档明确不设计缓存层、且 workspace 内文件访问必须经 cap-std,
// 而 polars 只吃物理路径。
thread_local! {
    static CSV_CACHE_DIR: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
}

/// 设置(或用 None 清除)CSV 缓存目录。
pub fn set_csv_cache_dir(dir: Option<std::path::PathBuf>) {
    CSV_CACHE_DIR.with(|c| *c.borrow_mut() = dir);
}

pub fn csv_cache_dir() -> std::path::PathBuf {
    CSV_CACHE_DIR
        .with(|c| c.borrow().clone())
        .unwrap_or_else(|| std::env::temp_dir().join("roe_csv_cache"))
}


// 注意：WORKBOOK 指针与 calc_api 共享同一个 thread_local 模式，
// 但 calc_df_api 有自己的 WORKBOOK_PTR，避免与 calc_api 耦合。
thread_local! {
    static WORKBOOK_PTR: RefCell<Option<*mut Workbook>> = const { RefCell::new(None) };
}

pub fn with_workbook_df<F>(wb: &mut Workbook, f: F)
where
    F: FnOnce(),
{
    WORKBOOK_PTR.with(|c| *c.borrow_mut() = Some(wb as *mut Workbook));
    f();
    WORKBOOK_PTR.with(|c| *c.borrow_mut() = None);
    // 脚本执行完后清理 WORKING_DF，避免跨调用污染
    WORKING_DF.with(|c| *c.borrow_mut() = None);
}

/// 仅绑定 WORKBOOK_PTR，不执行、不清理。
/// 供 run_calc（engine.rs）与 calc_api::with_workbook 配合使用，
/// 避免同一个 &mut Workbook 被两层闭包同时捕获导致的借用冲突（方案A修复）。
pub fn bind_workbook_df(wb: &mut Workbook) {
    WORKBOOK_PTR.with(|c| *c.borrow_mut() = Some(wb as *mut Workbook));
}

/// 解绑 WORKBOOK_PTR 并清理 WORKING_DF，与 bind_workbook_df 配套使用。
pub fn unbind_workbook_df() {
    WORKBOOK_PTR.with(|c| *c.borrow_mut() = None);
    WORKING_DF.with(|c| *c.borrow_mut() = None);
}

fn with_wb<R, F: FnOnce(&mut Workbook) -> R>(f: F) -> Result<R, String> {
    WORKBOOK_PTR.with(|c| {
        let ptr = c.borrow().ok_or_else(|| "err:workbook未绑定".to_string())?;
        Ok(f(unsafe { &mut *ptr }))
    })
}

fn get_lf() -> Result<LazyFrame, String> {
    WORKING_DF.with(|c| {
        c.borrow()
            .clone()
            .ok_or_else(|| "err:WORKING_DF为空，请先调用df_load_sheet或df_load_file".to_string())
    })
}

fn set_lf(lf: LazyFrame) {
    WORKING_DF.with(|c| *c.borrow_mut() = Some(lf));
}

fn collect_lf() -> Result<DataFrame, String> {
    let lf = get_lf()?;
    lf.collect().map_err(|e| format!("err:collect失败: {e}"))
}

// ════════════════════════════════════════════════════════════
// 注册所有 df 函数到 Rhai Engine
// ════════════════════════════════════════════════════════════

/// 只读环境:先全量注册,再用同名桩覆盖「碰文件系统 / 写数据集容器 / 写 workbook」的函数。
/// (只读引擎的调用方含 AI agent,不得让脚本读本机任意文件或改动数据集。)
pub fn register_readonly(engine: &mut Engine) {
    register(engine);
    const MSG: &str = "err:只读环境不可用(文件加载/写数据集/写工作簿)";
    engine.register_fn("df_load_file", |_p: String| -> String { MSG.to_string() });
    engine.register_fn("df_load_csv", |_p: String, _d: String, _h: bool| -> String { MSG.to_string() });
    engine.register_fn("df_load_csv_cached", |_p: String, _d: String, _h: bool| -> String { MSG.to_string() });
    engine.register_fn("df_load_parquet", |_p: String| -> String { MSG.to_string() });
    engine.register_fn("df_load_parquet_dir", |_p: String| -> String { MSG.to_string() });
    engine.register_fn("df_save_as", |_n: String| -> String { MSG.to_string() });
    engine.register_fn("df_drop_dataset", |_n: String| -> String { MSG.to_string() });
    engine.register_fn("df_to_sheet", |_n: String| -> i64 { -1 });
    engine.register_fn("df_pivot_to_sheet", |_c: String, _n: String| -> i64 { -1 });
    engine.register_fn("df_to_sheet_head", |_n: String, _k: i64| -> i64 { -1 });
    engine.register_fn("df_sql_save", |_s: String| -> String { MSG.to_string() });
    engine.register_fn("df_save_result", || -> String { MSG.to_string() });
}

pub fn register(engine: &mut Engine) {
    // ── 导入 ──────────────────────────────────────────────

    // df_load_sheet(sheet_idx) -> "ok" | "err:..."
    // 把 workbook 中指定 sheet 载入为 WORKING_DF（LazyFrame）
    engine.register_fn("df_load_sheet", |sheet_idx: i64| -> String {
        match with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)
                .ok_or_else(|| format!("err:sheet索引{}越界", sheet_idx))?;
            let df = roe_polars_bridge::sheet_to_dataframe(sheet)
                .map_err(|e| format!("err:{e}"))?;
            set_lf(df.lazy());
            Ok::<_, String>("ok".to_string())
        }) {
            Ok(inner) => inner.unwrap_or_else(|e| e),
            Err(e) => e,
        }
    });

    // df_load_file(path) -> "ok" | "err:..."
    // 直接从外部 xlsx/xls/ods 文件载入第一个 sheet（calamine 旁路，不进内存模型）
    engine.register_fn("df_load_file", |path: String| -> String {
        match roe_polars_bridge::df_from_xlsx_file(&path) {
            Ok(df) => { set_lf(df.lazy()); "ok".to_string() }
            Err(e) => format!("err:{e}"),
        }
    });

    // df_load_csv(path, delimiter, has_header) -> "ok" | "err:..."
    // 直接把 CSV 读成 DataFrame 放入 WORKING_DF,不进工作表格子(百万~千万行场景)。
    // delimiter 传空串表示自动探测;编码自动识别(UTF-8/BOM/UTF-16/GBK)。
    engine.register_fn("df_load_csv", |path: String, delimiter: String, has_header: bool| -> String {
        let d = delimiter.chars().next();
        match roe_polars_bridge::df_from_csv_file(&path, d, has_header) {
            Ok(df) => { set_lf(df.lazy()); "ok".to_string() }
            Err(e) => format!("err:{e}"),
        }
    });

    // df_load_csv_cached(path, delimiter, has_header) -> "ok" | "err:..."
    // 与 df_load_csv 相同参数;首次导入写 IPC 缓存,之后命中缓存只做 scan_ipc
    // (惰性、免重新解析)。WORKING_DF 为指向缓存文件的 LazyFrame。
    engine.register_fn("df_load_csv_cached", |path: String, delimiter: String, has_header: bool| -> String {
        let d = delimiter.chars().next();
        let ipc = match roe_polars_bridge::csv_to_ipc_cache(&path, d, has_header, &csv_cache_dir()) {
            Ok(p) => p,
            Err(e) => return format!("err:{e}"),
        };
        match roe_polars_bridge::scan_ipc_cache(&ipc) {
            Ok(lf) => { set_lf(lf); "ok".to_string() }
            Err(e) => format!("err:{e}"),
        }
    });

    // df_load_parquet(path) -> "ok" | "err:..."
    // 惰性扫描单个 parquet 文件,WORKING_DF 为 LazyFrame,不物化全表。
    engine.register_fn("df_load_parquet", |path: String| -> String {
        match roe_polars_bridge::scan_parquet_file(&path) {
            Ok(lf) => { set_lf(lf); "ok".to_string() }
            Err(e) => format!("err:{e}"),
        }
    });

    // df_load_parquet_dir(path) -> "ok" | "err:..."
    // path 可为文件或目录,可带 file: 前缀(Hive LOCATION)。目录取直接子文件(跳过 .crc/_SUCCESS)合并。
    engine.register_fn("df_load_parquet_dir", |path: String| -> String {
        match roe_polars_bridge::scan_parquet_dir(&path) {
            Ok(lf) => { set_lf(lf); "ok".to_string() }
            Err(e) => format!("err:{e}"),
        }
    });

    // ── 探查 ──────────────────────────────────────────────

    // df_info() -> JSON schema | "err:..."
    // 只返回列名和类型，不 collect（LazyFrame 不物化）
        // df_join(sheet_idx, left_on, right_on, how) -> "ok" | "err:..."
    // gap analysis 2.9核实结论：calc_df_api此前完全没有join能力（不是polars
    // 不支持——polars本身LazyFrame::join功能齐全，是ROE的Rhai封装层没有暴露）。
    // 本函数把当前 WORKING_DF 与另一个 sheet（sheet_idx 指定）做关联，结果
    // 覆盖 WORKING_DF，与其他 df_* 链式操作行为一致。
    //
    // 范围裁剪（本次实现不做，需要时再补）：
    // - 只支持单列join（left_on/right_on各一个列名字符串），不支持多列联合
    //   关联key——多列join需要JSON数组参数，Excel/WPS用户实际场景里单列
    //   关联（订单号/客户ID等单一主键）占绝大多数，多列场景需要时再加
    //   df_join_multi
    // - 第二张表只能来自当前workbook内的sheet（复用已有的sheet_to_dataframe），
    //   不支持join外部文件——如需要可仿照df_load_file模式另加df_join_file，
    //   保持每个函数职责单一
    // - how只支持inner/left/right/full四种最常用类型，不支持semi/anti/asof/
    //   cross/iejoin这些进阶join——这些在Excel/WPS用户心智模型里几乎没有
    //   对应概念，VLOOKUP/XLOOKUP场景基本只需要inner/left
    //
    // 列名冲突：沿用polars默认行为，右表同名列自动加"_right"后缀，不做
    // 额外处理。
    engine.register_fn("df_join", |sheet_idx: i64, left_on: String, right_on: String, how: String| -> String {
        let join_type = match how.to_lowercase().as_str() {
            "inner" => JoinType::Inner,
            "left"  => JoinType::Left,
            "right" => JoinType::Right,
            "full"  => JoinType::Full,
            other => return format!("err:不支持的join类型\"{other}\"，仅支持inner/left/right/full"),
        };

        let right_lf = match with_wb(|wb| {
            let sheet = wb.sheets.get(sheet_idx as usize)
                .ok_or_else(|| format!("err:sheet索引{}越界", sheet_idx))?;
            let df = roe_polars_bridge::sheet_to_dataframe(sheet)
                .map_err(|e| format!("err:{e}"))?;
            Ok::<_, String>(df.lazy())
        }) {
            Ok(Ok(lf)) => lf,
            Ok(Err(e)) => return e,
            Err(e) => return e,
        };

        match get_lf() {
            Ok(left_lf) => {
                let joined = left_lf.join(
                    right_lf,
                    [col(left_on.as_str())],
                    [col(right_on.as_str())],
                    JoinArgs::new(join_type),
                );
                set_lf(joined);
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    engine.register_fn("df_info", || -> String {
        match get_lf() {
            Ok(mut lf) => {
                match roe_polars_bridge::lazy_schema_info(&mut lf) {
                    Ok(s) => s,
                    Err(e) => format!("err:{e}"),
                }
            }
            Err(e) => e,
        }
    });

    // df_preview(n_rows) -> JSON | "err:..."
    // collect 后取前 n_rows 行，返回 {columns, rows, total_rows}
    engine.register_fn("df_preview", |n_rows: i64| -> String {
        match collect_lf() {
            Ok(df) => match roe_polars_bridge::df_preview(&df, n_rows as usize) {
                Ok(s) => s,
                Err(e) => format!("err:{e}"),
            },
            Err(e) => e,
        }
    });

    // df_dataset_overview(name, head_n) -> JSON | "err:..."
    // 只读:从数据集容器取出并惰性概览,不改 WORKING_DF、不写容器。
    engine.register_fn("df_dataset_overview", |name: String, head_n: i64| -> String {
        let got = match with_ds(|d| d.get(name.trim()).map(|x| x.to_lazy())) {
            Ok(g) => g,
            Err(e) => return e,
        };
        match got {
            Some(lf) => match roe_polars_bridge::lazy_overview(lf, head_n.clamp(0, 1000) as u32) {
                Ok(s) => s,
                Err(e) => format!("err:{e}"),
            },
            None => format!("err:数据集\"{}\"不存在", name),
        }
    });

    // df_page(offset, limit) -> JSON | "err:..."  惰性分页,不 collect 全表
    engine.register_fn("df_page", |offset: i64, limit: i64| -> String {
        match get_lf() {
            Ok(lf) => match roe_polars_bridge::lazy_page(lf, offset, limit.max(0) as u32) {
                Ok(s) => s,
                Err(e) => format!("err:{e}"),
            },
            Err(e) => e,
        }
    });

    // df_unique_values(col_name) -> JSON 数组 | "err:..."
    engine.register_fn("df_unique_values", |col: String| -> String {
        match collect_lf() {
            Ok(df) => match roe_polars_bridge::df_unique_values(&df, &col) {
                Ok(s) => s,
                Err(e) => format!("err:{e}"),
            },
            Err(e) => e,
        }
    });

    // ── 链式变换原语（只追加逻辑计划，不 collect）────────

    // df_select(cols_json) — 选列，cols_json = ["A","B"]
    engine.register_fn("df_select", |cols_json: String| -> String {
        let cols: Vec<String> = match serde_json::from_str(&cols_json) {
            Ok(v) => v,
            Err(e) => return format!("err:cols_json解析失败: {e}"),
        };
        match get_lf() {
            Ok(lf) => {
                let exprs: Vec<Expr> = cols.iter().map(|c| col(c.as_str())).collect();
                set_lf(lf.select(exprs));
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    // df_drop(cols_json) — 删列，cols_json = ["C","D"]
    engine.register_fn("df_drop", |cols_json: String| -> String {
        let cols: Vec<String> = match serde_json::from_str(&cols_json) {
            Ok(v) => v,
            Err(e) => return format!("err:cols_json解析失败: {e}"),
        };
        match get_lf() {
            Ok(lf) => {
                set_lf(lf.drop(polars::prelude::Selector::ByName { names: cols.into_iter().map(polars::datatypes::PlSmallStr::from).collect(), strict: false }));
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    // df_rename(old_name, new_name) — 改单列名
    engine.register_fn("df_rename", |old: String, new: String| -> String {
        match get_lf() {
            Ok(lf) => {
                set_lf(lf.rename([old.as_str()], [new.as_str()], true));
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    // df_sort(cols_json, descending_json) — 多列排序
    // cols_json = ["A","B"]，descending_json = [false, true]
    engine.register_fn("df_sort", |cols_json: String, desc_json: String| -> String {
        let cols: Vec<String> = match serde_json::from_str(&cols_json) {
            Ok(v) => v,
            Err(e) => return format!("err:cols_json解析失败: {e}"),
        };
        let descs: Vec<bool> = match serde_json::from_str(&desc_json) {
            Ok(v) => v,
            Err(e) => return format!("err:descending_json解析失败: {e}"),
        };
        if cols.len() != descs.len() {
            return "err:cols和descending长度不一致".to_string();
        }
        match get_lf() {
            Ok(lf) => {
                let exprs: Vec<Expr> = cols.iter().map(|c| col(c.as_str())).collect();
                let opts = SortMultipleOptions::default()
                    .with_order_descending_multi(descs)
                    .with_nulls_last(true);
                set_lf(lf.sort_by_exprs(exprs, opts));
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    // df_head(n) — 取前 n 行
    engine.register_fn("df_head", |n: i64| -> String {
        match get_lf() {
            Ok(lf) => { set_lf(lf.limit(n as u32)); "ok".to_string() }
            Err(e) => e,
        }
    });

    // df_tail(n) — 取后 n 行（需 collect，结果重新 lazy）
    engine.register_fn("df_tail", |n: i64| -> String {
        match collect_lf() {
            Ok(df) => {
                let h = df.height();
                let start = h.saturating_sub(n as usize);
                match df.slice(start as i64, n as usize).lazy().collect() {
                    Ok(sliced) => { set_lf(sliced.lazy()); "ok".to_string() }
                    Err(e) => format!("err:{e}"),
                }
            }
            Err(e) => e,
        }
    });

    // df_dedup(cols_json) — 按列去重，cols_json=[] 表示全列
    engine.register_fn("df_dedup", |cols_json: String| -> String {
        let cols: Vec<String> = match serde_json::from_str(&cols_json) {
            Ok(v) => v,
            Err(e) => return format!("err:cols_json解析失败: {e}"),
        };
        match get_lf() {
            Ok(lf) => {
                let subset = if cols.is_empty() { None } else {
                    Some(polars::prelude::Selector::ByName { names: cols.into_iter().map(polars::datatypes::PlSmallStr::from).collect(), strict: false })
                };
                set_lf(lf.unique(subset, UniqueKeepStrategy::First));
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    // df_drop_nulls(cols_json) — 删除含 null 的行，cols_json=[] 表示任意列含 null 即删
    engine.register_fn("df_drop_nulls", |cols_json: String| -> String {
        let cols: Vec<String> = match serde_json::from_str(&cols_json) {
            Ok(v) => v,
            Err(e) => return format!("err:cols_json解析失败: {e}"),
        };
        match get_lf() {
            Ok(lf) => {
                let subset = if cols.is_empty() { None } else {
                    Some(polars::prelude::Selector::ByName { names: cols.into_iter().map(polars::datatypes::PlSmallStr::from).collect(), strict: false })
                };
                set_lf(lf.drop_nulls(subset));
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    // ── SQL 兜底 ──────────────────────────────────────────

    // df_sql(table_name, sql) -> JSON preview（只读，不替换 WORKING_DF）
    // 返回查询结果的 df_preview(50)
    engine.register_fn("df_sql", |table_name: String, sql: String| -> String {
        match collect_lf() {
            Ok(df) => {
                match roe_polars_bridge::run_sql_on_dataframe(df, &table_name, &sql) {
                    Ok(result) => match roe_polars_bridge::df_preview(&result, 50) {
                        Ok(s) => s,
                        Err(e) => format!("err:{e}"),
                    },
                    Err(e) => format!("err:{e}"),
                }
            }
            Err(e) => e,
        }
    });

    // df_sql_apply(table_name, sql) -> "ok" | "err:..."
    // 执行 SQL 并将结果替换 WORKING_DF
    engine.register_fn("df_sql_apply", |table_name: String, sql: String| -> String {
        match collect_lf() {
            Ok(df) => {
                match roe_polars_bridge::run_sql_on_dataframe(df, &table_name, &sql) {
                    Ok(result) => { set_lf(result.lazy()); "ok".to_string() }
                    Err(e) => format!("err:{e}"),
                }
            }
            Err(e) => e,
        }
    });


    // ── 会话级数据集 ──────────────────────────────────────
    // df_save_as(name) -> "ok" | "err:..."  把 WORKING_DF 物化后存入 DATASETS(跨脚本调用保留)
    engine.register_fn("df_save_as", |name: String| -> String {
        let name = name.trim().to_string();
        if name.is_empty() {
            return "err:数据集名称不能为空".to_string();
        }
        match collect_lf() {
            Ok(df) => {
                match with_ds(|d| { d.insert(name, Dataset::Mem(df)); }) {
                    Ok(()) => "ok".to_string(),
                    Err(e) => e,
                }
            }
            Err(e) => e,
        }
    });

    // df_use(name) -> "ok" | "err:..."  取出数据集作为 WORKING_DF
    engine.register_fn("df_use", |name: String| -> String {
        let got = match with_ds(|d| d.get(name.trim()).cloned()) {
            Ok(g) => g,
            Err(e) => return e,
        };
        match got {
            Some(ds) => { set_lf(ds.to_lazy()); "ok".to_string() }
            None => format!("err:数据集\"{}\"不存在", name),
        }
    });

    // df_list_datasets() -> JSON [{"name","rows","cols","bytes"}]
    engine.register_fn("df_list_datasets", || -> String {
        let items: Vec<serde_json::Value> = match with_ds(|d| {
            d
                .iter()
                .map(|(k, v)| match v {
                    Dataset::Mem(df) => serde_json::json!({
                        "name": k,
                        "rows": df.height(),
                        "cols": df.width(),
                        "bytes": df.estimated_size(),
                    }),
                    Dataset::Lazy(lf) => {
                        let cols = lf.clone().collect_schema().map(|s| s.len()).unwrap_or(0);
                        serde_json::json!({
                            "name": k,
                            "rows": serde_json::Value::Null,
                            "cols": cols,
                            "bytes": serde_json::Value::Null,
                        })
                    }
                })
                .collect::<Vec<_>>()
        }) {
            Ok(v) => v,
            Err(e) => return e,
        };
        serde_json::Value::Array(items).to_string()
    });

    // df_drop_dataset(name) -> "ok" | "err:..."
    engine.register_fn("df_drop_dataset", |name: String| -> String {
        let removed = match with_ds(|d| d.remove(name.trim())) {
            Ok(r) => r,
            Err(e) => return e,
        };
        match removed {
            Some(_) => "ok".to_string(),
            None => format!("err:数据集\"{}\"不存在", name),
        }
    });

    // df_sql_ds(sql) -> JSON preview(50 行) | "err:..."  全部数据集注册为表,可 JOIN;不改 WORKING_DF
    engine.register_fn("df_sql_ds", |sql: String| -> String {
        let tables = match datasets_snapshot() {
            Ok(t) => t,
            Err(e) => return e,
        };
        if tables.is_empty() {
            return "err:没有可用的数据集,请先 df_save_as".to_string();
        }
        match roe_polars_bridge::run_sql_on_lazy_datasets(tables, &sql) {
            Ok(result) => match roe_polars_bridge::df_preview(&result, 50) {
                Ok(s) => s,
                Err(e) => format!("err:{e}"),
            },
            Err(e) => format!("err:{e}"),
        }
    });

    // df_sql_ds_apply(sql) -> "ok" | "err:..."  结果替换 WORKING_DF
    engine.register_fn("df_sql_ds_apply", |sql: String| -> String {
        let tables = match datasets_snapshot() {
            Ok(t) => t,
            Err(e) => return e,
        };
        if tables.is_empty() {
            return "err:没有可用的数据集,请先 df_save_as".to_string();
        }
        match roe_polars_bridge::run_sql_on_lazy_datasets(tables, &sql) {
            Ok(result) => { set_lf(result.lazy()); "ok".to_string() }
            Err(e) => format!("err:{e}"),
        }
    });

    // df_sql_save(sql) -> JSON {"name","rows","cols":[..]} | "err:..."
    // 全部数据集注册为表(可 JOIN),结果物化后存为新数据集 df_N;不改 WORKING_DF。
    engine.register_fn("df_sql_save", |sql: String| -> String {
        let tables = match datasets_snapshot() {
            Ok(t) => t,
            Err(e) => return e,
        };
        if tables.is_empty() {
            return "err:没有可用的数据集".to_string();
        }
        match roe_polars_bridge::run_sql_on_lazy_datasets(tables, &sql) {
            Ok(df) => {
                let rows = df.height();
                let cols: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
                match with_ds(|d| {
                    let name = next_name(d, "sql");
                    d.insert(name.clone(), Dataset::Mem(df));
                    name
                }) {
                    Ok(name) => serde_json::json!({ "name": name, "rows": rows, "cols": cols }).to_string(),
                    Err(e) => e,
                }
            }
            Err(e) => format!("err:{e}"),
        }
    });

    // df_save_result() -> JSON {"name","rows","cols":[..]} | "err:..."
    // 把 WORKING_DF 物化后存为新数据集 chain_N(DF 链结果)。需与链代码在同一次脚本内调用。
    engine.register_fn("df_save_result", || -> String {
        let df = match collect_lf() {
            Ok(df) => df,
            Err(e) => return e,
        };
        let rows = df.height();
        let cols: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
        match with_ds(|d| {
            let name = next_name(d, "chain");
            d.insert(name.clone(), Dataset::Mem(df));
            name
        }) {
            Ok(name) => serde_json::json!({ "name": name, "rows": rows, "cols": cols }).to_string(),
            Err(e) => e,
        }
    });

    // ── 聚合 ──────────────────────────────────────────────

    // df_groupby_agg(group_cols_json, aggs_json) -> "ok" | "err:..."
    // group_cols_json = ["Region"]
    // aggs_json = [{"col":"Sales","fn":"sum","as":"total"},{"col":"Sales","fn":"count","as":"cnt"}]
    // 支持的 fn: sum/mean/min/max/count/first/last/std/var
    engine.register_fn("df_groupby_agg", |group_json: String, aggs_json: String| -> String {
        let group_cols: Vec<String> = match serde_json::from_str(&group_json) {
            Ok(v) => v,
            Err(e) => return format!("err:group_cols解析失败: {e}"),
        };
        let aggs_val: Vec<serde_json::Value> = match serde_json::from_str(&aggs_json) {
            Ok(v) => v,
            Err(e) => return format!("err:aggs_json解析失败: {e}"),
        };
        match get_lf() {
            Ok(lf) => {
                let by: Vec<Expr> = group_cols.iter().map(|c| col(c.as_str())).collect();
                let mut agg_exprs: Vec<Expr> = Vec::new();
                for agg in &aggs_val {
                    let c = match agg["col"].as_str() {
                        Some(s) => s,
                        None => return "err:agg缺少col字段".to_string(),
                    };
                    let f = match agg["fn"].as_str() {
                        Some(s) => s,
                        None => return "err:agg缺少fn字段".to_string(),
                    };
                    let alias = agg["as"].as_str().unwrap_or(c);
                    let expr = match f {
                        "sum"   => col(c).sum(),
                        "mean"  => col(c).mean(),
                        "min"   => col(c).min(),
                        "max"   => col(c).max(),
                        "count" => col(c).count(),
                        "first" => col(c).first(),
                        "last"  => col(c).last(),
                        "std"   => col(c).std(1),
                        "var"   => col(c).var(1),
                        other   => return format!("err:不支持的聚合函数: {other}"),
                    };
                    agg_exprs.push(expr.alias(alias));
                }
                set_lf(lf.group_by(by).agg(agg_exprs));
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    // ── 派生列（含 when-then-otherwise）────────────────────

    // df_with_column(new_col, expr_json) -> "ok" | "err:..."
    // expr_json 两种形式：
    // 四则运算: {"a":"colA","op":"add|sub|mul|div","b":"colB"} 或 {"a":"colA","op":"mul","val":100}
    // 条件分支: {"when":{"col":"status","op":"eq","val":"VIP"},"then":{"col":"price","op":"mul","val":0.8},"otherwise":{"col":"price"}}
    engine.register_fn("df_with_column", |new_col: String, expr_json: String| -> String {
        let v: serde_json::Value = match serde_json::from_str(&expr_json) {
            Ok(v) => v,
            Err(e) => return format!("err:expr_json解析失败: {e}"),
        };
        match get_lf() {
            Ok(lf) => {
                match build_expr(&v) {
                    Ok(expr) => {
                        set_lf(lf.with_column(expr.alias(new_col.as_str())));
                        "ok".to_string()
                    }
                    Err(e) => format!("err:{e}"),
                }
            }
            Err(e) => e,
        }
    });

    // ── Reshape ───────────────────────────────────────────

    // df_unpivot(id_cols_json, value_cols_json) -> "ok" | "err:..."
    // id_cols_json = ["Region"]（保持不动的列）
    // value_cols_json = ["Q1","Q2","Q3"]（要 melt 的列，空=全部非id列）
    engine.register_fn("df_unpivot", |id_json: String, val_json: String| -> String {
        let id_cols: Vec<String> = match serde_json::from_str(&id_json) {
            Ok(v) => v,
            Err(e) => return format!("err:id_cols解析失败: {e}"),
        };
        let val_cols: Vec<String> = match serde_json::from_str(&val_json) {
            Ok(v) => v,
            Err(e) => return format!("err:value_cols解析失败: {e}"),
        };
        match get_lf() {
            Ok(lf) => {
                use polars::prelude::{UnpivotArgsDSL, Selector};
                use polars::datatypes::PlSmallStr;
                let mk_sel = |cols: Vec<String>| Selector::ByName {
                    names: cols.into_iter().map(PlSmallStr::from).collect(),
                    strict: false,
                };
                let on = if val_cols.is_empty() {
                    None
                } else {
                    Some(mk_sel(val_cols))
                };
                let index = mk_sel(id_cols);
                let args = UnpivotArgsDSL {
                    on,
                    index,
                    variable_name: None,
                    value_name: None,
                };
                set_lf(lf.unpivot(args));
                "ok".to_string()
            }
            Err(e) => e,
        }
    });

    // ── 落盘 ──────────────────────────────────────────────

    // df_to_sheet(sheet_name) -> sheet_idx（i64）或 -1（失败，见 last error）
    // collect WORKING_DF，追加为新 sheet 到 workbook，返回新 sheet 索引
    engine.register_fn("df_to_sheet", |sheet_name: String| -> i64 {
        let df = match collect_lf() {
            Ok(df) => df,
            Err(_) => return -1,
        };
        match with_wb(|wb| {
            match roe_polars_bridge::dataframe_to_sheet(sheet_name.clone(), &df) {
                Ok(sheet) => { wb.sheets.push(sheet); Ok(wb.sheets.len() as i64 - 1) }
                Err(_) => Err(()),
            }
        }) {
            Ok(Ok(idx)) => idx,
            _ => -1,
        }
    });

    // df_to_sheet_head(name, n) -> sheet_idx | -1
    // 写路径:把数据集前 n 行(n 夹在 1~100000)写成新 sheet 作为预览;重名时加 _2、_3 后缀。
    engine.register_fn("df_to_sheet_head", |name: String, n: i64| -> i64 {
        let lf = match with_ds(|d| d.get(name.trim()).map(|x| x.to_lazy())) {
            Ok(Some(lf)) => lf,
            _ => return -1,
        };
        let df = match lf.slice(0, n.clamp(1, 100_000) as u32).collect() {
            Ok(df) => df,
            Err(_) => return -1,
        };
        let base = name.trim().to_string();
        match with_wb(|wb| {
            let mut sheet_name = base.clone();
            let mut k = 2;
            while wb.sheets.iter().any(|s| s.name == sheet_name) {
                sheet_name = format!("{base}_{k}");
                k += 1;
            }
            match roe_polars_bridge::dataframe_to_sheet(sheet_name, &df) {
                Ok(sheet) => { wb.sheets.push(sheet); Ok(wb.sheets.len() as i64 - 1) }
                Err(_) => Err(()),
            }
        }) {
            Ok(Ok(idx)) => idx,
            _ => -1,
        }
    });

    // df_pivot(config_json) -> "ok" | "err:..."
    // config_json 示例: {"rows":["A"],"cols":["B"],"values":["C"],"agg":"sum","filters":{"D":["v1"]}}
    engine.register_fn("df_pivot", |config_json: String| -> String {
        let cfg: roe_polars_bridge::PivotConfig = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(e) => return format!("err:config parse error: {e}"),
        };
        let df = match collect_lf() {
            Ok(df) => df,
            Err(e) => return format!("err:{e}"),
        };
        match roe_polars_bridge::run_pivot(&df, &cfg) {
            Ok(result) => { set_lf(result.clone().lazy()); "ok".to_string() }
            Err(e) => format!("err:{e}"),
        }
    });

    // df_pivot_to_sheet(config_json, sheet_name) -> sheet_idx（i64）或 -1
    engine.register_fn("df_pivot_to_sheet", |config_json: String, sheet_name: String| -> i64 {
        let cfg: roe_polars_bridge::PivotConfig = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(_) => return -1,
        };
        let df = match collect_lf() {
            Ok(df) => df,
            Err(_) => return -1,
        };
        match with_wb(|wb| {
            let temp_sheet_name = format!("__pivot_source_{}", sheet_name);
            let source_sheet = match roe_polars_bridge::dataframe_to_sheet(temp_sheet_name, &df) {
                Ok(s) => s,
                Err(_) => return Err(()),
            };
            wb.sheets.push(source_sheet);
            let source_idx = wb.sheets.len() - 1;
            match roe_polars_bridge::pivot_sheet_into_workbook_v2(wb, source_idx, &cfg, sheet_name.clone()) {
                Ok(idx) => Ok(idx as i64),
                Err(_) => Err(()),
            }
        }) {
            Ok(Ok(idx)) => idx,
            _ => -1,
        }
    });
}

// ════════════════════════════════════════════════════════════
// 测试
// ════════════════════════════════════════════════════════════


// ════════════════════════════════════════════════════════════
// build_expr：把结构化 JSON 转为 Polars Expr
// ════════════════════════════════════════════════════════════

fn build_simple_expr(v: &serde_json::Value) -> Result<Expr, String> {
    // {"col":"X"} 直接引用列
    if let Some(c) = v["col"].as_str() {
        let base = col(c);
        // 可选的四则运算：{"col":"X","op":"mul","val":2.0}
        if let Some(op) = v["op"].as_str() {
            let rhs = if let Some(b) = v["b"].as_str() {
                col(b)
            } else if let Some(n) = v["val"].as_f64() {
                polars::prelude::lit(n)
            } else {
                return Err("expr缺少b或val".into());
            };
            return Ok(match op {
                "add" => base + rhs,
                "sub" => base - rhs,
                "mul" => base * rhs,
                "div" => base / rhs,
                other => return Err(format!("不支持的运算符: {other}")),
            });
        }
        return Ok(base);
    }
    // {"val": 数字} 常量
    if let Some(n) = v["val"].as_f64() {
        return Ok(polars::prelude::lit(n));
    }
    Err("无法解析expr节点".into())
}

fn build_predicate(v: &serde_json::Value) -> Result<Expr, String> {
    let c = v["col"].as_str().ok_or("predicate缺少col")?;
    let op = v["op"].as_str().ok_or("predicate缺少op")?;
    let rhs = if let Some(b) = v["b"].as_str() {
        col(b)
    } else if let Some(s) = v["val"].as_str() {
        polars::prelude::lit(s)
    } else if let Some(n) = v["val"].as_f64() {
        polars::prelude::lit(n)
    } else if let Some(b) = v["val"].as_bool() {
        polars::prelude::lit(b)
    } else {
        return Err("predicate缺少val或b".into());
    };
    Ok(match op {
        "eq"  => col(c).eq(rhs),
        "neq" => col(c).neq(rhs),
        "gt"  => col(c).gt(rhs),
        "gte" => col(c).gt_eq(rhs),
        "lt"  => col(c).lt(rhs),
        "lte" => col(c).lt_eq(rhs),
        other => return Err(format!("不支持的谓词: {other}")),
    })
}

fn build_expr(v: &serde_json::Value) -> Result<Expr, String> {
    // when-then-otherwise 形式
    if v["when"].is_object() {
        let pred  = build_predicate(&v["when"])?;
        let then  = build_simple_expr(&v["then"])?;
        let other = build_simple_expr(&v["otherwise"])?;
        return Ok(polars::prelude::when(pred).then(then).otherwise(other));
    }
    // 普通四则运算形式
    build_simple_expr(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use roe_model::calc::{CellBlock, CellValue, Sheet, SparseColumn, SparseGrid, Workbook};
    use smallvec::smallvec as sv;
    use std::collections::HashMap;

    fn make_wb() -> Workbook {
        let mut grid = SparseGrid::default();
        // Region | Sales
        // East   | 100
        // West   | 200
        // East   | 150
        for (ci, vals) in [
            vec!["Region", "East", "West", "East"],
            vec!["Sales", "100", "200", "150"],
        ].iter().enumerate() {
            grid.cols.insert(ci as u32, SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: vals.iter().map(|s| {
                        if let Ok(n) = s.parse::<f64>() { CellValue::Number(n) }
                        else { CellValue::Text(s.to_string()) }
                    }).collect::<smallvec::SmallVec<_>>(),
                }],
            });
        }
        Workbook {
            sheets: vec![Sheet {
                col_style: vec![],
                tables: vec![],
                name: "data".into(), cells: grid,
                col_widths: vec![], row_heights: vec![],
                frozen_rows: 0, frozen_cols: 0, date1904: false,
                zoom_scale: None, show_grid_lines: None,
                col_hidden: vec![], row_hidden: vec![],
                col_outline_level: vec![], row_outline_level: vec![],
                cell_formats:     std::collections::HashMap::new(),
                cond_formats:     vec![],
                data_validations: vec![],
                auto_filter:      None,
                merged_cells:     vec![],
                pivot:            None,
                default_col_width: 0.0,
                default_row_height: 0.0,
                floating_images: vec![],
                floating_shapes: vec![],
                next_object_id: 0,
                floating_charts: vec![],
                floating_stat_charts: vec![],
                sparkline_groups: vec![],
            }],
            named_ranges: HashMap::new(),
            named_formulas: HashMap::new(),
            styles: roe_model::style::StyleRegistry::default(),
            formula_dialect_source: Default::default(),
        }
    }

    fn make_engine() -> rhai::Engine {
        let mut e = rhai::Engine::new();
        register(&mut e);
        e
    }

    /// df_join专用：构造一个含2个sheet的Workbook。
    /// sheet 0（"data"）: Region | Sales -> East/100, West/200, East/150
    /// sheet 1（"lookup"）: RegionName | Manager -> East/Alice, West/Bob
    /// 故意让两张表的关联列名不同（Region vs RegionName），验证
    /// left_on != right_on的场景（gap analysis 2.9提到的真实业务场景：
    /// 两张表关联列名往往不同，比如"客户ID" vs "CustomerID"）。
    fn make_wb_two_sheets() -> Workbook {
        let mut wb = make_wb();

        let mut grid2 = SparseGrid::default();
        for (ci, vals) in [
            vec!["RegionName", "East", "West"],
            vec!["Manager", "Alice", "Bob"],
        ].iter().enumerate() {
            grid2.cols.insert(ci as u32, SparseColumn {
                blocks: sv![CellBlock {
                    start_row: 0,
                    values: vals.iter().map(|s| CellValue::Text(s.to_string())).collect::<smallvec::SmallVec<_>>(),
                }],
            });
        }
        wb.sheets.push(Sheet {
            col_style: vec![],
            tables: vec![],
            name: "lookup".into(), cells: grid2,
            col_widths: vec![], row_heights: vec![],
            frozen_rows: 0, frozen_cols: 0, date1904: false,
            zoom_scale: None, show_grid_lines: None,
            col_hidden: vec![], row_hidden: vec![],
            col_outline_level: vec![], row_outline_level: vec![],
            cell_formats:     std::collections::HashMap::new(),
            cond_formats:     vec![],
            data_validations: vec![],
            auto_filter:      None,
            merged_cells:     vec![],
            pivot:            None,
            default_col_width: 0.0,
            default_row_height: 0.0,
            floating_images: vec![],
            floating_shapes: vec![],
            next_object_id: 0,
            floating_charts: vec![],
            floating_stat_charts: vec![],
            sparkline_groups: vec![],
        });
        wb
    }

    #[test]
    fn test_df_join_inner_basic() {
        // sheet0(data): Region/Sales 3行(East,West,East)
        // sheet1(lookup): RegionName/Manager 2行(East,West)
        // inner join后应仍是3行（每行Region都能在lookup里找到对应RegionName），
        // 新增Manager列。
        let mut wb = make_wb_two_sheets();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let r: String = e.eval(r#"df_join(1, "Region", "RegionName", "inner")"#).unwrap();
            assert_eq!(r, "ok", "join应成功，实际: {r}");

            let info: String = e.eval("df_info()").unwrap();
            let v: serde_json::Value = serde_json::from_str(&info).unwrap();
            // df_info返回的columns数组元素是{"name":"...","type":"..."}对象
            // （见lazy_schema_info实现），不是纯字符串，取name字段比较。
            let cols: Vec<String> = v["columns"].as_array().unwrap()
                .iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
            assert!(cols.contains(&"Manager".to_string()), "join后应包含Manager列，实际列: {cols:?}");

            let preview: String = e.eval("df_preview(10)").unwrap();
            let pv: serde_json::Value = serde_json::from_str(&preview).unwrap();
            assert_eq!(pv["total_rows"], 3, "inner join后行数应为3（全部匹配），实际: {pv}");
        });
    }

    #[test]
    fn test_df_join_left_keeps_unmatched_rows() {
        // 验证left join的语义：即使右表没有匹配项，左表的行也应保留
        // （Manager列在未匹配行上应为null）。用一个lookup里缺失West的场景
        // 制造真正的"未匹配"情况，与inner join形成对照。
        let mut wb = make_wb_two_sheets();
        // 手动删掉lookup sheet里的West那一行，制造真正的未匹配场景
        {
            let lookup = &mut wb.sheets[1];
            // 重建只含East的lookup数据（RegionName/Manager各只有1个数据行）
            let mut grid2 = SparseGrid::default();
            for (ci, vals) in [
                vec!["RegionName", "East"],
                vec!["Manager", "Alice"],
            ].iter().enumerate() {
                grid2.cols.insert(ci as u32, SparseColumn {
                    blocks: sv![CellBlock {
                        start_row: 0,
                        values: vals.iter().map(|s| CellValue::Text(s.to_string())).collect::<smallvec::SmallVec<_>>(),
                    }],
                });
            }
            lookup.cells = grid2;
        }
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let r: String = e.eval(r#"df_join(1, "Region", "RegionName", "left")"#).unwrap();
            assert_eq!(r, "ok");

            let preview: String = e.eval("df_preview(10)").unwrap();
            let pv: serde_json::Value = serde_json::from_str(&preview).unwrap();
            // left join：左表3行（East/West/East）全部保留，即使West在lookup里已不存在
            assert_eq!(pv["total_rows"], 3, "left join应保留左表全部3行，实际: {pv}");
        });
    }

    #[test]
    fn test_df_join_invalid_how_returns_err() {
        // 不支持的how类型（如"outer"这种非标准命名，或纯打错）应返回明确的
        // err:前缀字符串，而不是panic或静默使用默认值。
        let mut wb = make_wb_two_sheets();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let r: String = e.eval(r#"df_join(1, "Region", "RegionName", "outer")"#).unwrap();
            assert!(r.starts_with("err:"), "不支持的join类型应返回err:前缀，实际: {r}");
        });
    }

    #[test]
    fn test_df_join_invalid_sheet_idx_returns_err() {
        // 引用不存在的sheet_idx应返回err，不panic。
        let mut wb = make_wb_two_sheets();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let r: String = e.eval(r#"df_join(99, "Region", "RegionName", "inner")"#).unwrap();
            assert!(r.starts_with("err:"), "越界sheet_idx应返回err，实际: {r}");
        });
    }

    #[test]
    fn test_load_and_preview() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            let r: String = e.eval("df_load_sheet(0)").unwrap();
            assert_eq!(r, "ok");
            let preview: String = e.eval("df_preview(10)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&preview).unwrap();
            assert_eq!(v["total_rows"], 3);
        });
    }

    #[test]
    fn test_df_info_no_collect() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let info: String = e.eval("df_info()").unwrap();
            let v: serde_json::Value = serde_json::from_str(&info).unwrap();
            let cols = v["columns"].as_array().unwrap();
            assert_eq!(cols.len(), 2);
        });
    }

    #[test]
    fn test_select_and_head() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            e.eval::<String>(r#"df_select("[\"Sales\"]")"#).unwrap();
            e.eval::<String>("df_head(2)").unwrap();
            let preview: String = e.eval("df_preview(10)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&preview).unwrap();
            assert_eq!(v["columns"], serde_json::json!(["Sales"]));
            assert_eq!(v["total_rows"], 2);
        });
    }

    #[test]
    fn test_sql_apply_and_to_sheet() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let r: String = e.eval(r#"df_sql_apply("t", "SELECT Region, SUM(Sales) AS total FROM t GROUP BY Region ORDER BY Region")"#).unwrap();
            assert_eq!(r, "ok");
            let idx: i64 = e.eval(r#"df_to_sheet("Result")"#).unwrap();
            assert_eq!(idx, 1);
        });
        assert_eq!(wb.sheets.len(), 2);
        assert_eq!(wb.sheets[1].name, "Result");
    }

    #[test]
    fn test_error_no_working_df() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            // 未 load，直接 preview 应返回 err: 开头
            let r: String = e.eval("df_preview(5)").unwrap();
            assert!(r.starts_with("err:"), "期望错误但得到: {r}");
        });
    }
    #[test]
    fn test_groupby_agg() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let r: String = e.eval(r#"df_groupby_agg("[\"Region\"]", "[{\"col\":\"Sales\",\"fn\":\"sum\",\"as\":\"total\"}]")"#).unwrap();
            assert_eq!(r, "ok");
            let preview: String = e.eval("df_preview(10)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&preview).unwrap();
            assert_eq!(v["total_rows"], 2);
        });
    }

    #[test]
    fn test_with_column_arithmetic() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let r: String = e.eval(r#"df_with_column("Sales2x", "{\"col\":\"Sales\",\"op\":\"mul\",\"val\":2.0}")"#).unwrap();
            assert_eq!(r, "ok");
            let preview: String = e.eval("df_preview(10)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&preview).unwrap();
            let cols = v["columns"].as_array().unwrap();
            assert!(cols.iter().any(|c| c.as_str() == Some("Sales2x")));
        });
    }

    #[test]
    fn test_unpivot() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let r: String = e.eval(r#"df_unpivot("[\"Region\"]", "[\"Sales\"]")"#).unwrap();
            assert_eq!(r, "ok");
            let preview: String = e.eval("df_preview(10)").unwrap();
            let v: serde_json::Value = serde_json::from_str(&preview).unwrap();
            let cols = v["columns"].as_array().unwrap();
            assert!(cols.iter().any(|c| c.as_str() == Some("variable")));
        });
    }


    // ── 新函数测试 ────────────────────────────────────────


    #[test]
    fn test_pivot_to_sheet_returns_valid_idx() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let cfg = r#"{"rows":["Region"],"cols":[],"values":["Sales"],"agg":"sum","filters":{}}"#;
            let script = format!(r#"df_pivot_to_sheet(`{}`, "pivot1")"#, cfg);
            let idx: i64 = e.eval(&script).unwrap();
            assert!(idx >= 0, "df_pivot_to_sheet should return a valid sheet index, got {}", idx);
        });
        assert_eq!(wb.sheets.len(), 3, "a new sheet (plus internal temp source sheet) should have been pushed");
        assert_eq!(wb.sheets[2].name, "pivot1");
    }

    #[test]
    fn test_pivot_to_sheet_throw_pattern_for_frontend() {
        let mut wb = make_wb();
        let e = make_engine();
        with_workbook_df(&mut wb, || {
            e.eval::<String>("df_load_sheet(0)").unwrap();
            let cfg = r#"{"rows":["NoSuchColumn"],"cols":[],"values":["Sales"],"agg":"sum","filters":{}}"#;
            let script = format!(
                r#"let idx = df_pivot_to_sheet(`{}`, "pivot2"); if idx < 0 {{ throw "pivot failed"; }} idx"#,
                cfg
            );
            let result: Result<i64, _> = e.eval(&script);
            assert!(result.is_err(), "throw pattern should surface the failure as Err");
        });
    }
}

#[cfg(test)]
mod datasets_tests {
    use super::*;

    thread_local! {
        static TEST_DS: RefCell<DatasetStore> = RefCell::new(DatasetStore::new());
    }

    /// 清空并(重新)绑定测试用数据集容器,替代旧的全局 clear_datasets()。
    fn clear_datasets() {
        TEST_DS.with(|s| {
            s.borrow_mut().clear();
            bind_datasets(unsafe { &mut *s.as_ptr() });
        });
    }

    fn mk(tag: &str, content: &str) -> DataFrame {
        let p = std::env::temp_dir().join(format!("roe_dstest_{}_{}.csv", std::process::id(), tag));
        std::fs::write(&p, content).unwrap();
        let path = p.to_string_lossy().to_string();
        let df = roe_polars_bridge::df_from_csv_file(&path, None, true).unwrap();
        let _ = std::fs::remove_file(&p);
        df
    }

    fn eng() -> Engine {
        let mut e = Engine::new();
        register(&mut e);
        e
    }

    #[test]
    fn csv_cached_load_info_preview_and_rebuild() {
        let dir = std::env::temp_dir().join(format!("roe_cachedir_script_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        set_csv_cache_dir(Some(dir.clone()));
        let src = std::env::temp_dir().join(format!("roe_cs_{}.csv", std::process::id()));
        std::fs::write(&src, "a,b\n1,2\n3,4\n").unwrap();
        let e = eng();
        let script = format!(r#"df_load_csv_cached("{}", "", true)"#, src.to_string_lossy().replace('\\', "/"));
        assert_eq!(e.eval::<String>(&script).unwrap(), "ok");
        assert!(e.eval::<String>("df_info()").unwrap().contains("\"a\""));
        assert_eq!(collect_lf().unwrap().height(), 2);
        let n_files = || std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(n_files(), 1);
        // 再载入同一文件:命中缓存,不新增文件
        assert_eq!(e.eval::<String>(&script).unwrap(), "ok");
        assert_eq!(n_files(), 1);
        // 改文件后重建
        std::fs::write(&src, "a,b\n1,2\n3,4\n5,6\n").unwrap();
        assert_eq!(e.eval::<String>(&script).unwrap(), "ok");
        assert_eq!(collect_lf().unwrap().height(), 3);
        assert_eq!(n_files(), 2);
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_dir_all(&dir);
        set_csv_cache_dir(None);
        unbind_workbook_df();
    }

    #[test]
    fn parquet_load_hive_warehouse_t_pq() {
        let home = match std::env::var("HOME") { Ok(h) => h, Err(_) => return };
        let p = format!("{home}/smalldata-lab/hive-warehouse/polars_test.db/t_pq/000000_0");
        if !std::path::Path::new(&p).exists() { return; } // 非本机环境跳过
        let e = eng();
        let script = format!(r#"df_load_parquet("{}")"#, p);
        assert_eq!(e.eval::<String>(&script).unwrap(), "ok");
        let info = e.eval::<String>("df_info()").unwrap();
        for c in ["id", "name", "amount"] { assert!(info.contains(c), "{info}"); }
        let df = collect_lf().unwrap();
        assert_eq!(df.height(), 3);
        println!("{info}\n{df}");
        let bad = e.eval::<String>(r#"df_load_parquet("/nonexistent/x.parquet")"#).unwrap();
        assert!(bad.starts_with("err:"), "{bad}");
        unbind_workbook_df();
    }

    #[test]
    fn parquet_dir_hive_location_and_multi_file() {
        let home = match std::env::var("HOME") { Ok(h) => h, Err(_) => return };
        let dir = format!("{home}/smalldata-lab/hive-warehouse/polars_test.db/t_pq");
        let f = format!("{dir}/000000_0");
        if !std::path::Path::new(&f).exists() { return; }
        let e = eng();
        // 1) Hive LOCATION 原样(file: 前缀 + 目录,目录里有 .crc)
        let s1 = format!(r#"df_load_parquet_dir("file:{}")"#, dir);
        assert_eq!(e.eval::<String>(&s1).unwrap(), "ok");
        assert_eq!(collect_lf().unwrap().height(), 3);
        // 2) 临时目录:两份数据 + .crc + _SUCCESS => 6 行
        let tmp = std::env::temp_dir().join(format!("roe_pqdir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::copy(&f, tmp.join("000000_0")).unwrap();
        std::fs::copy(&f, tmp.join("000001_0")).unwrap();
        std::fs::write(tmp.join(".000000_0.crc"), b"junk").unwrap();
        std::fs::write(tmp.join("_SUCCESS"), b"").unwrap();
        let s2 = format!(r#"df_load_parquet_dir("{}")"#, tmp.to_string_lossy());
        assert_eq!(e.eval::<String>(&s2).unwrap(), "ok");
        assert_eq!(collect_lf().unwrap().height(), 6);
        // 3) 错误路径
        let bad = e.eval::<String>(r#"df_load_parquet_dir("hdfs://nn/x")"#).unwrap();
        assert!(bad.starts_with("err:"), "{bad}");
        let bad2 = e.eval::<String>(r#"df_load_parquet_dir("/nonexistent_dir_xyz")"#).unwrap();
        assert!(bad2.starts_with("err:"), "{bad2}");
        let _ = std::fs::remove_dir_all(&tmp);
        unbind_workbook_df();
    }

    #[test]
    fn dataset_survives_unbind_and_df_use_restores() {
        clear_datasets();
        let e = eng();
        set_lf(mk("a", "a,b\n1,2\n3,4\n5,6\n").lazy());
        assert_eq!(e.eval::<String>(r#"df_save_as("t1")"#).unwrap(), "ok");
        unbind_workbook_df(); // 模拟脚本结束:WORKING_DF 被清
        assert!(get_lf().is_err());
        assert_eq!(e.eval::<String>(r#"df_use("t1")"#).unwrap(), "ok");
        assert_eq!(collect_lf().unwrap().height(), 3);
        clear_datasets();
    }

    #[test]
    fn dataset_list_drop_and_missing() {
        clear_datasets();
        let e = eng();
        set_lf(mk("b", "x\n1\n2\n").lazy());
        e.eval::<String>(r#"df_save_as("t2")"#).unwrap();
        let list = e.eval::<String>("df_list_datasets()").unwrap();
        assert!(list.contains("\"t2\"") && list.contains("\"rows\":2"), "{list}");
        assert_eq!(e.eval::<String>(r#"df_drop_dataset("t2")"#).unwrap(), "ok");
        assert!(e.eval::<String>(r#"df_use("t2")"#).unwrap().starts_with("err:"));
        assert!(e.eval::<String>(r#"df_save_as("  ")"#).unwrap().starts_with("err:"));
    }

    #[test]
    fn lazy_dataset_use_list_and_sql() {
        clear_datasets();
        let e = eng();
        TEST_DS.with(|s| {
            s.borrow_mut().insert("lz".to_string(), Dataset::Lazy(mk("l", "a\n1\n2\n3\n").lazy()));
        });
        assert_eq!(e.eval::<String>(r#"df_use("lz")"#).unwrap(), "ok");
        assert_eq!(collect_lf().unwrap().height(), 3);
        let list = e.eval::<String>("df_list_datasets()").unwrap();
        assert!(list.contains("\"rows\":null") && list.contains("\"cols\":1"), "{list}");
        let p = e.eval::<String>(r#"df_sql_ds("SELECT COUNT(*) AS n FROM lz")"#).unwrap();
        assert!(p.contains("\"total_rows\":1"), "{p}");
        clear_datasets();
    }

    #[test]
    fn overview_counts_nulls_and_head() {
        clear_datasets();
        let e = eng();
        set_lf(mk("ov", "a,b\n1,1.5\n2,\n3,2.5\n").lazy());
        assert_eq!(e.eval::<String>(r#"df_save_as("ov")"#).unwrap(), "ok");
        let s = e.eval::<String>(r#"df_dataset_overview("ov", 2)"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).expect(&s);
        assert_eq!(v["rows"], 3, "{s}");
        assert_eq!(v["cols"], 2, "{s}");
        assert_eq!(v["columns"][1]["nulls"], 1, "{s}");
        assert_eq!(v["columns"][0]["nulls"], 0, "{s}");
        assert_eq!(v["head"]["rows"].as_array().unwrap().len(), 2, "{s}");
        println!("overview => {s}");
        assert!(e.eval::<String>(r#"df_dataset_overview("nope", 5)"#).unwrap().starts_with("err:"));
        clear_datasets();
    }

    #[test]
    fn sql_join_across_two_datasets() {
        clear_datasets();
        let e = eng();
        set_lf(mk("o", "id,amt\n1,10\n2,20\n3,30\n").lazy());
        e.eval::<String>(r#"df_save_as("orders")"#).unwrap();
        set_lf(mk("c", "id,name\n1,alice\n2,bob\n").lazy());
        e.eval::<String>(r#"df_save_as("cust")"#).unwrap();
        let r = e
            .eval::<String>(r#"df_sql_ds_apply("SELECT orders.id, cust.name, orders.amt FROM orders JOIN cust ON orders.id = cust.id")"#)
            .unwrap();
        assert_eq!(r, "ok", "{r}");
        assert_eq!(collect_lf().unwrap().height(), 2);
        let p = e.eval::<String>(r#"df_sql_ds("SELECT COUNT(*) AS n FROM orders")"#).unwrap();
        assert!(p.contains("\"total_rows\":1"), "{p}");
        clear_datasets();
        assert!(e.eval::<String>(r#"df_sql_ds("SELECT 1")"#).unwrap().starts_with("err:"));
    }

    fn jv(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap_or_else(|er| panic!("非 JSON: {s} ({er})"))
    }

    #[test]
    fn sql_save_single_table_and_name_increments() {
        clear_datasets();
        let e = eng();
        set_lf(mk("s1", "id,amt\n1,10\n2,20\n3,30\n").lazy());
        e.eval::<String>(r#"df_save_as("orders")"#).unwrap();
        let r = e
            .eval::<String>(r#"df_sql_save("SELECT id FROM orders WHERE amt > 10")"#)
            .unwrap();
        let v = jv(&r);
        assert_eq!(v["name"], "sql_1");
        assert_eq!(v["rows"], 2);
        assert_eq!(v["cols"], serde_json::json!(["id"]));
        let r2 = e.eval::<String>(r#"df_sql_save("SELECT * FROM sql_1")"#).unwrap();
        assert_eq!(jv(&r2)["name"], "sql_2");
        let list = jv(&e.eval::<String>("df_list_datasets()").unwrap());
        let names: Vec<&str> = list.as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"sql_1") && names.contains(&"sql_2") && names.contains(&"orders"), "{names:?}");
        clear_datasets();
    }

    #[test]
    fn sql_save_join_result() {
        clear_datasets();
        let e = eng();
        set_lf(mk("j1", "id,amt\n1,10\n2,20\n3,30\n").lazy());
        e.eval::<String>(r#"df_save_as("orders")"#).unwrap();
        set_lf(mk("j2", "id,name\n1,alice\n2,bob\n").lazy());
        e.eval::<String>(r#"df_save_as("cust")"#).unwrap();
        let r = e
            .eval::<String>(r#"df_sql_save("SELECT orders.id, cust.name, orders.amt FROM orders JOIN cust ON orders.id = cust.id")"#)
            .unwrap();
        let v = jv(&r);
        assert_eq!(v["rows"], 2, "{r}");
        assert_eq!(v["cols"].as_array().unwrap().len(), 3, "{r}");
        clear_datasets();
    }

    #[test]
    fn sql_save_errors_do_not_register() {
        clear_datasets();
        let e = eng();
        // 空容器
        let r = e.eval::<String>(r#"df_sql_save("SELECT 1")"#).unwrap();
        assert!(r.starts_with("err:"), "{r}");
        // 坏 SQL:报错且不新增数据集
        set_lf(mk("e1", "id\n1\n2\n").lazy());
        e.eval::<String>(r#"df_save_as("t")"#).unwrap();
        let r = e.eval::<String>(r#"df_sql_save("SELEKT nonsense FROM t")"#).unwrap();
        assert!(r.starts_with("err:"), "{r}");
        let list = jv(&e.eval::<String>("df_list_datasets()").unwrap());
        assert_eq!(list.as_array().unwrap().len(), 1);
        clear_datasets();
    }

    #[test]
    fn sql_save_readonly_stub() {
        clear_datasets();
        let e = eng();
        set_lf(mk("r1", "id\n1\n").lazy());
        e.eval::<String>(r#"df_save_as("t")"#).unwrap();
        let mut ro = Engine::new();
        register_readonly(&mut ro);
        let r = ro.eval::<String>(r#"df_sql_save("SELECT * FROM t")"#).unwrap();
        assert!(r.starts_with("err:"), "{r}");
        let list = jv(&e.eval::<String>("df_list_datasets()").unwrap());
        assert_eq!(list.as_array().unwrap().len(), 1, "只读桩不应新增数据集");
        clear_datasets();
    }

    #[test]
    fn chain_ok_preview_and_save() {
        clear_datasets();
        let e = eng();
        set_lf(mk("c1", "r,v\na,1\na,2\nb,3\n").lazy());
        e.eval::<String>(r#"df_save_as("t")"#).unwrap();
        let chain = "df_use(\"t\");\ndf_groupby_agg(`[\"r\"]`, `[{\"col\":\"v\",\"fn\":\"sum\",\"as\":\"s\"}]`);\n";
        let pv = e.eval::<String>(&(wrap_chain(chain) + "df_preview(50)")).unwrap();
        assert!(pv.contains("\"total_rows\":2"), "{pv}");
        let r = e.eval::<String>(&(wrap_chain(chain) + "df_save_result()")).unwrap();
        let v = jv(&r);
        assert_eq!(v["name"], "chain_1", "{r}");
        assert_eq!(v["rows"], 2);
        assert_eq!(v["cols"], serde_json::json!(["r", "s"]));
        clear_datasets();
    }

    #[test]
    fn chain_error_stops_with_line_number() {
        clear_datasets();
        let e = eng();
        set_lf(mk("c2", "r,v\na,1\n").lazy());
        e.eval::<String>(r#"df_save_as("t")"#).unwrap();
        let chain = "df_use(\"t\");\ndf_use(\"missing\");\ndf_head(1);\n";
        let r = e.eval::<String>(&(wrap_chain(chain) + "df_save_result()"));
        let msg = format!("{:?}", r);
        assert!(r.is_err(), "应报错: {msg}");
        assert!(msg.contains("第2行"), "{msg}");
        let list = jv(&e.eval::<String>("df_list_datasets()").unwrap());
        assert_eq!(list.as_array().unwrap().len(), 1, "出错不应新增数据集");
        clear_datasets();
    }

    #[test]
    fn chain_save_readonly_stub() {
        clear_datasets();
        let e = eng();
        set_lf(mk("c3", "r\n1\n").lazy());
        e.eval::<String>(r#"df_save_as("t")"#).unwrap();
        let mut ro = Engine::new();
        register_readonly(&mut ro);
        let r = ro.eval::<String>(r#"df_use("t"); df_save_result()"#).unwrap();
        assert!(r.starts_with("err:"), "{r}");
        let list = jv(&e.eval::<String>("df_list_datasets()").unwrap());
        assert_eq!(list.as_array().unwrap().len(), 1);
        clear_datasets();
    }
}

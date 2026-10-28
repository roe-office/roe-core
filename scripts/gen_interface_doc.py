#!/usr/bin/env python3
"""从源码提取 ROE 接口清单,生成 roe_interface_inventory.md。
用法: python3 scripts/gen_interface_doc.py [输出路径]
以源码为准;接口变更后重跑即可。纯标准库,只读源码。
"""
import re, sys, datetime, pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "roe_interface_inventory.md"

def read(p):
    return (ROOT / p).read_text(encoding="utf-8", errors="replace")

def clean_sig(s):
    return re.sub(r"\s+", " ", s).strip().rstrip(", ")

# ── Calc 宏: fn "name" => |args| -> ret { ... }, true/false;
def parse_calc(path):
    src = read(path)
    items = []
    for m in re.finditer(r'fn\s+"([^"]+)"\s*=>\s*(\|[^|]*\|)\s*(?:->\s*([^{]+?))?\s*\{', src):
        name, args, ret = m.group(1), m.group(2), (m.group(3) or "()").strip()
        # 向后找该条目的 readonly 标记:第一个 "}, true;" 或 "}, false;"
        tail = src[m.end():]
        t = re.search(r'\}\s*,\s*(true|false)\s*;', tail)
        ro = t.group(1) if t else "?"
        items.append((name, clean_sig(args.strip("|")), clean_sig(ret), "只读" if ro == "true" else ("可写" if ro == "false" else "?")))
    return items

# ── engine.register_fn("name", |args| -> ret {
def parse_register_fn(path, src=None):
    if src is None:
        src = read(path)
    items = []
    for m in re.finditer(r'register_fn\(\s*"([^"]+)"\s*,\s*(?:move\s*)?(\|[^|]*\|)\s*(?:->\s*([^{]+?))?\s*\{', src):
        items.append((m.group(1), clean_sig(m.group(2).strip("|")), clean_sig(m.group(3) or "()")))
    return items

def parse_df(path):
    """DF 段:register_readonly 里的同名桩不单列,在真实函数上标注只读环境不可用。"""
    src = read(path)
    a = src.index("pub fn register_readonly")
    b = src.index("pub fn register(")
    assert a < b, "register_readonly 应在 register 之前"
    stubs = {n for n, _, _ in parse_register_fn(path, src[a:b])}
    real = parse_register_fn(path, src[:a] + src[b:])
    names = {n for n, _, _ in real}
    missing = stubs - names
    if missing:
        print("警告: 桩无对应真实函数:", sorted(missing))
    return [(n, sg, r, "不可用(桩)" if n in stubs else "可用") for n, sg, r in real]

def _balanced_args(src, open_idx):
    """从 open_idx(指向 '(')开始,返回配平括号内的参数文本和结束位置。"""
    depth, i = 0, open_idx
    while i < len(src):
        c = src[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return src[open_idx + 1:i], i
        i += 1
    return "", open_idx

def parse_cabi():
    out = []
    for p in sorted((ROOT / "roe-api/src").glob("*.rs")):
        src = p.read_text(encoding="utf-8", errors="replace")
        for m in re.finditer(r'pub\s+extern\s+"C"\s+fn\s+(roe_\w+)\s*\(', src):
            args, end = _balanced_args(src, m.end() - 1)
            rest = src[end + 1:end + 200]
            rm = re.match(r'\s*(?:->\s*([^{]+?))?\s*\{', rest)
            ret = clean_sig(rm.group(1)) if rm and rm.group(1) else "()"
            out.append((m.group(1), clean_sig(args), ret, p.name))
    return sorted(out)

def table(rows, headers):
    lines = ["| " + " | ".join(headers) + " |", "|" + "|".join(["---"] * len(headers)) + "|"]
    for r in rows:
        lines.append("| " + " | ".join(str(x).replace("|", "\\|") for x in r) + " |")
    return "\n".join(lines)

def main():
    today = datetime.date.today().isoformat()
    calc = parse_calc("roe-script/src/calc_api.rs")
    writer = parse_register_fn("roe-script/src/writer_api.rs") + parse_register_fn("roe-script/src/writer_api_ext.rs")
    impress = parse_register_fn("roe-script/src/impress_api.rs") + parse_register_fn("roe-script/src/impress_api_ext.rs")
    for _kind, _items in (("Writer", writer), ("Impress", impress)):
        _names = [n for n, _, _ in _items]
        assert len(_names) == len(set(_names)), f"{_kind} 清单里有重名函数"
    df = parse_df("roe-script/src/calc_df_api.rs")
    eng = parse_register_fn("roe-script/src/engine.rs")
    cabi = parse_cabi()

    L = []
    L.append(f"# ROE 现有接口清单\n\n> 自动生成于 {today},脚本: `scripts/gen_interface_doc.py`。**以源码为准**,接口变更后重跑。\n")
    L.append("## 关键约定(内容维护在 gen_interface_doc.py 中,整份文档每次重跑都会重新生成,请勿手改此 md)\n")
    L.append("- **`save_docx()` / `save_xlsx()` / `save_pptx()` 返回 base64 字符串,不落盘**;失败返回空串。落盘必须由调用方(Tool 层)base64 解码后写文件。")
    L.append("- **DataFrame 系列(`df_*`)出错返回以 `err:` 开头的字符串,不抛异常**,调用方需检查前缀。")
    L.append("- **只读引擎(`roe_query_script`、AI agent 只读路径)里,DF 表标「不可用(桩)」的函数被同名桩覆盖**:文件加载/写数据集/写工作簿类只返回 `err:只读环境不可用...`(sheet 类返回 -1)。文件由前台经 C-ABI `roe_df_open_parquet` 加载为命名数据集,脚本再用 `df_use` / `df_page` 操作。")
    L.append("- Calc 函数通过宏 `register_calc_apis!` 注册,带只读标记:只读环境(Query 工具)只能调用标记为「只读」的函数。")
    L.append("- 写出范围有限:`save_docx` 不含表格/图片/页眉页脚;`save_xlsx` 不含列宽/行高/冻结窗格。")
    L.append('''- **两条保存路径签名不同**:Rhai `save_*` 返回 base64;C-ABI `roe_save_document_to_*_bytes` 通过回调 `extern "C" fn(*const u8, usize, *mut c_void)` 交出字节。roe-agent 是同进程 Rust 消费者,不应走 C-ABI 回调版,应走 Rhai `save_*` 或直接调 `roe_parser::ooxml::*_writer::write`。''')
    L.append("- 新建空白文档(Rust 内部): `roe_api::document::new_document_impl(doc_type: i32) -> u64`,1=Writer 2=Calc 3=Impress,返回 0 表示非法。\n")
    L.append(f"## 一、Calc Rhai 接口({len(calc)} 个)\n来源: `roe-script/src/calc_api.rs`\n")
    L.append(table(calc, ["函数", "参数", "返回", "读写"]) + "\n")
    L.append(f"## 二、Writer Rhai 接口({len(writer)} 个)\n来源: `roe-script/src/writer_api.rs` + `writer_api_ext.rs`\n")
    L.append(table(writer, ["函数", "参数", "返回"]) + "\n")
    L.append(f"## 三、Impress Rhai 接口({len(impress)} 个)\n来源: `roe-script/src/impress_api.rs` + `impress_api_ext.rs`\n")
    L.append(table(impress, ["函数", "参数", "返回"]) + "\n")
    L.append(f"## 四、DataFrame Rhai 接口({len(df)} 个)\n来源: `roe-script/src/calc_df_api.rs`\n")
    L.append(table(df, ["函数", "参数", "返回", "只读环境"]) + "\n")
    L.append(f"## 五、引擎公共 Rhai 接口({len(eng)} 个;engine.rs 本身不注册函数,命中均为注释/测试,此节为空属正常)\n来源: `roe-script/src/engine.rs`\n")
    L.append(table(eng, ["函数", "参数", "返回"]) + "\n")
    L.append(f"## 六、C-ABI 导出接口({len(cabi)} 个)\n来源: `roe-api/src/*.rs`\n")
    L.append(table(cabi, ["函数", "参数", "返回", "文件"]) + "\n")
    OUT.write_text("\n".join(L), encoding="utf-8")
    print(f"已生成 {OUT}")
    print(f"Calc={len(calc)} Writer={len(writer)} Impress={len(impress)} DF={len(df)} Engine={len(eng)} C-ABI={len(cabi)}")

main()

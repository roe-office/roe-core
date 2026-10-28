import glob, os, re, sys
d = os.path.expanduser("~/.cargo/registry/src/rsproxy.cn-e3de039b2554c837/formualizer-eval-0.10.1/src")
bi = d + "/builtins/"

def category(path):
    rel = path[len(bi):]
    top = rel.split("/")[0]
    return top[:-3] if top.endswith(".rs") else top

def esc(s):
    return s.replace("\\", "\\\\").replace('"', '\\"')

entries = {}
BAD = []
for f in sorted(glob.glob(bi + "**/*.rs", recursive=True)):
    L = open(f, encoding="utf-8").read().split("\n")
    for i, x in enumerate(L):
        if "formualizer-docgen:schema:start" not in x:
            continue
        # 布局A:schema 块上方紧贴 /// 描述;布局B:描述在 #[derive]/pub struct 之前
        def doc_start(end):
            j = end
            while j > 0 and L[j-1].startswith("///"):
                j -= 1
            return j
        j = doc_start(i)
        if j == i:  # 紧贴处没有描述,按布局B往上跳过空行/属性/struct 行
            e = i
            hops = 0
            while e > 0 and hops < 6 and not L[e-1].startswith("///"):
                e -= 1
                hops += 1
            if e > 0 and L[e-1].startswith("///"):
                j = doc_start(e)
                i2 = e
            else:
                j = i
        desc = L[j][3:].strip() if j < i else ""
        if desc.startswith("[") or "docgen" in desc or desc == "":
            BAD.append((f, i + 1))
        name = sig = None
        k = i + 1
        while k < len(L) and "schema:end" not in L[k]:
            t = L[k]
            if t.startswith("/// Name: "):
                name = t[len("/// Name: "):].strip()
            elif t.startswith("/// Signature: "):
                sig = t[len("/// Signature: "):].strip()
            k += 1
        if not name or not sig:
            print("WARN 缺字段", f, i + 1, file=sys.stderr)
            continue
        if name not in entries:   # 同名多块保留第一个
            entries[name] = (sig, desc, category(f))

old = {}
src = open("roe-api/src/formula_function_list.rs", encoding="utf-8").read()
for m in re.finditer(r'FormulaFuncInfo \{ name: "((?:[^"\\]|\\.)*)", args: "((?:[^"\\]|\\.)*)", desc: "((?:[^"\\]|\\.)*)", category: "([^"]*)" \}', src):
    old[m.group(1)] = (m.group(2), m.group(3), m.group(4))

# 源码里没有文档注释的函数,手写英文描述(仅在自动取到的 desc 为空时才生效)
OVERRIDE_DESC = {
    "SUBTOTAL": "Returns a subtotal for a range using a function number, optionally ignoring hidden rows.",
    "AGGREGATE": "Returns an aggregate for a range using a function number, with options to ignore errors or hidden rows.",
    "TOCOL": "Converts an array or range into a single column.",
    "TOROW": "Converts an array or range into a single row.",
    "DEVSQ": "Returns the sum of squared deviations of values from their sample mean.",
}
for _n, _d in OVERRIDE_DESC.items():
    if _n in entries and not entries[_n][1]:
        entries[_n] = (entries[_n][0], _d, entries[_n][2])
_empty = sorted(n for n, v in entries.items() if not v[1])
if _empty:
    print("ERROR 仍有空 desc,不生成:", _empty)
    sys.exit(1)

head = '''// roe-api/src/formula_function_list.rs
// 自动生成：从 formualizer-eval 0.10.1 源码 builtins/**/*.rs 的
// [formualizer-docgen:schema:start] 注释块提取，共 %d 个内置函数（同名多块取第一个）。
// category 字段：builtins/ 下顶级目录名；顶级为单文件时取文件名（去 .rs）。
// 这是文档注释清单，不等于引擎已注册/可求值的函数集合。
// 生成脚本见 docs/ 或本次提交说明；若升级 formualizer-eval 版本，需重新生成。

pub struct FormulaFuncInfo {
    pub name: &'static str,
    pub args: &'static str,
    pub desc: &'static str,
    pub category: &'static str,
}

pub const FORMULA_FUNCTION_LIST: &[FormulaFuncInfo] = &[
''' % len(entries)
out = head
for n in sorted(entries):
    sig, desc, cat = entries[n]
    out += '    FormulaFuncInfo { name: "%s", args: "%s", desc: "%s", category: "%s" },\n' % (esc(n), esc(sig), esc(desc), cat)
out += "];\n"
open("/tmp/formula_function_list.new.rs", "w", encoding="utf-8").write(out)

print("old=%d new=%d" % (len(old), len(entries)))
print("desc 取错/为空:", len(BAD), BAD[:5])
dd = [n for n in entries if n in old and old[n][1] != entries[n][1]]
ds = [n for n in entries if n in old and old[n][0] != entries[n][0]]
dc = [n for n in entries if n in old and old[n][2] != entries[n][2]]
print("desc 变化:", len(dd), dd[:15])
print("签名变化:", len(ds), ds[:15])
print("category 变化:", len(dc), [(n, old[n][2], entries[n][2]) for n in dc[:15]])
print("旧清单顺序是否按名字排序:", list(old) == sorted(old))

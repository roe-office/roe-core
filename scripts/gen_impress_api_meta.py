#!/usr/bin/env python3
"""从 impress_api.rs 的 register_fn 条目生成 roe-script/src/impress_api_meta.rs。接口变更后重跑。"""
import re, json, pathlib
ROOT = pathlib.Path(__file__).resolve().parent.parent
def _prod(n):
    s = (ROOT / n).read_text(encoding="utf-8")
    return s[:s.index("\nmod tests")]
src = _prod("roe-script/src/impress_api.rs") + "\n" + _prod("roe-script/src/impress_api_ext.rs") + "\nmod tests\n"
cut = src.index("\nmod tests")  # 只扫生产代码,不扫测试模块
body = src[:cut]
sq = lambda s: re.sub(r"\s+", " ", s).strip()
rows = []
for m in re.finditer(r'register_fn\(\s*"([^"]+)"\s*,\s*(?:move\s*)?(\|[^|]*\|)\s*(?:->\s*([^{]+?))?\s*\{', body):
    params = sq(m.group(2).strip("|")).rstrip(",").strip()
    ret = sq(m.group(3) or "()")
    rows.append((m.group(1), params, ret))
names = [r[0] for r in rows]
assert len(names) == len(set(names)), "重名: %s" % sorted({n for n in names if names.count(n) > 1})
assert len(rows) == 54, "条目数 %d != 54(接口清单);若 Impress 新增了函数,先核对再改此处" % len(rows)
api_src = (ROOT / "roe-script/src/impress_api.rs").read_text(encoding="utf-8")
mm = re.search(r'pub const READ_FN_NAMES: &\[&str\] = &\[(.*?)\];', api_src, re.S)
assert mm, "找不到 READ_FN_NAMES"
reads = set(re.findall(r'"([a-z_0-9]+)"', mm.group(1)))
assert reads <= set(names), "READ_FN_NAMES 含不在接口清单里的函数: %s" % sorted(reads - set(names))
assert len(reads) == 18, "READ_FN_NAMES 数量 %d != 18;若有变更先核对再改此处" % len(reads)
out = ["// 自动生成,勿手改。脚本: scripts/gen_impress_api_meta.py(接口变更后重跑)",
       "// (名称, 参数, 返回, 只读)。只读标记取自 impress_api::READ_FN_NAMES。",
       "pub static IMPRESS_API_META: &[(&str, &str, &str, bool)] = &["]
for n, p, r in rows:
    out.append("    (%s, %s, %s, %s)," % (json.dumps(n), json.dumps(p, ensure_ascii=False), json.dumps(r, ensure_ascii=False), "true" if n in reads else "false"))
out.append("];")
(ROOT / "roe-script/src/impress_api_meta.rs").write_text("\n".join(out) + "\n", encoding="utf-8")
print("条目:", len(rows))

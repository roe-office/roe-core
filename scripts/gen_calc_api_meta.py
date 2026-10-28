#!/usr/bin/env python3
"""从 calc_api.rs 的 register_calc_apis! 条目生成 roe-script/src/calc_api_meta.rs。接口变更后重跑。"""
import re, json, pathlib
ROOT = pathlib.Path(__file__).resolve().parent.parent
src = (ROOT / "roe-script/src/calc_api.rs").read_text(encoding="utf-8")
body = src[src.index("fn define_and_register_apis"):]
sq = lambda s: re.sub(r"\s+", " ", s).strip()
rows = []
for m in re.finditer(r'fn\s+"([^"]+)"\s*=>\s*', body):
    j = body.index("{", m.end())
    head = body[m.end():j].strip()
    h = re.match(r"\|(.*?)\|\s*(?:->\s*(.*))?$", head, re.S)
    assert h, m.group(1)
    params = sq(h.group(1)).rstrip(",").strip()
    ret = sq(h.group(2) or "()")
    mr = re.match(r"Result<\s*(.+?)\s*,\s*Box<", ret)
    if mr:
        ret = mr.group(1) + " (失败时抛错)"
    t = re.search(r"\}\s*,\s*(true|false)\s*;", body[m.end():])
    assert t, m.group(1)
    rows.append((m.group(1), params, ret, t.group(1) == "true"))
rows.append(("list_functions", "filter: String", "String", True))
names = [r[0] for r in rows]
assert len(names) == len(set(names)), "重名"
out = ["// 自动生成,勿手改。脚本: scripts/gen_calc_api_meta.py(接口变更后重跑)",
       "// (名称, 参数, 返回, 是否只读)",
       "pub static CALC_API_META: &[(&str, &str, &str, bool)] = &["]
for n, p, r, ro in rows:
    out.append("    (%s, %s, %s, %s)," % (json.dumps(n), json.dumps(p, ensure_ascii=False), json.dumps(r, ensure_ascii=False), "true" if ro else "false"))
out.append("];")
(ROOT / "roe-script/src/calc_api_meta.rs").write_text("\n".join(out) + "\n", encoding="utf-8")
print("条目:", len(rows), " 只读:", sum(1 for r in rows if r[3]))

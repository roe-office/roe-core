#!/usr/bin/env python3
"""
探测 CreateGeometry.js 主函数 CreateGeometry(prst, oGeom) 内，每个
`case 'xxx':{ ... }` 块中的 f.AddCnx(ang, x, y) 调用，按声明顺序
（=idx）提取，输出为 JSON：{ prst_name: [[ang,x,y], ...], ... }

范围限定：只解析 CreateGeometry 主函数体（第一个 switch(prst) 所在函数），
不涉及 CreatePrstTxWarpGeometry（文字艺术字变形，另一个switch，无关）。

参数形式核实结论（2026-09，见 roe-ext_progress_v179.md 二.4节 +
本次会话交叉验证）：AddCnx 调用全部是 f.AddCnx('ang_token', 'x_token',
'y_token') 三个简单字符串/数字字面量参数，无表达式嵌套，可用简单正则
逐行匹配，不需要完整JS表达式解析器。
"""
import re
import json
import sys

SRC = "/home/yong/openlib/onlyoffice/sdkjs/common/Drawings/Format/CreateGeometry.js"

with open(SRC, "r", encoding="utf-8") as f:
    lines = f.readlines()

# 定位主函数 CreateGeometry(prst, oGeom) 的起止行（到下一个顶格 function 为止）
start_idx = None
end_idx = None
for i, line in enumerate(lines):
    if line.startswith("function CreateGeometry(prst"):
        start_idx = i
    elif start_idx is not None and line.startswith("function ") and i > start_idx:
        end_idx = i
        break
if end_idx is None:
    end_idx = len(lines)

body = lines[start_idx:end_idx]
print(f"[info] CreateGeometry 函数范围: 源文件第{start_idx+1}行 ~ 第{end_idx}行 "
      f"(共{len(body)}行)", file=sys.stderr)

# 逐行扫描：遇到 case 'xxx':{ 记录当前形状名；遇到 f.AddCnx(...) 记录到当前形状；
# 遇到 break; 或下一个 case 结束当前形状收集（case间不应有遗留状态污染）
case_re = re.compile(r"""case\s+'([A-Za-z0-9_]+)'\s*:""")
addcnx_re = re.compile(
    r"""f\.AddCnx\(\s*'([^']*)'\s*,\s*'([^']*)'\s*,\s*'([^']*)'\s*\)"""
)
# 也要捕获数字字面量参数（不带引号）的情况，以防万一
addcnx_re_mixed = re.compile(
    r"""f\.AddCnx\(\s*([^,]+?)\s*,\s*([^,]+?)\s*,\s*([^)]+?)\s*\)"""
)

result = {}
current_shape = None
unmatched_addcnx = []

for lineno, line in enumerate(body, start=start_idx + 1):
    m = case_re.search(line)
    if m:
        current_shape = m.group(1)
        if current_shape not in result:
            result[current_shape] = []
        continue

    if "AddCnx" in line and current_shape is not None:
        m2 = addcnx_re.search(line)
        if m2:
            ang, x, y = m2.group(1), m2.group(2), m2.group(3)
            result[current_shape].append([ang, x, y])
        else:
            # fallback：尝试宽松匹配，记录下来供人工核查
            m3 = addcnx_re_mixed.search(line)
            if m3:
                unmatched_addcnx.append((lineno, line.strip(), m3.groups()))
            else:
                unmatched_addcnx.append((lineno, line.strip(), None))

# 去掉没有任何 AddCnx 的形状（空列表），保留有数据的
result_nonempty = {k: v for k, v in result.items() if v}

total_calls = sum(len(v) for v in result_nonempty.values())
print(f"[info] 探测到 {len(result_nonempty)} 个形状含 AddCnx 定义，"
      f"共 {total_calls} 次调用", file=sys.stderr)
print(f"[info] 严格正则未匹配的 AddCnx 行数: {len(unmatched_addcnx)}", file=sys.stderr)

if unmatched_addcnx:
    print("[warn] 以下 AddCnx 调用未能用严格三引号参数正则匹配，需要人工检查：",
          file=sys.stderr)
    for lineno, text, groups in unmatched_addcnx:
        print(f"  L{lineno}: {text}  -> fallback捕获: {groups}", file=sys.stderr)

with open("/tmp/cxn_probe_result.json", "w", encoding="utf-8") as f:
    json.dump(result_nonempty, f, ensure_ascii=False, indent=2)

print(f"[info] 结果已写入 /tmp/cxn_probe_result.json", file=sys.stderr)

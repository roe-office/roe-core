# roe

Office 套件无头内核：文档模型、公式计算、渲染管线，不含任何 UI。

被 [`roe-desktop`](../roe-desktop)（slint 前台）通过路径依赖消费。

## Workspace 成员
- `roe-model` — 核心数据模型
- `roe-core` — 渲染/排版核心逻辑
- `roe-parser` — OOXML 解析（docx/xlsx/pptx），quick-xml
- `roe-calc` — Calc 公式引擎（formualizer + petgraph FormulaDAG）
- `roe-writer` — Writer 排版（parley/taffy/cosmic-text）
- `roe-impress` — Impress 渲染（rstar 空间索引 + charts-rs）
- `roe-formula` — LaTeX 公式排版（RaTeX）
- `roe-script` — Rhai 脚本引擎，含只读预览能力
- `roe-polars-bridge` — Polars 数据桥接（透视表、DataFrame）
- `roe-bridge-lok` / `roe-uno-compat` — 兼容层
- `roe-export` — 导出（PDF 等）
- `roe-api` — 对外 C-ABI facade，供 office-rs2 消费
- `roe-agent` — Agent 能力（rig-core + rig-agent）

## 技术栈
- **formualizer** — Excel/OpenFormula 公式解析求值
- **petgraph** — 公式依赖图
- **rstar** — 空间索引
- **parley** / **taffy** / **skrifa** / **swash** / **cosmic-text** — 文字排版
- **RaTeX** — LaTeX 公式排版（vendored，`RaTeX/`）
- **charts-rs** — 图表渲染（vendored，`charts-rs/`）
- **tiny-skia** / **resvg** — 2D 光栅渲染
- **quick-xml** — OOXML 解析
- **rhai** — 脚本引擎
- **rig-core** / **rig-agent** — Agent 框架（`roe-agent`）

## 编译
```bash
cargo check --workspace
cargo test --workspace --lib
```

## 已知事项
- `vendor/parley`、`vendor/formualizer-parse` 是本地修改版，通过
  workspace `exclude` + `[patch.crates-io]` 接入，具体原因见各自
  README/代码注释。

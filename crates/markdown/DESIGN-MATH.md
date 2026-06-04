# Math Rendering in Zed Markdown (via RaTeX)

## 0. 核心思路

**RaTeX 的 DisplayList 就是为「各平台原生渲染」设计的。** RaTeX 负责 LaTeX → DisplayList（一组与平台无关的绘制指令），各平台实现自己的 DisplayList → 原生画布的渲染器。GPUI 的 `canvas()` + `paint_quad()` + `paint_path()` + `ShapedLine::paint()` 正好覆盖了全部 4 种 DisplayItem，不需要任何中间栅格化步骤，也不需要改动 GPUI 本身。

```
LaTeX string → ratex-parser → ratex-layout → DisplayList → GPUI canvas() 原生渲染
                                                    └─ 自带 width/height/depth 布局信息
```

## 1. 现状分析

### 已支持的

- `pulldown-cmark` 解析 LaTeX 公式为 `InlineMath` / `DisplayMath` 事件
- `pulldown-cmark` 自带 `ENABLE_MATH` option

### 缺失的

- `parser.rs`: 数学事件被静默丢弃（`=> {}`）
- `parser.rs`: `MarkdownEvent` 枚举无 `InlineMath` / `DisplayMath` 变体
- `parser.rs`: `PARSE_OPTIONS` 未开启 `ENABLE_MATH`
- `markdown.rs`: 渲染循环无数学事件处理

### 参考：Mermaid 集成模式

Mermaid 的集成是本次实现的最佳参考，因为它在代码结构上高度相似——外部渲染引擎 → 异步 pipeline → GPUI element。

| Mermaid                                  | Math (RaTeX)                                    |
| ---------------------------------------- | ----------------------------------------------- |
| `extract_mermaid_diagrams()` 遍历 events | `extract_math_expressions()` 遍历 events        |
| `MermaidState` 管理缓存                  | `MathState` 管理缓存                            |
| background spawn → `merman` 渲染 PNG     | background spawn → `ratex` layout → DisplayList |
| `render_mermaid_diagram()` → GPUI Image  | `render_math()` → GPUI canvas() 原生绘制        |
| `OnceLock` 异步就绪标志着色              | `OnceLock<DisplayList>` 异步就绪标志            |

关键区别：Mermaid 返回像素图，Math 返回矢量 DisplayList——这让我们可以做到完全原生的 GPUI 渲染。

## 2. 架构设计

### 2.1 整体数据流

```
Markdown source
  │
  ▼
pulldown_cmark (ENABLE_MATH on)
  │  InlineMath(text) / DisplayMath(text)
  ▼
parser.rs → MarkdownEvent::InlineMath / DisplayMath (带 source range)
  │
  ▼
parse_markdown_with_options() → ParsedMarkdownData { events[] }
  │
  ▼
markdown.rs Markdown::parse()
  ├─ extract_math_expressions(source, &events) → BTreeMap<offset, ParsedMathExpression>
  │
  └─ ParsedMarkdown { math_expressions, ... }
  │
  ▼
渲染循环 match event {
    MarkdownEvent::InlineMath  → render_math(expr, math_state, inline=true)  → AnyElement
    MarkdownEvent::DisplayMath → render_math(expr, math_state, inline=false) → AnyElement
}
  │
  ▼
render_math()
  ├─ ratex parse + layout → DisplayList (background spawn, cached via OnceLock)
  └─ DisplayList 就绪 → gpui::canvas() 遍历 DisplayItem[] 原生绘制
```

### 2.2 DisplayList → GPUI 原生渲染映射

RaTeX 的 `DisplayList` 是最终输出——一组绝对坐标的平面绘制指令：

```rust
// ratex-types 提供的核心类型
pub struct DisplayList {
    pub items: Vec<DisplayItem>,
    pub width: f64,      // em units
    pub height: f64,     // em units (above baseline)
    pub depth: f64,      // em units (below baseline)
}

pub enum DisplayItem {
    GlyphPath { x: f64, y: f64, scale: f64, font: String, char_code: u32, color: Color },
    Line      { x: f64, y: f64, width: f64, thickness: f64, color: Color, dashed: bool },
    Rect      { x: f64, y: f64, width: f64, height: f64, color: Color },
    Path      { x: f64, y: f64, commands: Vec<PathCommand>, fill: bool, color: Color },
}
// PathCommand = MoveTo | LineTo | CubicTo | QuadTo | Close
// Color = { r: f32, g: f32, b: f32, a: f32 }
```

**与 GPUI paint API 的一一对应：**

| RaTeX DisplayItem                    | GPUI 绘制 API                                                             |
| ------------------------------------ | ------------------------------------------------------------------------- |
| `Rect { x, y, w, h, color }`         | `window.paint_quad(PaintQuad { bounds, background: color, ..default() })` |
| `Line { x, y, w, thick, color }`     | `window.paint_quad(PaintQuad { bounds: 细矩形, background: color, .. })`  |
| `Path { commands, fill, color }`     | `window.paint_path(path, color)` — 遍历 commands 构建 `gpui::Path`        |
| `GlyphPath { font, char_code, ... }` | `TextSystem::shape_line` + `ShapedLine::paint(origin, window, cx)`        |

**路径指令转换：**

| RaTeX `PathCommand`                | GPUI `Path<Pixels>` 方法                                         |
| ---------------------------------- | ---------------------------------------------------------------- |
| `MoveTo { x, y }`                  | `path.move_to(point(px(x), px(y)))`                              |
| `LineTo { x, y }`                  | `path.line_to(point(px(x), px(y)))`                              |
| `CubicTo { x1, y1, x2, y2, x, y }` | `path.curve_to(to, ctrl)` — GPUI 的 curve_to 接受 (终点, 控制点) |
| `QuadTo { x1, y1, x, y }`          | 需做二次→三次贝塞尔转换\*                                        |
| `Close`                            | GPUI 通过新 contour 的开始隐式 close                             |

> _QuadTo → CubicTo 转换公式：`ctrl1 = current + 2/3_(x1 - current.x, y1 - current.y)`, `ctrl2 = to + 2/3\*(x1 - to.x, y1 - to.y)`，然后调 `curve_to(to, ctrl1)`。

**坐标系统转换：** ratex 输出 em units，GPUI 使用 pixels：

```
pixel = em_value * font_size_px  // font_size_px 由 MarkdownStyle 决定（如 16px）
baseline_y = dl.height * font_size_px  // baseline 在 display list 顶部下方 height em 处
// y 轴方向一致：都是向下为正
```

**颜色转换：**

```rust
fn ratex_color_to_gpui(c: &ratex_types::Color) -> Hsla {
    rgba(c.r, c.g, c.b, c.a)
    // ratex Color: f32 × 4 (0.0–1.0), GPUI rgba: f32 × 4 (0.0–1.0), 直接透传
}
```

### 2.3 字体加载与 GlyphPath 渲染

`GlyphPath` 依赖 KaTeX 字体族来渲染数学符号。RaTeX 使用的字体全部来自 `ratex-katex-fonts`：

| ratex font name     | TTF 文件                                            | 用途               |
| ------------------- | --------------------------------------------------- | ------------------ |
| `KaTeX_Main`        | `KaTeX_Main-*.ttf` (Regular/Bold/Italic/BoldItalic) | 默认正文字体       |
| `KaTeX_Math`        | `KaTeX_Math-*.ttf` (Italic/BoldItalic)              | 数学斜体变量       |
| `KaTeX_AMS`         | `KaTeX_AMS-Regular.ttf`                             | AMS 扩展符号       |
| `KaTeX_Caligraphic` | `KaTeX_Caligraphic-*.ttf`                           | `\mathcal` 花体    |
| `KaTeX_Fraktur`     | `KaTeX_Fraktur-*.ttf`                               | `\mathfrak` 哥特体 |
| `KaTeX_SansSerif`   | `KaTeX_SansSerif-*.ttf`                             | `\mathsf` 无衬线   |
| `KaTeX_Script`      | `KaTeX_Script-Regular.ttf`                          | `\mathscr` 手写体  |
| `KaTeX_Size1–4`     | `KaTeX_Size[1-4]-Regular.ttf`                       | 可伸缩分隔符       |
| `KaTeX_Typewriter`  | `KaTeX_Typewriter-Regular.ttf`                      | `\mathtt` 等宽     |

**字体加载**（应用启动时一次性加载全部 20 个 TTF）：

```rust
fn ensure_math_fonts_loaded(cx: &mut App) {
    static LOADED: AtomicBool = AtomicBool::new(false);
    if LOADED.swap(true, Ordering::SeqCst) { return; }
    let fonts: Vec<Cow<'static, [u8]>> = KATEX_FONT_BYTES.iter()
        .map(|b| Cow::Borrowed(*b)).collect();
    cx.text_system().add_fonts(fonts).log_err();
}
```

**GlyphPath 渲染**——不走 `paint_glyph` 底层 API，而是走 GPUI 标准文本路径 `shape_line` + `ShapedLine::paint`：

```rust
DisplayItem::GlyphPath { x, y, scale, font, char_code, color } => {
    let ch = char::from_u32(*char_code).unwrap_or('\u{FFFD}');
    let text: SharedString = ch.to_string().into();
    let line = text_system.shape_line(
        text,
        px(*scale as f32 * font_size),
        &[TextRun {
            len: ch.len_utf8(),
            font: font(font.as_str()),
            color: ratex_color_to_gpui(*color),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let origin = bounds.origin + point(
        px(*x as f32 * font_size),
        px(baseline_y + *y as f32 * font_size),
    );
    line.paint(origin, window, cx).log_err();
}
```

这样做的好处：**不需要改动 GPUI 任何代码**，复用现有的 shaping 和 paint 管线。每个字形一次 `shape_line` 调用，公式通常几十个符号，总开销可忽略（1-2ms），且整个 pipeline 异步，不影响主线程。

### 2.4 模块边界

```
crates/markdown/
├── src/
│   ├── math.rs          ← 新增：MathState, render_math, extract_math_expressions,
│   │                           DisplayList → GPUI canvas 渲染器, 字体管理
│   ├── parser.rs        ← 改动：MarkdownEvent 加 InlineMath/DisplayMath 变体,
│   │                           开 ENABLE_MATH, 转发事件
│   ├── markdown.rs      ← 改动：MarkdownOptions 加 render_math,
│   │                           ParsedMarkdown 加 math_expressions,
│   │                           渲染循环加两个 match 分支,
│   │                           主题变化时清空 math 缓存
│   └── markdown.rs      ← 改动：同上
│
crates/math_render/       ← 后续可选抽取：DisplayList → GPUI 渲染器 crate
```

**初期直接在 `math.rs` 里实现全部逻辑**，等接口稳定（DisplayList → GPUI 映射、字体管理、缓存策略）再抽 crate。

## 3. 实现步骤

### Step 1: parser.rs — 解析管线

**文件**: `crates/markdown/src/parser.rs`

1. `MarkdownEvent` 枚举加变体（不存 String，通过 `source[range]` 获取，和 `Text`/`Code` 事件一致）：

```rust
pub enum MarkdownEvent {
    // ... 现有变体 ...
    /// Inline math: $...$
    InlineMath,
    /// Display math: $$...$$
    DisplayMath,
}
```

2. `PARSE_OPTIONS` 加 `ENABLE_MATH`：

```rust
pub const PARSE_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_FOOTNOTES)
    // ... existing ...
    .union(Options::ENABLE_MATH);
```

3. 事件转发（替换现有 `{}`）：

```rust
pulldown_cmark::Event::InlineMath(_) => {
    state.push_event(range, MarkdownEvent::InlineMath)
}
pulldown_cmark::Event::DisplayMath(_) => {
    state.push_event(range, MarkdownEvent::DisplayMath)
}
```

4. 测试更新：`UNWANTED_OPTIONS` 中移除 `ENABLE_MATH`。

### Step 2: math.rs — 提取、解析、缓存、原生渲染

**新文件**: `crates/markdown/src/math.rs`

#### 数据结构

```rust
use ratex_types::{Color as RatexColor, DisplayItem, DisplayList, PathCommand};
use gpui::*;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock, atomic::AtomicBool};

// ── 字体管理 ──────────────────────────────────────────

/// KaTeX 字体字节，编译时嵌入（通过 include_bytes! 或 asset system）
const KATEX_FONT_DATA: &[(&str, &[u8])] = &[
    ("KaTeX_Main-Regular",       include_bytes!("../../assets/fonts/KaTeX_Main-Regular.ttf")),
    ("KaTeX_Main-Bold",          include_bytes!("...")),
    // ... 全部 20 个 TTF 文件 ...
];

static FONTS_LOADED: AtomicBool = AtomicBool::new(false);

/// 确保 KaTeX 字体已加载到 GPUI TextSystem。多次调用安全，仅首次执行。
pub(crate) fn ensure_math_fonts_loaded(text_system: &TextSystem) {
    if FONTS_LOADED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let fonts: Vec<_> = KATEX_FONT_DATA.iter()
        .map(|(_, bytes)| Cow::Borrowed(*bytes))
        .collect();
    text_system.add_fonts(fonts).log_err();
}

// ── 数学表达式 ────────────────────────────────────────

pub(crate) struct ParsedMathExpression {
    pub(crate) content_range: Range<usize>,
    pub(crate) tex: SharedString,
    pub(crate) display_mode: bool,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct MathCacheKey {
    tex: SharedString,
    display_mode: bool,
    font_size_px: u32,  // 取整足够区分
}

struct CachedMathRender {
    display_list: Arc<OnceLock<anyhow::Result<DisplayList>>>,
    _task: Task<()>,
}

pub(crate) struct MathState {
    cache: HashMap<MathCacheKey, Arc<CachedMathRender>>,
    order: Vec<MathCacheKey>,
}
```

#### extract_math_expressions()

和 Mermaid 的 `extract_mermaid_diagrams()` 同模式：

```rust
pub(crate) fn extract_math_expressions(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
) -> BTreeMap<usize, ParsedMathExpression> {
    let mut expressions = BTreeMap::default();
    for (source_range, event) in events {
        let display_mode = match event {
            MarkdownEvent::InlineMath => false,
            MarkdownEvent::DisplayMath => true,
            _ => continue,
        };
        let tex = source[source_range.clone()].to_string();
        let tex = strip_math_delimiters(&tex, display_mode);
        if tex.trim().is_empty() { continue; }
        expressions.insert(source_range.start, ParsedMathExpression {
            content_range: source_range.clone(),
            tex: SharedString::from(tex),
            display_mode,
        });
    }
    expressions
}

fn strip_math_delimiters(tex: &str, display_mode: bool) -> String {
    if display_mode {
        tex.trim().strip_prefix("$$").and_then(|s| s.strip_suffix("$$")).unwrap_or(tex)
    } else {
        tex.trim().strip_prefix('$').and_then(|s| s.strip_suffix('$')).unwrap_or(tex)
    }.to_string()
}
```

#### MathState::update()

```rust
impl MathState {
    pub(crate) fn update(
        &mut self,
        parsed: &ParsedMarkdown,
        font_size: f32,
        text_system: &TextSystem,
        cx: &mut Context<Markdown>,
    ) {
        ensure_math_fonts_loaded(text_system);

        for expr in parsed.math_expressions.values() {
            let key = MathCacheKey {
                tex: expr.tex.clone(),
                display_mode: expr.display_mode,
                font_size_px: font_size as u32,
            };
            if self.cache.contains_key(&key) { continue; }

            let tex = expr.tex.clone();
            let display_list = Arc::new(OnceLock::new());
            let dl = display_list.clone();
            let task = cx.background_spawn(async move {
                // ratex pipeline: parse → layout → DisplayList
                let result = parse_and_layout(&tex);
                let _ = dl.set(result);
            });
            self.cache.insert(key, Arc::new(CachedMathRender {
                display_list,
                _task: task,
            }));
        }
    }
}
```

#### render_math() — 核心：DisplayList → GPUI canvas 全原生绘制

```rust
pub(crate) fn render_math(
    expr: &ParsedMathExpression,
    math_state: &MathState,
    font_size: Pixels,
    style: &MarkdownStyle,
    source_offset: usize,
) -> AnyElement {
    let key = MathCacheKey {
        tex: expr.tex.clone(),
        display_mode: expr.display_mode,
        font_size_px: font_size.0 as u32,
    };
    let cached = math_state.cache.get(&key);
    let display_list = cached.and_then(|c| c.display_list.get()?.as_ref().ok());

    match display_list {
        Some(dl) => {
            let total_height = dl.height + dl.depth;  // em
            let width = px(dl.width as f32 * font_size.0);
            let height = px(total_height as f32 * font_size.0);

            // Clone what we need for the paint closure
            let dl = dl.clone();

            canvas(
                // prepaint: nothing special, just pass through
                move |bounds, _window, _cx| dl,
                // paint: iterate DisplayList and draw each item natively
                move |bounds, dl, window, cx| {
                    let baseline_y = dl.height as f32 * font_size.0;
                    let text_system = cx.text_system().clone();

                    for item in &dl.items {
                        match item {
                            // ── Rect: 填充矩形 ──
                            DisplayItem::Rect { x, y, width, height, color } => {
                                let origin = bounds.origin + point(
                                    px(*x as f32 * font_size.0),
                                    px(baseline_y + *y as f32 * font_size.0),
                                );
                                window.paint_quad(PaintQuad {
                                    bounds: Bounds {
                                        origin,
                                        size: size(
                                            px(*width as f32 * font_size.0),
                                            px(*height as f32 * font_size.0),
                                        ),
                                    },
                                    background: Some(ratex_color_to_gpui(*color)),
                                    ..Default::default()
                                });
                            }

                            // ── Line: 水平线（用细矩形模拟）──
                            DisplayItem::Line { x, y, width, thickness, color, .. } => {
                                let origin = bounds.origin + point(
                                    px(*x as f32 * font_size.0),
                                    px(baseline_y + *y as f32 * font_size.0),
                                );
                                window.paint_quad(PaintQuad {
                                    bounds: Bounds {
                                        origin,
                                        size: size(
                                            px(*width as f32 * font_size.0),
                                            px(*thickness as f32 * font_size.0),
                                        ),
                                    },
                                    background: Some(ratex_color_to_gpui(*color)),
                                    ..Default::default()
                                });
                            }

                            // ── Path: SVG 风格路径 ──
                            DisplayItem::Path { x, y, commands, fill, color } => {
                                let offset = bounds.origin + point(
                                    px(*x as f32 * font_size.0),
                                    px(baseline_y + *y as f32 * font_size.0),
                                );
                                let mut path = gpui::Path::new(offset);
                                for cmd in commands {
                                    match cmd {
                                        PathCommand::MoveTo { x, y } => {
                                            let to = offset + point(
                                                px(*x as f32 * font_size.0),
                                                px(*y as f32 * font_size.0),
                                            );
                                            path.move_to(to);
                                        }
                                        PathCommand::LineTo { x, y } => {
                                            let to = offset + point(
                                                px(*x as f32 * font_size.0),
                                                px(*y as f32 * font_size.0),
                                            );
                                            path.line_to(to);
                                        }
                                        PathCommand::CubicTo { x1, y1, x2, y2, x, y } => {
                                            // GPUI curve_to 接受 (终点, 控制点1)
                                            let ctrl = offset + point(
                                                px(*x1 as f32 * font_size.0),
                                                px(*y1 as f32 * font_size.0),
                                            );
                                            let to = offset + point(
                                                px(*x as f32 * font_size.0),
                                                px(*y as f32 * font_size.0),
                                            );
                                            path.curve_to(to, ctrl);
                                        }
                                        PathCommand::QuadTo { x1, y1, x, y } => {
                                            // 二次 → 三次贝塞尔转换
                                            let current = /* ... */;
                                            let ctrl_x1 = current.x + 2.0/3.0 * (*x1 - current.x);
                                            let ctrl_y1 = current.y + 2.0/3.0 * (*y1 - current.y);
                                            let to = offset + point(
                                                px(*x as f32 * font_size.0),
                                                px(*y as f32 * font_size.0),
                                            );
                                            let ctrl = offset + point(
                                                px(ctrl_x1 as f32 * font_size.0),
                                                px(ctrl_y1 as f32 * font_size.0),
                                            );
                                            path.curve_to(to, ctrl);
                                        }
                                        PathCommand::Close => { /* 新 contour 隐式 close */ }
                                    }
                                }
                                let color = ratex_color_to_gpui(*color);
                                if *fill {
                                    window.paint_path(path, color);
                                } else {
                                    window.paint_path(path, color);
                                }
                            }

                            // ── GlyphPath: 字体字形 ──
                            DisplayItem::GlyphPath { x, y, scale, font, char_code, color } => {
                                let ch = char::from_u32(*char_code).unwrap_or('\u{FFFD}');
                                let text: SharedString = ch.to_string().into();
                                let line = text_system.shape_line(
                                    text,
                                    px(*scale as f32 * font_size.0),
                                    &[TextRun {
                                        len: ch.len_utf8(),
                                        font: font(font.as_str()),
                                        color: ratex_color_to_gpui(*color),
                                        background_color: None,
                                        underline: None,
                                        strikethrough: None,
                                    }],
                                    None,
                                );
                                let origin = bounds.origin + point(
                                    px(*x as f32 * font_size.0),
                                    px(baseline_y + *y as f32 * font_size.0),
                                );
                                line.paint(origin, window, cx).log_err();
                            }
                        }
                    }
                },
            )
            .w(width)
            .h(height)
            .into_any_element()
        }
        None => {
            // 渲染中 / 解析失败 → 显示原始 LaTeX
            div()
                .text_color(if expr.display_mode { Color::Muted } else { Color::Default })
                .text_size(font_size)
                .child(SharedString::from(format!(
                    "{}{}{}",
                    if expr.display_mode { "$$" } else { "$" },
                    expr.tex,
                    if expr.display_mode { "$$" } else { "$" },
                )))
                .into_any_element()
        }
    }
}
```

### Step 3: markdown.rs — 渲染循环与配置集成

**文件**: `crates/markdown/src/markdown.rs`

1. `MarkdownOptions` 加开关：

```rust
#[derive(Clone, Copy, Default)]
pub struct MarkdownOptions {
    pub render_math: bool,
    // ... 现有字段 ...
}
```

2. `Markdown` struct 加字段：

```rust
pub struct Markdown {
    math_state: MathState,
    // ... 现有字段 ...
}
```

3. `ParsedMarkdown` 加字段：

```rust
pub(crate) math_expressions: BTreeMap<usize, ParsedMathExpression>,
```

4. 构造时初始化 + parse 流程集成（参照 mermaid 的 `mermaid_state.update()` 位置）：

```rust
// Markdown::new_with_options() 中
this.math_state = MathState::default();

// parse() 的 background_spawn 回调中
this.math_state.update(&parsed, font_size, cx.text_system(), cx);
```

5. 渲染循环加两个分支（在 `MarkdownEvent::Text` / `MarkdownEvent::Code` 附近）：

```rust
MarkdownEvent::InlineMath => {
    if render_math {
        if let Some(expr) = parsed_math.math_expressions.get(&range.start) {
            builder.push_sourced_element(
                range.clone(),
                render_math(expr, math_state, font_size, &self.style, range.start),
            );
            continue;
        }
    }
    // fallback: 不渲染 math → 显示原始 LaTeX 文本
    builder.push_text(&source[range.clone()], range.clone());
}
MarkdownEvent::DisplayMath => {
    if render_math {
        if let Some(expr) = parsed_math.math_expressions.get(&range.start) {
            let el = div()
                .w_full()
                .py_2()
                .flex()
                .justify_center()
                .child(render_math(expr, math_state, font_size, &self.style, range.start));
            builder.push_sourced_element(range.clone(), el);
            continue;
        }
    }
    builder.push_text(&source[range.clone()], range.clone());
}
```

### Step 4: Cargo.toml — 依赖

```toml
# crates/markdown/Cargo.toml
ratex-parser = { git = "https://github.com/erweixin/RaTeX", rev = "<commit>" }
ratex-layout = { git = "https://github.com/erweixin/RaTeX", rev = "<commit>" }
ratex-types  = { git = "https://github.com/erweixin/RaTeX", rev = "<commit>" }
# 不需要 ratex-render — 全部原生 GPUI 渲染
```

### Step 5: 字体文件嵌入

将 `ratex-katex-fonts/fonts/` 下的 20 个 TTF 复制到 `crates/assets/fonts/`（或 `crates/markdown/fonts/`），通过 include_bytes! 或 asset system 加载。与现有 Zed 字体加载流程一致。

## 4. 关键设计决策

### 4.1 Inline vs Block Math 的布局

- **Display math** (`$$...$$`)：作为 block-level sourced element 插入 Markdown tree，包在 `div().w_full().flex().justify_center()` 中居中显示，上下留白。
- **Inline math** (`$...$`)：不应长期用 `inline_flex`/普通 child element 拼接。Zed 当前 Markdown 文本管线是 `PendingLine -> StyledText -> RenderedText`，直接 flush 文本再插 element 会破坏行内 baseline、换行、selection/source mapping 和 link hit testing。长期方案应实现 **inline replacement object**：在 text layout 中用 U+FFFC 占位参与 shaping/wrapping，再在 paint 阶段按占位 glyph bounds 绘制公式。

详见本文档第 9 节「长期行内公式架构」。

### 4.2 缓存策略

同 Mermaid 模式：

- Key = `(tex, display_mode, font_size as u32)`
- `OnceLock<Result<DisplayList>>` 确保同一公式只 parse+layout 一次
- 主题变化 → 清空缓存（公式 Color 可能随主题联动）

### 4.3 错误处理

| 阶段                       | 失败表现                                                    |
| -------------------------- | ----------------------------------------------------------- |
| ratex parse/layout 失败    | 显示原始 LaTeX 源码（灰色），用户可识别问题                 |
| 字体未加载                 | `shape_line` 在 font 不存在时返回默认字形，公式可能形状异常 |
| 字体有，但某个字符没有字形 | `shape_line` 的 shaping 结果为空 → 跳过该字形               |
| 异步尚未完成               | 显示原始 LaTeX 源码，OnceLock 就绪后自动切换为渲染结果      |

### 4.4 DPI / Scale Factor

- ratex 输出 em units，乘以 `font_size_px` 得到像素坐标
- 窗口 DPI 变化时 canvas 自动 re-layout → paint 重绘
- 缓存 key 包含 `font_size as u32`，不同字号或 DPI 下触发新的 parse+layout

## 5. 文件变更清单

```
crates/markdown/src/parser.rs               ~20 lines  (变体 + option + 转发)
crates/markdown/src/math.rs                 ~400 lines (新文件)
crates/markdown/src/markdown.rs             ~80 lines  (options + state + 渲染分支)
crates/markdown/Cargo.toml                  ~4 lines   (ratex git deps)
crates/markdown/DESIGN-MATH.md              本文档
```

KaTeX 字体文件（选一个位置）：

```
crates/assets/fonts/KaTeX_*.ttf            添加 20 个 TTF 文件（~1.2 MB）
```

后续可选抽取：

```
crates/math_render/                         DisplayList → GPUI 渲染器独立 crate
```

## 6. 测试策略

### 单元测试（parser.rs）

- `$x^2$` → `InlineMath` 事件，range 正确（含 `$` 符号）
- `$$\frac{1}{2}$$` → `DisplayMath` 事件
- 已有测试无回归

### 单元测试（math.rs）

- `extract_math_expressions()` 正确区分 inline/display
- `strip_math_delimiters()` 正确剥离 `$` / `$$`
- `MathState::update()` 缓存逻辑：命中/未命中
- ratex_color → Hsla 转换
- QuadTo → CubicTo 转换数学正确性

### 集成测试（markdown.rs）

- `render_markdown("$E=mc^2$")` 不 panic
- 公式在表格/列表/引用块中正常渲染
- 主题切换 → 缓存清空 → 重新渲染

### 后续

- 用 ratex golden tests 做像素级对比

## 7. 风险 & 缓解

| 风险                                | 缓解                                               |
| ----------------------------------- | -------------------------------------------------- |
| ratex 代码仓库不稳定（unreleased）  | pin commit hash，版本锁定                          |
| 20 个 TTF 字体增加包体积（~1.2 MB） | 按需加载，首次渲染数学公式时才 load                |
| QuadTo → CubicTo 转换精度           | 验证 vs ratex-render 的 tiny-skia 输出             |
| GPUI TextStyle/颜色未适配 dark mode | ratex 的 Color 直接映射到 Hsla，主题联动需额外处理 |

## 8. 后续增强（非首期）

- [ ] `\ce` / `\pu` 化学/物理公式支持（ratex 已内置）
- [ ] 公式右键菜单：Copy as LaTeX
- [ ] 公式 block 的 hover 工具栏（参考 code block 的 copy button）
- [ ] 公式点击进入编辑模式

## 9. 长期行内公式架构

本节记录 **Zed 社区可接受的长期方案**。目标不是做一个临时能看的 `inline_flex` 拼接，而是在 Markdown 文本排版管线中正式支持 inline replacement object，使行内公式像 inline image / object replacement glyph 一样参与换行、baseline、selection、link hit testing 和复制。

### 9.1 为什么不能用短期 `inline_flex` 方案

Zed 当前 Markdown builder 的核心是：

```rust
struct PendingLine {
    text: String,
    runs: Vec<TextRun>,
    source_mappings: Vec<SourceMapping>,
}
```

`push_text()` 会把文本追加到 `PendingLine`。`flush_text()` 再把整段 pending text 变成一个 `StyledText`：

```rust
let text = StyledText::new(line.text).with_runs(line.runs);
self.rendered_lines.push(RenderedLine { layout: text.layout().clone(), ... });
self.div_stack.last_mut().unwrap().extend([text.into_any()]);
```

但 `push_sourced_element()` 会先 `flush_text()`，然后把 element 作为普通 child 插入：

```rust
fn push_sourced_element(&mut self, source_range: Range<usize>, element: impl Into<AnyElement>) {
    self.flush_text();
    ...
    self.div_stack.last_mut().unwrap().extend([...]);
}
```

所以如果行内公式直接走 element 插入，会把一段文字拆成多个 sibling：

```text
StyledText("hello ")
MathElement("x^2")
StyledText(" world")
```

这会带来长期不可接受的问题：

- baseline 不是文本 layout 决定的，公式容易偏上/偏下；
- formula width 不参与 `StyledText` 的 wrapping，换行行为不稳定；
- source mapping 和 selection 只知道 flush 后的多个独立元素，很难做到连续选择；
- 链接命中区域需要另写一套逻辑，`[$x^2$](...)` 很容易不完整；
- copy selected text 时，公式很难自然还原为 `$x^2$`；
- 性能上每个 inline formula 都是额外 element/layout，不如文本管线中的 replacement object 可控。

因此，短期 `inline_flex` 可以做 prototype，但不应作为最终 PR 方案。

### 9.2 最佳实践：inline replacement object

推荐方案是在 Markdown text layout 中引入 **inline replacement object**。这是 Web/排版系统里处理行内图片、行内 widget、数学公式的通用方式。

核心思想：

```text
Markdown inline stream
  text: "hello "
  math: "$x^2$"
  text: " world"

转换为 text layout stream
  "hello \u{fffc} world"
          ▲
          object replacement character
```

其中 `U+FFFC OBJECT REPLACEMENT CHARACTER` 只用于逻辑排版，不直接显示。公式自己的 metrics 参与 line layout：

```rust
struct InlineObjectMetrics {
    width: Pixels,
    ascent: Pixels,
    descent: Pixels,
}
```

对 RaTeX 来说，metrics 天然来自 `DisplayList`：

```rust
width   = display_list.width  * font_size
ascent  = display_list.height * font_size
 descent = display_list.depth  * font_size
height  = ascent + descent
```

排版时：

```text
line baseline = max(text ascent, object ascent, ...)
object top y  = line baseline - object ascent
```

绘制时根据 object 在行内 layout 中的位置，调用现有 `paint_display_item()` 绘制 DisplayList。

### 9.3 与 gpui-component 的参考实现对齐

`gpui-component` 已经实现了类似模型，值得作为设计参考，但不能直接照搬全部代码。它的关键结构是：

```rust
const INLINE_OBJECT_REPLACEMENT: char = '\u{fffc}';

enum ParagraphInlineItem {
    Text(...),
    Image(...),
    Math(ParagraphInlineMath),
    Break,
}

struct ParagraphInlineMath {
    node: MathNode,
    link: Option<LinkMark>,
    display: bool,
}
```

它给 math node 提供 baseline metrics：

```rust
pub(crate) struct MathMetrics {
    pub(crate) size: Size<Pixels>,
    pub(crate) ascent: Pixels,
    pub(crate) descent: Pixels,
}
```

行内排版时把公式当成 flow item：

```rust
logical.push(INLINE_OBJECT_REPLACEMENT);
flow_items.push(ParagraphInlineFlowItem {
    width: metrics.size.width,
    kind: ParagraphInlineFlowItemKind::Math { math, metrics },
    ...
});
```

baseline 对齐：

```rust
update_line_metrics(line, metrics.ascent, metrics.descent);
let y = line.ascent - metrics.ascent;
```

paint 阶段：

```rust
let math_bounds = Bounds { origin, size: math.size };
math.node.paint_at(math_bounds, text_color, window, cx);
```

Zed 的实现不一定要重写整个 `ParagraphInlineLayout`，但应该采用同一原则：**用 object replacement 参与文本 layout，而不是用 flex child 拼接。**

### 9.4 推荐的 Zed 数据结构扩展

在 `crates/markdown/src/markdown.rs` 中扩展 `PendingLine`：

```rust
const INLINE_OBJECT_REPLACEMENT: char = '\u{fffc}';

struct PendingLine {
    text: String,
    runs: Vec<TextRun>,
    source_mappings: Vec<SourceMapping>,
    inline_objects: Vec<PendingInlineObject>,
}

struct PendingInlineObject {
    rendered_index: usize,
    source_range: Range<usize>,
    kind: InlineObjectKind,
    metrics: InlineObjectMetrics,
}

enum InlineObjectKind {
    Math {
        expression: ParsedMathExpressionContents,
    },
}

struct InlineObjectMetrics {
    width: Pixels,
    ascent: Pixels,
    descent: Pixels,
}
```

`rendered_index` 是 `U+FFFC` 插入前的 byte/char position，用于之后从 laid-out text 中找到占位对象的位置。

> 注意：如果 `StyledText` / `TextLayout` 使用 byte index，则这里必须存 byte index；如果内部更适合 UTF-16/codepoint/grapheme index，应统一命名避免混淆。`U+FFFC` 是 3-byte UTF-8，source mapping 和 text run len 都要明确使用同一种单位。

### 9.5 Math metrics API

当前 `render_math_expression()` 返回 `AnyElement`，适合 display math，但行内公式需要先知道 metrics 才能参与排版。建议新增非渲染 API：

```rust
pub(crate) struct MathLayoutMetrics {
    pub(crate) width: Pixels,
    pub(crate) ascent: Pixels,
    pub(crate) descent: Pixels,
}

pub(crate) fn math_layout_metrics(
    expr: &ParsedMathExpression,
    math_state: &MathState,
    font_size: Pixels,
) -> Option<MathLayoutMetrics>;
```

规则：

- DisplayList 已就绪：返回真实 metrics；
- DisplayList 未就绪但有 fallback：返回 fallback metrics；
- 未就绪且无 fallback：返回原始 `$...$` 文本的 shaped width/line metrics，避免 layout 抖动过大；
- parse/layout 失败：返回原始 `$...$` 文本 metrics，并在 paint/copy 中显示原始文本。

为了减少 layout 抖动，`MathState::get_fallback()` 现有逻辑可继续用于 inline math：同一 source 顺序下，上一轮已有 DisplayList 的公式先用旧 metrics 占位。

### 9.6 `push_inline_math` 行为

新增 builder 方法：

```rust
fn push_inline_math(
    &mut self,
    source_range: Range<usize>,
    expr: &ParsedMathExpression,
    math_state: &MathState,
    font_size: Pixels,
) {
    let Some(metrics) = math_layout_metrics(expr, math_state, font_size) else {
        self.push_text(&source[source_range.clone()], source_range);
        return;
    };

    let rendered_index = self.pending_line.text.len();
    self.pending_line.source_mappings.push(SourceMapping {
        rendered_index,
        source_index: source_range.start,
    });
    self.pending_line.text.push(INLINE_OBJECT_REPLACEMENT);
    self.pending_line.runs.push(self.text_style().to_run(INLINE_OBJECT_REPLACEMENT.len_utf8()));
    self.pending_line.inline_objects.push(PendingInlineObject {
        rendered_index,
        source_range: source_range.clone(),
        kind: InlineObjectKind::Math {
            expression: expr.contents.clone(),
        },
        metrics,
    });
    self.current_source_index = source_range.end;
}
```

`MarkdownEvent::InlineMath` 变成：

```rust
MarkdownEvent::InlineMath => {
    if render_math {
        if let Some(expr) = parsed_markdown.math_expressions.get(&range.start) {
            builder.push_inline_math(range.clone(), expr, &math_state, font_size);
        } else {
            builder.push_text(&parsed_markdown.source[range.clone()], range.clone());
        }
    } else {
        builder.push_text(&parsed_markdown.source[range.clone()], range.clone());
    }
}
```

### 9.7 Text layout 集成点

这是该方案的核心，也是需要社区讨论的地方。Zed 当前 `StyledText`/`TextLayout` 并不知道 `U+FFFC` 的外部 object metrics。长期可接受的做法是给文本布局增加通用能力，而不是只为 Markdown math 打补丁。

建议新增 GPUI/Markdown 级别的通用接口：

```rust
struct InlineReplacement {
    rendered_range: Range<usize>,
    width: Pixels,
    ascent: Pixels,
    descent: Pixels,
}
```

然后让 `StyledText` layout 支持：

```rust
StyledText::new(text)
    .with_runs(runs)
    .with_inline_replacements(replacements)
```

布局语义：

- replacement 对应的 text range 通常是 `U+FFFC`；
- shaping/wrapping 使用 replacement 的 width，而不是字体中 `U+FFFC` glyph 的 advance；
- line ascent/descent 合并 replacement 的 ascent/descent；
- `TextLayout` 能返回 replacement 的 laid-out bounds。

可能的 API：

```rust
impl TextLayout {
    fn bounds_for_inline_replacement(&self, rendered_range: Range<usize>) -> Option<Bounds<Pixels>>;
}
```

或者在 Markdown 自己保存：

```rust
struct RenderedInlineObject {
    source_range: Range<usize>,
    rendered_range: Range<usize>,
    kind: InlineObjectKind,
}
```

paint 阶段通过 `TextLayout` 查询 placeholder bounds，再画公式。

社区可接受点：这不是「Markdown math 特化」，而是 GPUI/Markdown 未来可复用的能力：

- inline math；
- inline images；
- emoji/sticker/custom widgets；
- mentions/chips；
- diagnostics badges；
- rich preview placeholders。

### 9.8 Paint 阶段

`flush_text()` 需要把 inline objects 转入 `RenderedLine`：

```rust
struct RenderedLine {
    layout: TextLayout,
    source_mappings: Vec<SourceMapping>,
    source_end: usize,
    language: Option<Arc<Language>>,
    text_align: TextAlign,
    inline_objects: Vec<RenderedInlineObject>,
}

struct RenderedInlineObject {
    source_range: Range<usize>,
    rendered_range: Range<usize>,
    kind: InlineObjectKind,
    metrics: InlineObjectMetrics,
}
```

Markdown paint 阶段：

```rust
for object in &rendered_line.inline_objects {
    let Some(bounds) = rendered_line
        .layout
        .bounds_for_inline_replacement(object.rendered_range.clone())
    else {
        continue;
    };

    match &object.kind {
        InlineObjectKind::Math { expression } => {
            paint_math_expression_at(expression, bounds, style.base_text_style.color, window);
        }
    }
}
```

`paint_math_expression_at` 复用现有 `paint_display_item()`：

```rust
origin.x = bounds.origin.x
origin.y = bounds.origin.y + metrics.ascent - dl.height * font_size
```

如果 `bounds` 已经代表 object top-left，且 object metrics 来自 `dl.height/depth`，则更简单：

```rust
origin = bounds.origin + padding
```

关键是：object 的 top-left 必须由 baseline 计算得出，而不是 flex alignment 猜出来。

### 9.9 Selection、source mapping 和 copy

inline math 的 source range 应覆盖原始 `$...$`，不是仅内部 LaTeX。复制时应保留 Markdown 源码。

推荐行为：

- 光标/selection 命中 formula object 时，将其视为一个不可拆分 inline object；
- selection 覆盖 object 任意部分时，copy 输出完整 `$...$`；
- source mapping 中 `U+FFFC` 对应 `source_range.start`；
- `source_end` 覆盖到 `source_range.end`；
- `bounds_for_source_range()` 对 inline object 返回 object bounds。

这要求 `RenderedText` 的 range 查询支持 inline object：

```rust
impl RenderedText {
    fn bounds_for_source_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        // include text bounds + inline object bounds
    }
}
```

如果第一版无法完整支持编辑级 selection，最低可接受行为：

- rendered bounds 用于 hover/click/highlight；
- copy selected text 仍从 source range 直接取 Markdown source；
- formula object 不允许内部局部选择。

### 9.10 Link hit testing

`[$x^2$](https://example.com)` 是重要用例。inline math 在 link 中时应表现得像文字：

- 颜色使用 link color；
- hover cursor 使用 pointer；
- click bounds 是 formula object bounds；
- rendered link 的 `source_range` 包含 math object。

实现上有两个选择：

1. 在 `InlineObjectKind::Math` 中保存当前 link 信息；
2. 继续沿用现有 `rendered_links` + `bounds_for_source_range()`，只要 inline object bounds 能按 source range 返回，就自然包含在 link hitbox 中。

推荐选择 2，因为更通用，也更接近现有 Zed Markdown link 逻辑。

### 9.11 Performance 设计

长期高性能目标：

- RaTeX parse/layout 仍在 background task 中完成；
- DisplayList geometry 按 `(latex, display_mode)` 缓存，不把颜色放进 key；
- paint 阶段用当前 text color 替换默认 black；
- glyph 走 `paint_glyph` / glyph atlas；
- rect/line/path 走 GPUI primitives；
- inline replacement metrics 只在 DisplayList 或 font size 改变时重算。

缓存 key 建议：

```rust
#[derive(Hash, Eq, PartialEq)]
struct MathCacheKey {
    contents: SharedString,
    display_mode: bool,
}
```

Metrics 不必进 parse/layout cache，因为 DisplayList 是 em units：

```rust
metrics_px = display_list_metrics_em * font_size_px
```

这样主题切换只重绘，不需要重新 parse/layout。字号变化只重算 metrics，不需要重新 parse/layout。

### 9.12 分阶段实现计划

#### Phase 1: DisplayList metrics API

- [ ] 给 `math.rs` 增加 `MathLayoutMetrics`；
- [ ] 从 `DisplayList { width, height, depth }` 计算 inline metrics；
- [ ] 保持 display math 现有行为不变；
- [ ] 添加 metrics 单元测试。

#### Phase 2: Markdown inline object model

- [ ] 扩展 `PendingLine` 保存 `inline_objects`；
- [ ] 添加 `push_inline_math()`；
- [ ] `MarkdownEvent::InlineMath` 使用 `push_inline_math()`；
- [ ] `flush_text()` 把 pending inline objects 转成 `RenderedLine.inline_objects`。

#### Phase 3: TextLayout replacement support

- [ ] 给 `StyledText` / `TextLayout` 增加 inline replacement metrics 支持；
- [ ] replacement width 参与 wrapping；
- [ ] replacement ascent/descent 参与 line metrics；
- [ ] 提供 bounds query API。

这是最可能需要 GPUI maintainer 讨论的部分。PR 描述应避免提「只为 math」，而应说明这是通用 inline replacement object 支持。

#### Phase 4: Paint inline math

- [ ] 根据 replacement bounds 调用 `paint_display_item()`；
- [ ] 默认颜色跟随 current text style；
- [ ] link 范围内公式使用 link color；
- [ ] pending/failed layout 显示原始 `$...$`。

#### Phase 5: Selection/link/copy polish

- [ ] `bounds_for_source_range()` 包含 inline object bounds；
- [ ] click/hover link 包含 formula object；
- [ ] copy selection 保留 `$...$`；
- [ ] inline math inside links 添加 regression test。

### 9.13 社区 PR 拆分建议

为了更容易被 Zed 社区接受，不建议一个 PR 同时实现全部内容。推荐拆分：

1. **GPUI/Markdown: Add inline replacement object support**
   - 不提 math 或只作为 motivating example；
   - 添加 `U+FFFC` replacement metrics、bounds query、tests；
   - 可用 fake inline object 测试 width/baseline/wrapping。

2. **Markdown: Render inline math using replacement objects**
   - 使用 Phase 1/2/4；
   - 复用已有 DisplayList renderer；
   - 添加 `$x^2$`、`text $x^2$ text`、link 内公式测试。

3. **Markdown: Polish inline math selection and copy**
   - 对 selection/link/source mapping 做补充；
   - 添加 copy regression tests。

这样每个 PR 的价值独立，维护者也更容易 review。

### 9.14 验收标准

长期方案完成后应满足：

- `text $x^2$ text` 在同一行内显示，baseline 与文字自然对齐；
- inline formula 宽度参与换行，不溢出、不随机断裂；
- display math 现有居中 block 行为不回归；
- 深色主题下公式颜色跟随 Markdown 文本；
- `\color{red}{x}` 等显式颜色不被默认文本色覆盖；
- `[$x^2$](url)` 可点击，hover/cursor/link color 正常；
- selection 覆盖 inline formula 时复制得到 `$x^2$`；
- 大量重复 inline formula 不触发重复 parse/layout，滚动时主要走 glyph atlas；
- `cargo check -p markdown`、相关 GPUI text layout tests 通过。

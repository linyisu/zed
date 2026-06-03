use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use collections::BTreeMap;
use gpui::{
    AppContext, Bounds, Context, FillOptions, FillRule, Hsla, PathBuilder, PathStyle, Pixels, Rgba,
    SharedString, Task, TextRun, TextSystem, Window, canvas, fill, font, px,
};
use ratex_font::{FontId, katex_ttf_glyph_char};
use ratex_layout::LayoutOptions;
use ratex_types::{Color as RatexColor, DisplayItem, DisplayList, MathStyle, PathCommand};

use anyhow::Result;
use ui::{AnyElement, IntoElement, ParentElement, Styled, div};

use crate::parser::MarkdownEvent;
use crate::{Markdown, MarkdownStyle, ParsedMarkdown};

type MathExpressionCache = HashMap<ParsedMathExpressionContents, Arc<CachedMathExpression>>;

static FONTS_REGISTERED: OnceLock<()> = OnceLock::new();

const KATEX_FONTS: &[(FontId, &str)] = &[
    (FontId::MainRegular, "KaTeX_Main-Regular.ttf"),
    (FontId::MainBold, "KaTeX_Main-Bold.ttf"),
    (FontId::MainItalic, "KaTeX_Main-Italic.ttf"),
    (FontId::MainBoldItalic, "KaTeX_Main-BoldItalic.ttf"),
    (FontId::MathItalic, "KaTeX_Math-Italic.ttf"),
    (FontId::MathBoldItalic, "KaTeX_Math-BoldItalic.ttf"),
    (FontId::AmsRegular, "KaTeX_AMS-Regular.ttf"),
    (FontId::CaligraphicRegular, "KaTeX_Caligraphic-Regular.ttf"),
    (FontId::FrakturRegular, "KaTeX_Fraktur-Regular.ttf"),
    (FontId::FrakturBold, "KaTeX_Fraktur-Bold.ttf"),
    (FontId::SansSerifRegular, "KaTeX_SansSerif-Regular.ttf"),
    (FontId::SansSerifBold, "KaTeX_SansSerif-Bold.ttf"),
    (FontId::SansSerifItalic, "KaTeX_SansSerif-Italic.ttf"),
    (FontId::ScriptRegular, "KaTeX_Script-Regular.ttf"),
    (FontId::TypewriterRegular, "KaTeX_Typewriter-Regular.ttf"),
    (FontId::Size1Regular, "KaTeX_Size1-Regular.ttf"),
    (FontId::Size2Regular, "KaTeX_Size2-Regular.ttf"),
    (FontId::Size3Regular, "KaTeX_Size3-Regular.ttf"),
    (FontId::Size4Regular, "KaTeX_Size4-Regular.ttf"),
];

fn katex_gpui_font(font_name: &str) -> gpui::Font {
    match font_name {
        "Main-Bold" => font("KaTeX_Main").bold(),
        "Main-Italic" => font("KaTeX_Main").italic(),
        "Main-BoldItalic" => font("KaTeX_Main").bold().italic(),
        "Math-Italic" => font("KaTeX_Math").italic(),
        "Math-BoldItalic" => font("KaTeX_Math").bold().italic(),
        "AMS-Regular" => font("KaTeX_AMS"),
        "Caligraphic-Regular" => font("KaTeX_Caligraphic"),
        "Fraktur-Regular" => font("KaTeX_Fraktur"),
        "Fraktur-Bold" => font("KaTeX_Fraktur").bold(),
        "SansSerif-Regular" => font("KaTeX_SansSerif"),
        "SansSerif-Bold" => font("KaTeX_SansSerif").bold(),
        "SansSerif-Italic" => font("KaTeX_SansSerif").italic(),
        "Script-Regular" => font("KaTeX_Script"),
        "Typewriter-Regular" => font("KaTeX_Typewriter"),
        "Size1-Regular" => font("KaTeX_Size1"),
        "Size2-Regular" => font("KaTeX_Size2"),
        "Size3-Regular" => font("KaTeX_Size3"),
        "Size4-Regular" => font("KaTeX_Size4"),
        _ => font("KaTeX_Main"),
    }
}

fn is_system_fallback_font(font_id: FontId) -> bool {
    matches!(
        font_id,
        FontId::CjkRegular | FontId::CjkFallback | FontId::EmojiFallback
    )
}

fn register_katex_fonts(text_system: &TextSystem) {
    let fonts = KATEX_FONTS
        .iter()
        .filter_map(|(_, filename)| ratex_katex_fonts::ttf_bytes(filename))
        .collect::<Vec<_>>();

    if fonts.is_empty() {
        return;
    }

    if let Err(error) = text_system.add_fonts(fonts) {
        log::debug!("failed to register KaTeX fonts: {error}");
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedMathExpression {
    pub(crate) content_range: Range<usize>,
    pub(crate) contents: ParsedMathExpressionContents,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ParsedMathExpressionContents {
    pub(crate) contents: SharedString,
    pub(crate) display_mode: bool,
}

#[derive(Default, Clone)]
pub(crate) struct MathState {
    cache: MathExpressionCache,
    order: Vec<ParsedMathExpressionContents>,
}

impl MathState {
    pub(crate) fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
    }

    fn get_fallback(
        idx: usize,
        old_order: &[ParsedMathExpressionContents],
        new_order_len: usize,
        cache: &MathExpressionCache,
    ) -> Option<Arc<DisplayList>> {
        if old_order.len() != new_order_len {
            return None;
        }

        old_order.get(idx).and_then(|old_content| {
            cache.get(old_content).and_then(|old_cached| {
                old_cached
                    .display_list
                    .get()
                    .and_then(|result| result.as_ref().ok().cloned())
                    .or_else(|| old_cached.fallback.clone())
            })
        })
    }

    pub(crate) fn update(&mut self, parsed: &ParsedMarkdown, cx: &mut Context<Markdown>) {
        FONTS_REGISTERED.get_or_init(|| {
            register_katex_fonts(cx.text_system());
        });
        let mut new_order = Vec::new();
        for math_expression in parsed.math_expressions.values() {
            new_order.push(math_expression.contents.clone());
        }

        for (idx, new_content) in new_order.iter().enumerate() {
            if !self.cache.contains_key(new_content) {
                let fallback = Self::get_fallback(idx, &self.order, new_order.len(), &self.cache);
                self.cache.insert(
                    new_content.clone(),
                    Arc::new(CachedMathExpression::new(new_content.clone(), fallback, cx)),
                );
            }
        }

        let new_order_set: std::collections::HashSet<_> = new_order.iter().cloned().collect();
        self.cache
            .retain(|content, _| new_order_set.contains(content));
        self.order = new_order;
    }
}

fn parse_and_layout(latex: &str, display_mode: bool) -> Result<Arc<DisplayList>> {
    let ast =
        ratex_parser::parse(latex).map_err(|err| anyhow::anyhow!("ratex parse error: {err}"))?;
    let options = LayoutOptions {
        style: if display_mode {
            MathStyle::Display
        } else {
            MathStyle::Text
        },
        color: RatexColor::BLACK,
        ..LayoutOptions::default()
    };
    let layout_box = ratex_layout::layout(&ast, &options);
    let display_list = ratex_layout::to_display_list(&layout_box);
    Ok(Arc::new(display_list))
}

struct CachedMathExpression {
    display_list: Arc<OnceLock<anyhow::Result<Arc<DisplayList>>>>,
    fallback: Option<Arc<DisplayList>>,
    _task: Task<()>,
}

impl CachedMathExpression {
    fn new(
        contents: ParsedMathExpressionContents,
        fallback: Option<Arc<DisplayList>>,
        cx: &mut Context<Markdown>,
    ) -> Self {
        let display_list = Arc::new(OnceLock::<anyhow::Result<Arc<DisplayList>>>::new());
        let display_list_clone = display_list.clone();

        let task = cx.spawn(async move |this, cx| {
            let value = cx
                .background_spawn(async move {
                    parse_and_layout(&contents.contents, contents.display_mode)
                        .map_err(|error| anyhow::anyhow!("{error}"))
                })
                .await;
            let _ = display_list_clone.set(value);
            this.update(cx, |_, cx| {
                cx.notify();
            })
            .ok();
        });

        Self {
            display_list,
            fallback,
            _task: task,
        }
    }
}

pub(crate) fn extract_math_expressions(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
) -> BTreeMap<usize, ParsedMathExpression> {
    let mut math_expressions = BTreeMap::default();

    for (source_range, event) in events {
        let display_mode = match event {
            MarkdownEvent::InlineMath => false,
            MarkdownEvent::DisplayMath => true,
            _ => continue,
        };

        let contents = &source[source_range.clone()];
        let inner = strip_math_delimiters(contents, display_mode);
        if inner.trim().is_empty() {
            continue;
        }
        math_expressions.insert(
            source_range.start,
            ParsedMathExpression {
                content_range: source_range.clone(),
                contents: ParsedMathExpressionContents {
                    contents: inner.into(),
                    display_mode,
                },
            },
        );
    }

    math_expressions
}

fn strip_math_delimiters(tex: &str, display_mode: bool) -> String {
    let s = tex.trim();
    if display_mode {
        s.strip_prefix("$$")
            .and_then(|s| s.strip_suffix("$$"))
            .unwrap_or(s)
    } else {
        s.strip_prefix('$')
            .and_then(|s| s.strip_suffix('$'))
            .unwrap_or(s)
    }
    .to_string()
}

fn ratex_color_to_hsla(color: &RatexColor) -> Hsla {
    Hsla::from(Rgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a,
    })
}

fn resolved_font_matches(window: &Window, font_id: gpui::FontId, expected: &gpui::Font) -> bool {
    window
        .text_system()
        .get_font_for_id(font_id)
        .map(|resolved| {
            resolved.family == expected.family
                && resolved.weight == expected.weight
                && resolved.style == expected.style
        })
        .unwrap_or(false)
}

fn paint_display_item(
    item: &DisplayItem,
    origin: gpui::Point<Pixels>,
    baseline_y: Pixels,
    font_size: Pixels,
    window: &mut Window,
) {
    match item {
        DisplayItem::GlyphPath {
            x,
            y,
            scale,
            font,
            char_code,
            color,
        } => {
            let x_px = *x as f32 * font_size;
            let y_px = *y as f32 * font_size;
            let mut origin = gpui::point(origin.x + x_px, origin.y + baseline_y - y_px);
            let em = font_size * *scale as f32;

            let font_id = FontId::parse(font).unwrap_or(FontId::MainRegular);
            let (ch, font, require_katex_font) = if is_system_fallback_font(font_id) {
                let Some(ch) = char::from_u32(*char_code) else {
                    return;
                };
                (ch, window.text_style().font(), false)
            } else {
                let ch = katex_ttf_glyph_char(font_id, *char_code);
                (ch, katex_gpui_font(font), true)
            };

            let text = ch.to_string();
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: ratex_color_to_hsla(color),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(text.into(), em, &[run], None);
            let Some(run) = line.runs.first() else {
                return;
            };
            if require_katex_font && !resolved_font_matches(window, run.font_id, &font) {
                return;
            }
            let Some(glyph) = run.glyphs.first() else {
                return;
            };
            origin = origin + gpui::point(glyph.position.x, px(0.));
            //NOTE: We ignore glyph painting errors here, as they can be caused by invalid font glyphs, and we don't want to crash the entire rendering because of that.
            if glyph.is_emoji {
                window
                    .paint_emoji(origin, run.font_id, glyph.id, em)
                    .unwrap();
            } else {
                window
                    .paint_glyph(
                        origin,
                        run.font_id,
                        glyph.id,
                        em,
                        ratex_color_to_hsla(color),
                    )
                    .unwrap();
            }
        }

        DisplayItem::Line {
            x,
            y,
            width,
            thickness,
            color,
            dashed,
        } => {
            let x_px = *x as f32 * font_size;
            let y_px = *y as f32 * font_size;
            let line_origin = gpui::point(origin.x + x_px, origin.y + baseline_y - y_px);
            if *dashed {
                let end = gpui::point(line_origin.x + *width as f32 * font_size, line_origin.y);
                let thickness_px = px((*thickness as f32 * font_size.as_f32()).max(1.0));
                let mut builder = PathBuilder::stroke(thickness_px).dash_array(&[px(4.), px(2.)]);
                builder.move_to(line_origin);
                builder.line_to(end);
                if let Ok(path) = builder.build() {
                    window.paint_path(path, ratex_color_to_hsla(color));
                }
            }
            let bounds = Bounds::new(
                line_origin,
                gpui::size(*width as f32 * font_size, *thickness as f32 * font_size),
            );
            window.paint_quad(fill(bounds, ratex_color_to_hsla(color)));
        }
        DisplayItem::Rect {
            x,
            y,
            width,
            height,
            color,
        } => {
            let x_px = *x as f32 * font_size;
            let y_px = *y as f32 * font_size;
            let rect_origin = gpui::point(origin.x + x_px, origin.y + baseline_y - y_px);
            let bounds = Bounds::new(
                rect_origin,
                gpui::size(*width as f32 * font_size, *height as f32 * font_size),
            );
            window.paint_quad(fill(bounds, ratex_color_to_hsla(color)));
        }
        DisplayItem::Path {
            x,
            y,
            commands,
            fill,
            color,
        } => {
            let x_px = *x as f32 * font_size;
            let y_px = *y as f32 * font_size;
            let path_offset = gpui::point(origin.x + x_px, origin.y + baseline_y - y_px);
            let mut builder = if *fill {
                PathBuilder::fill().with_style(PathStyle::Fill(
                    FillOptions::default().with_fill_rule(FillRule::EvenOdd),
                ))
            } else {
                // NOTE: define const
                PathBuilder::stroke(px(1.5))
            };
            for cmd in commands {
                match cmd {
                    PathCommand::MoveTo { x, y } => {
                        let to =
                            path_offset + gpui::point(*x as f32 * font_size, *y as f32 * font_size);
                        builder.move_to(to);
                    }
                    PathCommand::LineTo { x, y } => {
                        let to =
                            path_offset + gpui::point(*x as f32 * font_size, *y as f32 * font_size);
                        builder.line_to(to);
                    }
                    PathCommand::QuadTo { x1, y1, x, y } => {
                        let ctrl = path_offset
                            + gpui::point(*x1 as f32 * font_size, *y1 as f32 * font_size);
                        let to =
                            path_offset + gpui::point(*x as f32 * font_size, *y as f32 * font_size);
                        builder.curve_to(to, ctrl);
                    }
                    PathCommand::CubicTo {
                        x1,
                        y1,
                        x2,
                        y2,
                        x,
                        y,
                    } => {
                        let ctrl1 = path_offset
                            + gpui::point(*x1 as f32 * font_size, *y1 as f32 * font_size);
                        let ctrl2 = path_offset
                            + gpui::point(*x2 as f32 * font_size, *y2 as f32 * font_size);
                        let to =
                            path_offset + gpui::point(*x as f32 * font_size, *y as f32 * font_size);
                        builder.cubic_bezier_to(to, ctrl1, ctrl2);
                    }
                    PathCommand::Close => {}
                }
            }
            //NOTE: We ignore path building errors here, as they can be caused by invalid font glyphs, and we don't want to crash the entire rendering because of that.
            let path = builder.build().unwrap();
            window.paint_path(path, ratex_color_to_hsla(color));
        }
    }
}

pub(crate) fn render_math_expression(
    expr: &ParsedMathExpression,
    math_state: &MathState,
    font_size: Pixels,
    _style: &MarkdownStyle,
) -> AnyElement {
    let cached = math_state.cache.get(&expr.contents);
    let display_list = cached.and_then(|c| c.display_list.get()?.as_ref().ok());

    match display_list {
        Some(dl) => {
            let total_height = dl.height + dl.depth;
            let width = px(dl.width as f32 * font_size.as_f32());
            let height = px(total_height as f32 * font_size.as_f32());
            let dl = dl.clone();

            canvas(
                move |_bounds, _window, _cx| (dl, font_size),
                move |bounds, (dl, font_size), window, cx| {
                    let baseline_y = px(dl.height as f32 * font_size.as_f32());
                    let text_system = cx.text_system().clone();
                    for item in &dl.items {
                        paint_display_item(item, bounds.origin, baseline_y, font_size, window);
                    }
                },
            )
            .w(width)
            .h(height)
            .into_any_element()
        }
        None => {
            // 加载中 / 失败 → 显示原始 LaTeX
            div()
                .child(SharedString::from(format!(
                    "{}{}{}",
                    if expr.contents.display_mode {
                        "$$"
                    } else {
                        "$"
                    },
                    expr.contents.contents,
                    if expr.contents.display_mode {
                        "$$"
                    } else {
                        "$"
                    }
                )))
                .into_any_element()
        }
    }
}

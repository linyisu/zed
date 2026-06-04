# Inline math progress note

## Current state

This branch has completed the foundation for Markdown inline math replacement objects, but inline math rendering is not enabled yet.

Implemented pieces:

- `crates/markdown/src/math.rs`
  - Added `MathLayoutMetrics { width, ascent, descent }`.
  - Added `display_list_metrics(...)` and `math_layout_metrics(...)` so RaTeX display lists can report baseline metrics before painting.
  - Display math now reuses the metrics helper.

- `crates/markdown/src/markdown.rs`
  - Added `PendingLine.inline_objects` and `RenderedLine.inline_objects`.
  - Added `PendingInlineObject`, `RenderedInlineObject`, and `InlineObjectKind::Math`.
  - Added `INLINE_OBJECT_REPLACEMENT` (`U+FFFC`).
  - Added `push_inline_math(...)`, which inserts the placeholder and records source range, expression, and metrics.
  - `MarkdownEvent::InlineMath` is not wired to this method yet.

- `crates/gpui/src/text_system/line_layout.rs`
  - Added generic `InlineReplacement` in the text-system layer.
  - Added `layout_wrapped_line_with_replacements(...)` and `layout_line_with_replacements(...)`.
  - Added replacement-aware layout cache keys.
  - Added `apply_inline_replacements_to_layout(...)`, which adjusts width, later glyph x positions, ascent, and descent.

- `crates/gpui/src/text_system.rs`
  - Added `shape_text_with_replacements(...)`, which splits replacements by line and forwards them to `LineLayoutCache`.
  - Existing `shape_text(...)` delegates with an empty replacement slice.

- `crates/gpui/src/elements/text.rs`
  - `StyledText` now stores `inline_replacements`.
  - Added `StyledText::with_inline_replacements(...)`.
  - `TextLayoutInner` stores replacements.
  - `TextLayout::inline_replacements()` exposes stored replacements.
  - `StyledText` measurement calls `shape_text_with_replacements(...)`.

## Validation

Run and passed:

```sh
cargo fmt --package gpui --package markdown
git diff --check
cargo check -p gpui
cargo check -p markdown
```

## Important limitation / risk

This implementation modifies GPUI's text layout system. The user is concerned this may be the wrong direction because `gpui-component` implements inline math without modifying GPUI.

Before continuing implementation, compare this approach with `gpui-component`:

- `/home/mengh04/Workspace/gpui-component/crates/ui/src/text/inline.rs`
- `/home/mengh04/Workspace/gpui-component/crates/ui/src/text/math.rs`
- `/home/mengh04/Workspace/gpui-component/crates/ui/src/text/node.rs`

Known from earlier investigation: `gpui-component` does not use `StyledText`; it has its own paragraph layout engine with `ParagraphInlineItem::Math`, `U+FFFC`, custom line metrics, and custom painting. It avoids GPUI changes by owning the whole paragraph text layout pipeline.

Question to answer next: for Zed Markdown, is it better to keep adding generic replacement support to GPUI `StyledText`, or to build a Markdown-owned paragraph inline layout similar to gpui-component?

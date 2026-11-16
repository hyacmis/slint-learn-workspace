mod parser;
mod templates;

use crate::{MarkdownBlock, MarkdownPreview};
use parser::{BlockKind, ParsedBlock, parse_blocks};
pub(crate) use templates::insert_template;

/// 正文默认字号，标题按级别放大。
const BODY_FONT_SIZE: f32 = 15.0;
const CODE_FONT_SIZE: f32 = 13.0;
const CODE_FONT_FAMILY: &str = "Consolas";

fn text_color() -> slint::Color {
    slint::Color::from_rgb_u8(0x1f, 0x23, 0x28)
}

fn heading_color() -> slint::Color {
    slint::Color::from_rgb_u8(0x0f, 0x17, 0x1f)
}

fn quote_color() -> slint::Color {
    slint::Color::from_rgb_u8(0x57, 0x60, 0x6a)
}

fn rule_color() -> slint::Color {
    slint::Color::from_rgb_u8(0x8c, 0x95, 0x9f)
}

pub fn render(markdown: slint::SharedString) -> MarkdownPreview {
    let parsed = parse_blocks(&markdown);
    let simplified = parsed
        .iter()
        .any(|block| block_style(block.kind()).simplified);

    let mut errors = Vec::new();
    let blocks: Vec<MarkdownBlock> = parsed
        .iter()
        .map(|block| build_block(block, &mut errors))
        .collect();

    let message = if let Some(error) = errors.first() {
        format!("部分 Markdown 无法解析，已按纯文本显示：{error}")
    } else if blocks.is_empty() {
        "文档为空，输入 Markdown 后会自动渲染".to_string()
    } else if simplified {
        "已渲染：Slint 不支持标题、引用和代码块，预览使用近似样式".to_string()
    } else {
        format!("Markdown 渲染成功，共 {} 个字符", markdown.chars().count())
    };

    MarkdownPreview {
        blocks: slint::ModelRc::new(slint::VecModel::from(blocks)),
        message: message.into(),
        parsed: errors.is_empty(),
    }
}

/// 预览中一个块的近似样式。
struct BlockStyle {
    font_size: f32,
    font_family: &'static str,
    color: slint::Color,
    indent: f32,
    /// 是否使用了 Slint 不支持、只能近似的块级语法。
    simplified: bool,
}

fn block_style(kind: BlockKind) -> BlockStyle {
    match kind {
        BlockKind::Heading(level) => BlockStyle {
            font_size: heading_font_size(level),
            font_family: "",
            color: heading_color(),
            indent: 0.0,
            simplified: true,
        },
        BlockKind::Paragraph => BlockStyle {
            font_size: BODY_FONT_SIZE,
            font_family: "",
            color: text_color(),
            indent: 0.0,
            simplified: false,
        },
        BlockKind::List => BlockStyle {
            font_size: BODY_FONT_SIZE,
            font_family: "",
            color: text_color(),
            indent: 6.0,
            simplified: false,
        },
        BlockKind::Quote => BlockStyle {
            font_size: BODY_FONT_SIZE,
            font_family: "",
            color: quote_color(),
            indent: 10.0,
            simplified: true,
        },
        BlockKind::Code => BlockStyle {
            font_size: CODE_FONT_SIZE,
            font_family: CODE_FONT_FAMILY,
            color: text_color(),
            indent: 10.0,
            simplified: true,
        },
        BlockKind::Rule => BlockStyle {
            font_size: BODY_FONT_SIZE,
            font_family: "",
            color: rule_color(),
            indent: 0.0,
            simplified: true,
        },
    }
}

#[cfg(test)]
fn preview_blocks(markdown: &str) -> (Vec<MarkdownBlock>, Vec<String>) {
    let mut errors = Vec::new();
    let blocks = parse_blocks(markdown)
        .iter()
        .map(|block| build_block(block, &mut errors))
        .collect();
    (blocks, errors)
}

fn build_block(block: &ParsedBlock, errors: &mut Vec<String>) -> MarkdownBlock {
    let style = block_style(block.kind());
    let text = match block.kind() {
        BlockKind::Rule => slint::StyledText::from_plain_text("────────────────────────────────"),
        BlockKind::Code => code_text(&block.source),
        BlockKind::Heading(_) => heading_text(&block.source, errors),
        BlockKind::Paragraph | BlockKind::List | BlockKind::Quote => {
            markdown_text(&block.source, errors)
        }
    };

    MarkdownBlock {
        text,
        font_size: style.font_size,
        font_family: style.font_family.into(),
        color: style.color,
        indent: style.indent,
    }
}

/// 代码块按纯文本渲染，避免代码里的 *、_、反引号被当成 Markdown。
fn code_text(source: &str) -> slint::StyledText {
    if source.is_empty() {
        return slint::StyledText::from_plain_text(" ");
    }
    slint::StyledText::from_plain_text(source)
}

/// Slint 不支持标题语法，用“加粗 + 放大字号”近似；加粗失败时退回普通文本。
fn heading_text(source: &str, errors: &mut Vec<String>) -> slint::StyledText {
    if source.trim().is_empty() {
        return slint::StyledText::from_plain_text(" ");
    }
    match slint::StyledText::from_markdown(&format!("**{source}**")) {
        Ok(text) => text,
        Err(_) => markdown_text(source, errors),
    }
}

fn markdown_text(source: &str, errors: &mut Vec<String>) -> slint::StyledText {
    if source.trim().is_empty() {
        return slint::StyledText::from_plain_text(" ");
    }
    match slint::StyledText::from_markdown(source) {
        Ok(text) => text,
        Err(error) => {
            errors.push(error.to_string());
            slint::StyledText::from_plain_text(source)
        }
    }
}

fn heading_font_size(level: usize) -> f32 {
    match level {
        1 => 30.0,
        2 => 26.0,
        3 => 22.0,
        4 => 20.0,
        5 => 18.0,
        _ => 16.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MarkdownOperation;
    use slint::Model as _;

    const DEFAULT_DOCUMENT: &str = "# Markdown 编辑器\n\n使用上方工具插入常见 **Markdown** 格式，右侧会实时渲染。\n\n- 支持 **粗体**、*Italic / 斜体*、\x60行内代码\x60\n- 支持列表、引用和代码块\n\n> 引用内容会用灰色文字近似显示。";

    #[test]
    fn renders_default_document() {
        let (blocks, errors) = preview_blocks(DEFAULT_DOCUMENT);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!blocks.is_empty());
    }

    #[test]
    fn preserves_native_italic_markdown_formatting() {
        let source = "*Italic text / 斜体文本*";
        let (blocks, errors) = preview_blocks(source);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].text,
            slint::StyledText::from_markdown(source).unwrap()
        );
    }

    #[test]
    fn renders_all_toolbar_templates() {
        for operation in [
            MarkdownOperation::HeadingOne,
            MarkdownOperation::HeadingTwo,
            MarkdownOperation::Bold,
            MarkdownOperation::Italic,
            MarkdownOperation::InlineCode,
            MarkdownOperation::Link,
            MarkdownOperation::UnorderedList,
            MarkdownOperation::OrderedList,
            MarkdownOperation::Quote,
            MarkdownOperation::CodeBlock,
        ] {
            let preview = render(templates::template(operation).into());
            assert!(preview.parsed, "{operation:?}: {}", preview.message);
        }
    }

    #[test]
    fn renders_combined_templates() {
        let combined = [
            MarkdownOperation::HeadingOne,
            MarkdownOperation::HeadingTwo,
            MarkdownOperation::Bold,
            MarkdownOperation::Italic,
            MarkdownOperation::InlineCode,
            MarkdownOperation::Link,
            MarkdownOperation::UnorderedList,
            MarkdownOperation::OrderedList,
            MarkdownOperation::Quote,
            MarkdownOperation::CodeBlock,
        ]
        .into_iter()
        .map(templates::template)
        .collect::<Vec<_>>()
        .join("\n");

        let preview = render(combined.into());
        assert!(preview.parsed, "{}", preview.message);
    }

    #[test]
    fn heading_levels_use_distinct_font_sizes() {
        let (blocks, errors) =
            preview_blocks("# First\n## Second\n### Third\n#### Fourth\n##### Fifth\n###### Sixth");

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.font_size)
                .collect::<Vec<_>>(),
            vec![30.0, 26.0, 22.0, 20.0, 18.0, 16.0]
        );
    }

    #[test]
    fn code_blocks_keep_their_text_and_use_monospace_font() {
        let (blocks, errors) = preview_blocks("\x60\x60\x60rust\nlet x = *ptr;\n\x60\x60\x60");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].font_family, "Consolas");
    }

    #[test]
    fn empty_document_renders_nothing() {
        let preview = render("".into());
        assert!(preview.parsed);
        assert_eq!(preview.blocks.row_count(), 0);
        assert!(preview.message.contains("文档为空"));
    }
}

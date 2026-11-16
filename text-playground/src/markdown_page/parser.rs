/// Markdown 的块级语法。行内样式留给 `StyledText::from_markdown` 解析。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockKind {
    Heading(usize),
    Paragraph,
    List,
    Quote,
    Code,
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedBlock {
    kind: BlockKind,
    /// 交给 StyledText 解析的 Markdown 源码；代码块中则是原始内容。
    pub(super) source: String,
}

impl ParsedBlock {
    pub(super) fn kind(&self) -> BlockKind {
        self.kind
    }

    fn new(kind: BlockKind, source: impl Into<String>) -> Self {
        Self {
            kind,
            source: source.into(),
        }
    }
}

pub(super) fn parse_blocks(markdown: &str) -> Vec<ParsedBlock> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            index += 1;
            continue;
        }

        if let Some(fence) = opening_fence(trimmed) {
            index += 1;
            let mut content = Vec::new();
            while index < lines.len() {
                if closing_fence(lines[index].trim_start(), fence) {
                    index += 1;
                    break;
                }
                content.push(lines[index]);
                index += 1;
            }
            blocks.push(ParsedBlock::new(BlockKind::Code, content.join("\n")));
            continue;
        }

        if let Some((level, content)) = atx_heading(trimmed) {
            blocks.push(ParsedBlock::new(BlockKind::Heading(level), content));
            index += 1;
            continue;
        }

        if is_thematic_break(trimmed) {
            blocks.push(ParsedBlock::new(BlockKind::Rule, ""));
            index += 1;
            continue;
        }

        if trimmed.starts_with('>') {
            let mut content = Vec::new();
            while index < lines.len() {
                let current = lines[index].trim();
                let Some(quoted) = current.strip_prefix('>') else {
                    break;
                };
                content.push(quoted.strip_prefix(' ').unwrap_or(quoted));
                index += 1;
            }
            let source = content
                .iter()
                .map(|line| format!("│ {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            blocks.push(ParsedBlock::new(BlockKind::Quote, source));
            continue;
        }

        if is_list_item(trimmed) {
            let mut content = Vec::new();
            while index < lines.len() {
                let current = lines[index];
                let current_trimmed = current.trim();
                if current_trimmed.is_empty() {
                    break;
                }
                let continuation = is_list_item(current_trimmed)
                    || current.starts_with(' ')
                    || current.starts_with('\t');
                if !continuation {
                    break;
                }
                content.push(current.trim_end());
                index += 1;
            }
            blocks.push(ParsedBlock::new(BlockKind::List, content.join("\n")));
            continue;
        }

        let mut content = Vec::new();
        while index < lines.len() {
            let current = lines[index];
            let current_trimmed = current.trim();
            if current_trimmed.is_empty()
                || opening_fence(current_trimmed).is_some()
                || atx_heading(current_trimmed).is_some()
                || is_thematic_break(current_trimmed)
                || (!content.is_empty() && setext_heading_level(current_trimmed).is_some())
                || current_trimmed.starts_with('>')
                || is_list_item(current_trimmed)
            {
                break;
            }
            content.push(current.trim_end());
            index += 1;
        }

        if !content.is_empty()
            && index < lines.len()
            && let Some(level) = setext_heading_level(lines[index].trim())
        {
            blocks.push(ParsedBlock::new(
                BlockKind::Heading(level),
                content.join("\n"),
            ));
            index += 1;
            continue;
        }
        blocks.push(ParsedBlock::new(BlockKind::Paragraph, content.join("\n")));
    }

    blocks
}

fn setext_heading_level(line: &str) -> Option<usize> {
    let marker = line.chars().find(|ch| !ch.is_whitespace())?;
    let level = match marker {
        '=' => 1,
        '-' => 2,
        _ => return None,
    };
    line.chars()
        .all(|ch| ch.is_whitespace() || ch == marker)
        .then_some(level)
}

fn opening_fence(trimmed: &str) -> Option<(char, usize)> {
    let fence_char = trimmed.chars().next()?;
    if fence_char != '\x60' && fence_char != '~' {
        return None;
    }
    let fence_len = trimmed.chars().take_while(|ch| *ch == fence_char).count();
    (fence_len >= 3).then_some((fence_char, fence_len))
}

fn closing_fence(trimmed: &str, (fence_char, fence_len): (char, usize)) -> bool {
    let run = trimmed.chars().take_while(|ch| *ch == fence_char).count();
    run >= fence_len && trimmed[run..].trim().is_empty()
}

fn atx_heading(line: &str) -> Option<(usize, String)> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some((level, strip_closing_hashes(rest.trim()).to_string()))
}

fn strip_closing_hashes(content: &str) -> &str {
    let without_hashes = content.trim_end_matches('#');
    if without_hashes.len() == content.len() {
        return content;
    }
    if without_hashes.is_empty() {
        return "";
    }
    if without_hashes.ends_with(' ') || without_hashes.ends_with('\t') {
        without_hashes.trim_end()
    } else {
        content
    }
}

fn is_thematic_break(line: &str) -> bool {
    let mut chars = line.chars().filter(|ch| !ch.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let marker_count = 1 + chars.filter(|ch| *ch == first).count();
    marker_count >= 3 && line.chars().all(|ch| ch.is_whitespace() || ch == first)
}

fn is_list_item(trimmed: &str) -> bool {
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return true;
    }

    let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return false;
    }
    let rest = &trimmed[digits..];
    rest.strip_prefix('.')
        .or_else(|| rest.strip_prefix(')'))
        .is_some_and(|remainder| remainder.starts_with(' '))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(markdown: &str) -> Vec<BlockKind> {
        parse_blocks(markdown)
            .iter()
            .map(ParsedBlock::kind)
            .collect()
    }

    #[test]
    fn splits_block_level_syntax() {
        let markdown = "# Title\n\nfirst line\nsecond line\n\n- one\n- two\n\n> quoted\n\n\x60\x60\x60text\nfn main() {}\n\x60\x60\x60\n\n---";
        assert_eq!(
            kinds(markdown),
            vec![
                BlockKind::Heading(1),
                BlockKind::Paragraph,
                BlockKind::List,
                BlockKind::Quote,
                BlockKind::Code,
                BlockKind::Rule,
            ]
        );

        let blocks = parse_blocks(markdown);
        assert_eq!(blocks[1].source, "first line\nsecond line");
        assert_eq!(blocks[4].source, "fn main() {}");
    }

    #[test]
    fn recognizes_setext_headings_without_confusing_thematic_breaks() {
        let blocks = parse_blocks("First level\n===\n\nSecond level\n---\n\nParagraph\n\n---");
        assert_eq!(
            blocks.iter().map(ParsedBlock::kind).collect::<Vec<_>>(),
            vec![
                BlockKind::Heading(1),
                BlockKind::Heading(2),
                BlockKind::Paragraph,
                BlockKind::Rule,
            ]
        );
        assert_eq!(blocks[0].source, "First level");
        assert_eq!(blocks[1].source, "Second level");
    }

    #[test]
    fn keeps_unterminated_code_block_content() {
        let blocks = parse_blocks("\x60\x60\x60rust\nfn main() {}\n");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind(), BlockKind::Code);
        assert_eq!(blocks[0].source, "fn main() {}");
    }
}

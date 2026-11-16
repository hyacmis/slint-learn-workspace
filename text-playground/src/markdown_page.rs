use crate::{MarkdownActionResult, MarkdownOperation};

pub fn changed_message(text: &str) -> String {
    format!("Markdown 字符数：{}", text.chars().count())
}

pub fn insert_template(operation: MarkdownOperation) -> MarkdownActionResult {
    MarkdownActionResult {
        template: template(operation).into(),
        message: format!("已添加 {} 模板", operation_name(operation)).into(),
    }
}

fn template(operation: MarkdownOperation) -> &'static str {
    match operation {
        MarkdownOperation::HeadingOne => "# 一级标题\n",
        MarkdownOperation::HeadingTwo => "## 二级标题\n",
        MarkdownOperation::Bold => "**粗体文本**\n",
        MarkdownOperation::Italic => "*斜体文本*\n",
        MarkdownOperation::InlineCode => "`代码`\n",
        MarkdownOperation::Link => "[链接文字](https://example.com)\n",
        MarkdownOperation::UnorderedList => "- 列表项一\n- 列表项二\n",
        MarkdownOperation::OrderedList => "1. 列表项一\n2. 列表项二\n",
        MarkdownOperation::Quote => "> 引用内容\n",
        MarkdownOperation::CodeBlock => "```text\n代码块\n```\n",
    }
}

fn operation_name(operation: MarkdownOperation) -> &'static str {
    match operation {
        MarkdownOperation::HeadingOne => "一级标题",
        MarkdownOperation::HeadingTwo => "二级标题",
        MarkdownOperation::Bold => "粗体",
        MarkdownOperation::Italic => "斜体",
        MarkdownOperation::InlineCode => "行内代码",
        MarkdownOperation::Link => "链接",
        MarkdownOperation::UnorderedList => "无序列表",
        MarkdownOperation::OrderedList => "有序列表",
        MarkdownOperation::Quote => "引用",
        MarkdownOperation::CodeBlock => "代码块",
    }
}

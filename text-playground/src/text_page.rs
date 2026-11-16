use crate::{TextActionResult, TextMetrics, TextOperation};

pub fn metrics(text: &str) -> TextMetrics {
    TextMetrics {
        character_count: text.chars().count() as i32,
        word_count: text.split_whitespace().count() as i32,
        line_count: text.lines().count() as i32,
        byte_count: text.len() as i32,
    }
}

pub fn transform(text: &str, operation: TextOperation) -> TextActionResult {
    let transformed = match operation {
        TextOperation::Uppercase => text.to_uppercase(),
        TextOperation::Lowercase => text.to_lowercase(),
        TextOperation::Trim => text.trim().to_string(),
        TextOperation::Reverse => text.chars().rev().collect(),
        TextOperation::RemoveWhitespace => text.chars().filter(|ch| !ch.is_whitespace()).collect(),
    };

    TextActionResult {
        text: transformed.into(),
        message: format!("已完成：{}", operation_name(operation)).into(),
    }
}

pub fn replace(text: &str, find: &str, replacement: &str) -> TextActionResult {
    if find.is_empty() {
        return TextActionResult {
            text: text.into(),
            message: "请输入要查找的内容".into(),
        };
    }

    let count = text.matches(find).count();
    TextActionResult {
        text: text.replace(find, replacement).into(),
        message: format!("已替换 {count} 处").into(),
    }
}

fn operation_name(operation: TextOperation) -> &'static str {
    match operation {
        TextOperation::Uppercase => "转换为大写",
        TextOperation::Lowercase => "转换为小写",
        TextOperation::Trim => "去除首尾空白",
        TextOperation::Reverse => "反转文本",
        TextOperation::RemoveWhitespace => "删除所有空白",
    }
}

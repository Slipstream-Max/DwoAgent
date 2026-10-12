//! 跨分段的代码围栏平衡（Telegram / 飞书共用）。
//!
//! 渠道出站策略（2026-10-12 定稿 —— markdown 原文是唯一事实来源，不做转换）：
//! - Telegram：正文原样走 `sendRichMessage`（Rich Markdown，原生标题 / 表格 /
//!   引用 / 列表 / 公式）。
//! - 飞书：正文原样包进交互卡片的 markdown 元素。
//! - QQ：原样走官方 markdown 富文本（msg_type 2）。
//! - 微信：原样直出纯文本。
//!
//! 本模块只剩「分段后把围栏补平」这一个职责：切分在字符界，围栏代码块可能被
//! 切在两半 —— 补成每段自身闭合，客户端才会把这一段正确渲染成代码块。

/// 跨 chunk 保持的代码围栏状态。
#[derive(Debug, Clone, Default)]
pub struct FenceState {
    open: Option<FenceMarker>,
}

/// 描述一个已打开的围栏（定界符、长度与语言）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FenceMarker {
    ch: char,
    len: usize,
    lang: String,
}

/// chunk 从围栏中间开始时补开头，结束时补结尾。
///
/// 返回的片段自身围栏闭合；跨 chunk 的语义由 `state` 记住。
pub fn balance_fences(chunk: &str, state: &mut FenceState) -> String {
    let mut out = String::new();
    if let Some(marker) = state.open.clone() {
        out.push_str(&fence_line(&marker));
        out.push('\n');
    }
    let mut first = true;
    for line in chunk.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        if let Some(marker) = state.open.clone() {
            if is_fence_close(line, &marker) {
                state.open = None;
            }
        } else if let Some(marker) = parse_fence_open(line) {
            state.open = Some(marker);
        }
        out.push_str(line);
    }
    if let Some(marker) = state.open.clone() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&marker.ch.to_string().repeat(marker.len));
    }
    out
}

/// 识别围栏起始行（4 空格缩进按缩进代码块处理，不在这里管）。
fn parse_fence_open(line: &str) -> Option<FenceMarker> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|other| *other == ch).count();
    if len < 3 {
        return None;
    }
    let rest: String = trimmed.chars().skip(len).collect();
    if rest.contains(ch) {
        return None;
    }
    Some(FenceMarker {
        ch,
        len,
        lang: rest.trim().to_string(),
    })
}

fn is_fence_close(line: &str, marker: &FenceMarker) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let count = trimmed
        .chars()
        .take_while(|other| *other == marker.ch)
        .count();
    count >= marker.len && trimmed.chars().all(|other| other == marker.ch)
}

fn fence_line(marker: &FenceMarker) -> String {
    let mut line = marker.ch.to_string().repeat(marker.len);
    line.push_str(&marker.lang);
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balances_fences_across_chunks() {
        let mut state = FenceState::default();
        let first = balance_fences("```python\nprint(1)", &mut state);
        assert_eq!(first, "```python\nprint(1)\n```");
        let second = balance_fences("print(2)\n```", &mut state);
        assert_eq!(second, "```python\nprint(2)\n```");
        assert!(state.open.is_none());
    }

    #[test]
    fn plain_text_passes_through() {
        let mut state = FenceState::default();
        assert_eq!(balance_fences("**hi**\n\n", &mut state), "**hi**\n\n");
    }
}

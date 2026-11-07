use unicode_properties::emoji::UnicodeEmoji;
use unicode_segmentation::UnicodeSegmentation;

pub fn text_run(input: &str) -> std::borrow::Cow<'_, str> {
    if input.contains('\u{fe0e}') {
        std::borrow::Cow::Owned(input.replace('\u{fe0e}', ""))
    } else {
        std::borrow::Cow::Borrowed(input)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    Text(&'a str),
    Emoji(&'a str),
    Fallback(&'a str),
}

fn is_emoji_cluster(cluster: &str) -> bool {
    if cluster.contains('\u{fe0e}') {
        return false;
    }
    let chars: Vec<char> = cluster.chars().collect();
    if chars
        .iter()
        .any(|ch| matches!(*ch as u32, 0x1f1e6..=0x1f1ff))
    {
        return chars.len() == 2
            && chars
                .iter()
                .all(|ch| matches!(*ch as u32, 0x1f1e6..=0x1f1ff));
    }
    if chars
        .first()
        .is_some_and(|ch| ch.is_ascii_digit() || matches!(ch, '#' | '*'))
    {
        return chars.iter().any(|ch| *ch == '\u{20e3}');
    }
    let Some(&first) = chars.first() else {
        return false;
    };
    first.is_emoji_char()
        && (cluster.contains('\u{fe0f}')
            || first.emoji_status() != unicode_properties::emoji::EmojiStatus::EmojiOther
                && first.emoji_status()
                    != unicode_properties::emoji::EmojiStatus::EmojiModifierBase)
}

pub fn tokens(input: &str, mut supported: impl FnMut(&str) -> bool) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut text_start = 0;
    for (start, grapheme) in input.grapheme_indices(true) {
        let emoji = is_emoji_cluster(grapheme);
        let is_supported = emoji && supported(grapheme);
        if is_supported || emoji {
            if text_start < start {
                out.push(Token::Text(&input[text_start..start]));
            }
            out.push(if is_supported {
                Token::Emoji(grapheme)
            } else {
                Token::Fallback(grapheme)
            });
            text_start = start + grapheme.len();
        }
    }
    if text_start < input.len() {
        out.push(Token::Text(&input[text_start..]));
    }
    out
}

pub fn layout(
    input: &str,
    max_width: i32,
    space_width: i32,
    mut width: impl FnMut(&str) -> i32,
) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in input.split('\n') {
        let mut line = String::new();
        let mut line_width: i32 = 0;
        for word in paragraph.split_whitespace() {
            let word_width = width(word);
            let separator = i32::from(!line.is_empty()) * space_width;
            if !line.is_empty()
                && line_width
                    .saturating_add(separator)
                    .saturating_add(word_width)
                    > max_width
            {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            if word_width > max_width && max_width >= 0 {
                for grapheme in word.graphemes(true) {
                    let grapheme_width = width(grapheme);
                    if !line.is_empty() && line_width.saturating_add(grapheme_width) > max_width {
                        lines.push(std::mem::take(&mut line));
                        line_width = 0;
                    }
                    line.push_str(grapheme);
                    line_width = line_width.saturating_add(grapheme_width);
                }
            } else {
                if !line.is_empty() {
                    line.push(' ');
                    line_width = line_width.saturating_add(space_width);
                }
                line.push_str(word);
                line_width = line_width.saturating_add(word_width);
            }
        }
        if !line.is_empty() {
            lines.push(line);
        } else if paragraph.is_empty() && !input.is_empty() {
            lines.push(String::new());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_preserves_presentation_and_actual_graphemes() {
        assert_eq!(tokens("⌚", |_| true), vec![Token::Emoji("⌚")]);
        assert_eq!(tokens("☀", |_| true), vec![Token::Text("☀")]);
        assert_eq!(tokens("☀️", |_| true), vec![Token::Emoji("☀️")]);
        assert_eq!(tokens("☀︎", |_| true), vec![Token::Text("☀︎")]);
        assert_eq!(tokens("1️⃣", |_| true), vec![Token::Emoji("1️⃣")]);
        assert_eq!(tokens("©", |_| true), vec![Token::Text("©")]);
        assert_eq!(tokens("©️", |_| true), vec![Token::Emoji("©️")]);
        assert_eq!(tokens("a\u{301}", |_| true), vec![Token::Text("a\u{301}")]);
        assert_eq!(tokens("😀", |_| true), vec![Token::Emoji("😀")]);
    }

    #[test]
    fn layout_and_truncation_keep_cluster_boundaries_and_blank_lines() {
        assert_eq!(
            layout("a\n\nb", 20, 1, |s| s.len() as i32),
            vec!["a", "", "b"]
        );
        let input = "a1️⃣b";
        let end = truncated_prefix_end(input, |s| s.len() <= "a1️⃣".len());
        assert_eq!(&input[..end], "a1️⃣");
    }
}

pub fn truncated_prefix_end(input: &str, mut fits: impl FnMut(&str) -> bool) -> usize {
    let mut end = 0;
    for (start, grapheme) in input.grapheme_indices(true) {
        let next = start + grapheme.len();
        if !fits(&input[..next]) {
            break;
        }
        end = next;
    }
    end
}

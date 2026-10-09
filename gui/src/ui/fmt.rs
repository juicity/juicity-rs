use slint::SharedString;

fn em_width(ch: char) -> f32 {
    if !ch.is_ascii() {
        1.0
    } else if ch.is_ascii_uppercase() {
        0.65
    } else if ch.is_ascii_alphanumeric() {
        0.55
    } else {
        0.3
    }
}

pub fn elide_middle(text: SharedString, width: f32, font_size: f32) -> SharedString {
    if font_size <= 0.0 {
        return text;
    }
    let budget = (width / font_size).max(0.0);
    if text.chars().map(em_width).sum::<f32>() <= budget {
        return text;
    }
    if budget < 1.0 {
        return SharedString::default();
    }
    let available = budget - 1.0;
    let mut used = 0.0;
    let mut head = 0;
    for (index, ch) in text.char_indices() {
        used += em_width(ch);
        if used > available * 0.6 {
            break;
        }
        head = index + ch.len_utf8();
    }
    used = 0.0;
    let mut tail = text.len();
    for (index, ch) in text.char_indices().rev() {
        used += em_width(ch);
        if used > available * 0.4 || index < head {
            break;
        }
        tail = index;
    }
    let mut result = String::with_capacity(head + 3 + text.len() - tail);
    result.push_str(&text[..head]);
    result.push('…');
    result.push_str(&text[tail..]);
    result.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_boundaries_and_budget() {
        let text: SharedString = "https://例子.example/規則.txt".into();
        assert_eq!(elide_middle(text.clone(), 1000.0, 14.0), text);
        let shortened = elide_middle(text, 130.0, 14.0);
        assert!(shortened.starts_with("https://"));
        assert!(shortened.ends_with(".txt"));
        assert!(shortened.contains('…'));
        assert!(shortened.chars().map(em_width).sum::<f32>() <= 130.0 / 14.0);
        assert_eq!(elide_middle("abc".into(), 0.0, 14.0), "");
        assert_eq!(elide_middle("abc".into(), 14.0, 14.0), "…");
    }
}

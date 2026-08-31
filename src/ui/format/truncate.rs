use ::std::iter::FromIterator;
use ::unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn truncate_iter_to_unicode_width<Input, Collect>(iter: Input, width: usize) -> Collect
where
    Input: Iterator<Item = char>,
    Collect: FromIterator<char>,
{
    let mut chunk_width = 0;
    iter.take_while(|ch| {
        chunk_width += ch.width().unwrap_or(1);
        chunk_width <= width
    })
    .collect()
}

pub fn truncate_middle(row: &str, max_length: u16) -> String {
    if max_length < 6 {
        truncate_iter_to_unicode_width(row.chars(), max_length as usize)
    } else if row.width() as u16 > max_length {
        let marker = if max_length.is_multiple_of(2) {
            "[...]"
        } else {
            "[..]"
        };
        let remaining = usize::from(max_length).saturating_sub(marker.width());
        let first_width = remaining / 2;
        let second_width = remaining.saturating_sub(first_width);
        let first_slice = truncate_iter_to_unicode_width::<_, String>(row.chars(), first_width);
        let second_slice =
            truncate_iter_to_unicode_width::<_, Vec<_>>(row.chars().rev(), second_width)
                .into_iter()
                .rev()
                .collect::<String>();

        format!("{first_slice}{marker}{second_slice}")
    } else {
        row.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_char_boundary() {
        assert_eq!(
            truncate_middle("굿걸 - 누가 방송국을 털었나 E06.mp4", 30),
            "굿걸 - 누가 [...]었나 E06.mp4",
        );
    }

    #[test]
    fn truncate_middle_respects_even_marker_budget() {
        let truncated = truncate_middle("abcdefg", 6);
        assert_eq!(truncated.width(), 6);
    }
}

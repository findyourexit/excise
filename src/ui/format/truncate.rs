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
        let split_point = (max_length as usize / 2) - 2;
        let first_slice = truncate_iter_to_unicode_width::<_, String>(row.chars(), split_point);
        let second_slice =
            truncate_iter_to_unicode_width::<_, Vec<_>>(row.chars().rev(), split_point)
                .into_iter()
                .rev()
                .collect::<String>();

        if max_length % 2 == 0 {
            format!("{first_slice}[...]{second_slice}")
        } else {
            format!("{first_slice}[..]{second_slice}")
        }
    } else {
        row.to_string()
    }
}

pub fn truncate_end(row: &str, max_len: u16) -> String {
    let max_len = max_len as usize;
    if row.width() <= max_len {
        return row.to_string();
    }
    if max_len <= 3 {
        return ".".repeat(max_len);
    }

    let content = truncate_iter_to_unicode_width::<_, String>(row.chars(), max_len - 3);
    format!("{content}...")
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
    fn truncate_end_respects_zero_and_unicode_widths() {
        assert_eq!(truncate_end("abc", 0), "");
        assert_eq!(truncate_end("abcdef", 4), "a...");
        assert_eq!(truncate_end("굿걸", 3), "...");
        assert_eq!(truncate_end("굿걸", 4), "굿걸");
    }
}

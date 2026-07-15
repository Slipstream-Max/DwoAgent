pub(super) fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(if eof {
            lines.len()
        } else {
            start.min(lines.len())
        });
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    let search_start = if eof { last } else { start.min(last) };
    for strategy in [
        MatchStrategy::Exact,
        MatchStrategy::Rstrip,
        MatchStrategy::Trim,
        MatchStrategy::Unicode,
    ] {
        for index in search_start..=last {
            if pattern
                .iter()
                .enumerate()
                .all(|(offset, expected)| line_matches(&lines[index + offset], expected, strategy))
            {
                return Some(index);
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum MatchStrategy {
    Exact,
    Rstrip,
    Trim,
    Unicode,
}

fn line_matches(actual: &str, expected: &str, strategy: MatchStrategy) -> bool {
    match strategy {
        MatchStrategy::Exact => actual == expected,
        MatchStrategy::Rstrip => actual.trim_end() == expected.trim_end(),
        MatchStrategy::Trim => actual.trim() == expected.trim(),
        MatchStrategy::Unicode => normalize_unicode(actual) == normalize_unicode(expected),
    }
}

fn normalize_unicode(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

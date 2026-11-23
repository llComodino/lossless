/// Longest line a modal shows; the rest is cut.
pub const MAX_CHARS: usize = 4096;

/// One output line made safe to draw: lossy UTF-8, escape sequences and
/// control characters removed, tabs expanded to 8 columns.
pub fn sanitize(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::new();
    let mut n = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if n >= MAX_CHARS {
            break;
        }
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => {
                let pad = 8 - n % 8;
                for _ in 0..pad.min(MAX_CHARS - n) {
                    out.push(' ');
                }
                n += pad;
            }
            c if c.is_control() => {}
            c => {
                out.push(c);
                n += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes() {
        assert_eq!(sanitize(b"\x1b[31mred\x1b[0m"), "red");
        assert_eq!(sanitize(b"\x1b]0;title\x07after"), "after");
    }

    #[test]
    fn expands_tabs_and_drops_controls() {
        assert_eq!(sanitize(b"a\tb\x07\r"), "a       b");
        assert_eq!(sanitize(b"abcdefgh\tx"), "abcdefgh        x");
    }

    #[test]
    fn huge_line_is_truncated() {
        assert_eq!(sanitize(&vec![b'x'; 1 << 20]).chars().count(), MAX_CHARS);
    }

    #[test]
    fn invalid_utf8_is_lossy() {
        assert_eq!(sanitize(b"a\xffb"), "a\u{fffd}b");
    }
}

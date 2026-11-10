//! Whitespace diagnostics do not transform checkout bytes or select merge drivers.
//! Every other attribute remains outside the no-checkout-attributes profile.
pub(crate) fn whitespace_only(bytes: &[u8]) -> bool {
    if bytes.len() > 65_536 {
        return false;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    text.lines().all(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return true;
        }
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        fields.len() >= 2
            && !fields[0].starts_with("[attr]")
            && fields[1..].iter().all(|field| {
                field.strip_prefix("whitespace=").is_some_and(|value| {
                    value.split(',').all(|flag| {
                        matches!(
                            flag.strip_prefix('-').unwrap_or(flag),
                            "blank-at-eol"
                                | "blank-at-eof"
                                | "space-before-tab"
                                | "indent-with-non-tab"
                                | "tab-in-indent"
                                | "cr-at-eol"
                                | "trailing-space"
                        )
                    })
                })
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_diagnostic_flags_are_accepted() {
        assert!(whitespace_only(b"# Preserve evidence\ndocs/*.patch whitespace=-blank-at-eol\ndocs/*.log whitespace=-blank-at-eof\n"));
        for text in [
            "* filter=lfs",
            "* text=auto",
            "* merge=custom",
            "* working-tree-encoding=UTF-16",
            "* whitespace=-blank-at-eol filter=custom",
            "[attr]diagnostic whitespace=-blank-at-eof",
            "* whitespace=unknown",
            "* whitespace=",
            "* whitespace=-blank-at-eol,",
            "* diff=external",
        ] {
            assert!(!whitespace_only(text.as_bytes()), "{text}");
        }
        assert!(!whitespace_only(&[0xff]));
        assert!(!whitespace_only(&vec![b'#'; 65_537]));
    }
}

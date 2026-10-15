pub(super) fn path_for_shell(path: &str) -> Vec<u16> {
    let (prefix, path) = if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        (r"\\", path)
    } else {
        ("", path.strip_prefix(r"\\?\").unwrap_or(path))
    };
    prefix
        .encode_utf16()
        .chain(path.encode_utf16())
        .chain(Some(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::path_for_shell;

    #[test]
    fn preserves_extended_unc_root() {
        // U-07: canonical network directories must not become relative paths.
        assert_eq!(
            String::from_utf16(&path_for_shell(r"\\?\UNC\server\share\目录")).unwrap(),
            "\\\\server\\share\\目录\0"
        );
    }

    #[test]
    fn preserves_drive_and_ordinary_unc_paths() {
        for (input, expected) in [
            (r"C:\目录", r"C:\目录"),
            (r"\\?\C:\目录", r"C:\目录"),
            (r"\\server\share\目录", r"\\server\share\目录"),
        ] {
            assert_eq!(
                String::from_utf16(&path_for_shell(input)).unwrap(),
                format!("{expected}\0")
            );
        }
    }
}

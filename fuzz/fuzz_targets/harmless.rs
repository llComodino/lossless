//! What an accepted config may contain, stated independently of the parser's
//! grammar: the fuzz targets assert it for everything the daemon accepts.

use std::path::Path;

/// Device names reach ALSA's whole plugin syntax; an accepted one must be a
/// bare route or a hardware family with `id`/`n` arguments: no quotes,
/// braces, pipes, slashes, spaces or other plugin families.
pub fn device(name: &str) -> bool {
    let charset = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_-:,=".contains(&b));
    let family_ok = match name.split_once(':') {
        Some((family, args)) => {
            ["hw", "plughw", "front", "sysdefault"].contains(&family) && !args.contains(':')
        }
        None => ["default", "null", "pipewire", "pulse", "sysdefault"].contains(&name),
    };
    charset && family_ok && name.len() <= 48
}

/// Config paths: absolute, no `.`/`..` segments, no control characters.
pub fn path(p: &Path) -> bool {
    let Some(s) = p.to_str() else { return false };
    p.is_absolute()
        && !s.split('/').any(|seg| seg == "." || seg == "..")
        && !s.chars().any(char::is_control)
        && s.len() <= 4096
}

use crate::{StreamFormat, Track};

/// `m:ss`, or `h:mm:ss` from an hour up.
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// `24-bit/96 kHz`.
pub fn rate_str(f: &StreamFormat) -> String {
    format!("{}-bit/{} kHz", f.bits, f64::from(f.rate) / 1000.0)
}

/// `24-bit/96 kHz/2ch`.
pub fn format_str(f: &StreamFormat) -> String {
    format!("{}/{}ch", rate_str(f), f.channels)
}

/// "Artist - Title (Album)", falling back to the file name.
pub fn describe(t: &Track) -> String {
    let name = || {
        std::path::Path::new(&t.path)
            .file_name()
            .map_or_else(|| t.path.clone(), |n| n.to_string_lossy().into_owned())
    };
    let mut s = match (&t.artist, &t.title) {
        (Some(a), Some(ti)) => format!("{a} - {ti}"),
        (None, Some(ti)) => ti.clone(),
        _ => name(),
    };
    if let Some(album) = &t.album {
        s.push_str(&format!(" ({album})"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str, artist: Option<&str>, title: Option<&str>, album: Option<&str>) -> Track {
        Track {
            path: path.into(),
            title: title.map(Into::into),
            artist: artist.map(Into::into),
            album: album.map(Into::into),
            track_number: None,
            disc_number: None,
            duration_ms: None,
            format: None,
        }
    }

    #[test]
    fn clock_switches_to_hours() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(59_999), "0:59");
        assert_eq!(clock(3_600_000), "1:00:00");
        assert_eq!(clock(36_061_000), "10:01:01");
    }

    #[test]
    fn formats_with_and_without_channels() {
        let f = StreamFormat {
            rate: 44_100,
            bits: 16,
            channels: 2,
        };
        assert_eq!(rate_str(&f), "16-bit/44.1 kHz");
        assert_eq!(format_str(&f), "16-bit/44.1 kHz/2ch");
        let f = StreamFormat {
            rate: 96_000,
            bits: 24,
            channels: 2,
        };
        assert_eq!(rate_str(&f), "24-bit/96 kHz");
    }

    #[test]
    fn describe_falls_back_to_the_file_name() {
        let p = "/m/01 x.flac";
        assert_eq!(
            describe(&track(p, Some("A"), Some("T"), Some("L"))),
            "A - T (L)"
        );
        assert_eq!(describe(&track(p, None, Some("T"), None)), "T");
        assert_eq!(describe(&track(p, Some("A"), None, None)), "01 x.flac");
        assert_eq!(describe(&track("/", None, None, None)), "/");
    }
}

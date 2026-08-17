//! Display helpers shared by every surface that renders events for a human
//! (CLI tables, TUI panels, analyzer finding prose).
//!
//! These lived as private copies in `smeltr-cli`, `smeltr-tui` and
//! `smeltr-analyzer`, and had drifted apart: three of the five byte
//! formatters labelled gibibytes as "GB" (off by 7%), and one `truncate`
//! sliced by byte index, panicking on any non-ASCII name. One definition
//! each keeps every surface reporting the same number for the same bytes.

/// Format a byte count with binary (1024-based) units and matching labels.
///
/// Used for every table and panel that shows memory. Jetsam prose uses
/// [`decimal_gb`] instead — see its note.
pub fn binary_bytes(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if b >= GIB {
        format!("{:.2} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.1} MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.0} KiB", b as f64 / KIB as f64)
    } else {
        format!("{b} B")
    }
}

/// Byte count as decimal gigabytes (1e9), for prose that quotes a jetsam
/// report. macOS and the `.ips` reports themselves express footprints this
/// way; rendering them as GiB would contradict the numbers the user reads
/// in Console.app.
pub fn decimal_gb(b: u64) -> f64 {
    b as f64 / 1_000_000_000.0
}

/// Truncate to at most `max` characters, marking elision with `…`.
///
/// Counts characters, never bytes: slicing a `str` by byte index panics
/// when the cut lands mid-codepoint, which any accented module qualname
/// or file path will do.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Last path component, falling back to the whole string when there is none.
pub fn basename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_bytes_labels_binary_units() {
        assert_eq!(binary_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
        assert_eq!(binary_bytes(4 * 1024 * 1024), "4.0 MiB");
        assert_eq!(binary_bytes(2048), "2 KiB");
        assert_eq!(binary_bytes(512), "512 B");
        assert_eq!(binary_bytes(0), "0 B");
    }

    /// The bug this module exists to end: 2e9 bytes is 1.86 GiB, and three
    /// call sites used to print that figure under a "GB" label.
    #[test]
    fn binary_bytes_does_not_mislabel_gibibytes_as_gb() {
        let s = binary_bytes(2_000_000_000);
        assert_eq!(s, "1.86 GiB");
        assert!(!s.contains("GB"), "{s}");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        // 48 'a' then a multibyte char: a byte-indexed slice would panic.
        let s = format!("{}é", "a".repeat(48));
        let out = truncate(&s, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn truncate_zero_max_is_empty_not_underflow() {
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn basename_takes_last_component() {
        assert_eq!(basename("/a/b/c.py"), "c.py");
        assert_eq!(basename("c.py"), "c.py");
        assert_eq!(basename(""), "");
    }

    #[test]
    fn decimal_gb_matches_jetsam_report_convention() {
        assert!((decimal_gb(2_000_000_000) - 2.0).abs() < f64::EPSILON);
    }
}

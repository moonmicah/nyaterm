//! `ls -l` parsing for HarmonyOS devices.
//!
//! hdc exposes no structured file API, so entry metadata comes from the device's
//! own `ls -l`. HarmonyOS ships toybox, which prints an ISO timestamp
//! (`2024-05-06 07:08`); the GNU-style `May  6 07:08` form is accepted as well so
//! a differently built device shell degrades to a missing timestamp instead of
//! an unusable directory listing.

use crate::sftp::SftpFileType;

/// One parsed `ls -l` line. `symlink_target` is only set for symbolic links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedEntry {
    pub name: String,
    pub file_type: SftpFileType,
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub owner: String,
    pub group: String,
    pub modified_at: Option<u32>,
    pub symlink_target: Option<String>,
}

/// Parse one `ls -l` line. Returns `None` for anything that is not a long-format
/// entry (blank lines, `total 12` headers, warnings).
pub(crate) fn parse_line(line: &str) -> Option<ParsedEntry> {
    let line = line.trim_end_matches('\r');
    if line.trim().is_empty() {
        return None;
    }
    let tokens = tokenize(line);
    if tokens.len() < 6 {
        return None;
    }
    let (file_type, mode) = parse_mode(tokens[0].1)?;
    let mut index = 1;
    // Link count. Some shells omit it; only consume it when it is numeric and a
    // further token still exists.
    if tokens.len() > index + 5 && tokens[index].1.bytes().all(|byte| byte.is_ascii_digit()) {
        index += 1;
    }
    let owner = tokens.get(index)?.1.to_string();
    index += 1;
    let group = tokens.get(index)?.1.to_string();
    index += 1;
    let size = match tokens.get(index).map(|(_, token)| *token) {
        Some(token) if token.bytes().all(|byte| byte.is_ascii_digit()) => {
            index += 1;
            token.parse::<u64>().ok()
        }
        // Character and block devices print `major, minor` instead of a size.
        Some(token) if token.ends_with(',') => {
            index += 2;
            None
        }
        _ => None,
    };
    let (modified_at, name_index) = parse_timestamp(&tokens, index);
    let name_index = name_index.min(tokens.len().saturating_sub(1));
    let raw_name = line[tokens[name_index].0..].trim_end();
    if raw_name.is_empty() {
        return None;
    }
    let (name, symlink_target) = match raw_name.split_once(" -> ") {
        Some((name, target)) if file_type == SftpFileType::Symlink => {
            (name.trim_end().to_string(), Some(target.to_string()))
        }
        _ => (raw_name.to_string(), None),
    };
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(ParsedEntry {
        name,
        file_type,
        size,
        mode: Some(mode),
        owner,
        group,
        modified_at,
        symlink_target,
    })
}

/// Byte offset and text of every space-separated token, so a trailing name may
/// contain spaces.
fn tokenize(line: &str) -> Vec<(usize, &str)> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index] == b' ' {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let start = index;
        while index < bytes.len() && bytes[index] != b' ' {
            index += 1;
        }
        tokens.push((start, &line[start..index]));
    }
    tokens
}

/// Decode a `-rw-r--r--` style token into a file type and permission bits.
pub(crate) fn parse_mode(token: &str) -> Option<(SftpFileType, u32)> {
    let bytes = token.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let file_type = match bytes[0] {
        b'-' => SftpFileType::File,
        b'd' => SftpFileType::Directory,
        b'l' => SftpFileType::Symlink,
        b'b' | b'c' | b'p' | b's' => SftpFileType::Other,
        _ => return None,
    };
    let permissions = &bytes[1..10];
    let mut mode = 0_u32;
    for (index, byte) in permissions.iter().enumerate() {
        // `S` and `T` mean "special bit without execute".
        if !matches!(byte, b'-' | b'S' | b'T') {
            mode |= 1 << (8 - index);
        }
    }
    if matches!(permissions[2], b's' | b'S') {
        mode |= 0o4000;
    }
    if matches!(permissions[5], b's' | b'S') {
        mode |= 0o2000;
    }
    if matches!(permissions[8], b't' | b'T') {
        mode |= 0o1000;
    }
    Some((file_type, mode))
}

/// Returns the modification time and the index of the first name token.
fn parse_timestamp(tokens: &[(usize, &str)], index: usize) -> (Option<u32>, usize) {
    let Some((_, first)) = tokens.get(index) else {
        return (None, index);
    };
    if is_iso_date(first) {
        let time = tokens.get(index + 1).map(|(_, token)| *token).unwrap_or("");
        return (parse_iso(first, time), (index + 2).min(tokens.len()));
    }
    if is_month_abbreviation(first) {
        let day = tokens.get(index + 1).map(|(_, token)| *token).unwrap_or("");
        let third = tokens.get(index + 2).map(|(_, token)| *token).unwrap_or("");
        return (
            parse_month_day(first, day, third),
            (index + 3).min(tokens.len()),
        );
    }
    (None, index)
}

fn is_iso_date(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..].iter().all(u8::is_ascii_digit)
}

fn is_month_abbreviation(token: &str) -> bool {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS.contains(&token)
}

fn month_number(token: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| *month == token)
        .map(|index| index as u32 + 1)
}

fn parse_iso(date: &str, time: &str) -> Option<u32> {
    let year = date[..4].parse::<i64>().ok()?;
    let month = date[5..7].parse::<u32>().ok()?;
    let day = date[8..10].parse::<u32>().ok()?;
    let (hour, minute, second) = parse_clock(time).unwrap_or((0, 0, 0));
    epoch_from_civil(year, month, day, hour, minute, second)
}

fn parse_month_day(month: &str, day: &str, third: &str) -> Option<u32> {
    let month = month_number(month)?;
    let day = day.parse::<u32>().ok()?;
    // Recent entries print a clock, older ones print a year.
    match parse_clock(third) {
        Some((hour, minute, second)) => {
            let year = current_year()?;
            epoch_from_civil(year, month, day, hour, minute, second)
        }
        None => {
            let year = third.parse::<i64>().ok()?;
            epoch_from_civil(year, month, day, 0, 0, 0)
        }
    }
}

fn parse_clock(token: &str) -> Option<(u32, u32, u32)> {
    let mut parts = token.split(':');
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next()?.parse::<u32>().ok()?;
    let second = match parts.next() {
        Some(second) => second.parse::<u32>().ok()?,
        None => 0,
    };
    (hour < 24 && minute < 60 && second < 61).then_some((hour, minute, second))
}

fn current_year() -> Option<i64> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let days = (seconds / 86_400) as i64;
    Some(civil_from_days(days)?.0)
}

/// Seconds since the Unix epoch, treating the device clock as UTC. The device
/// timezone offset is constant, so comparisons between two listings stay valid.
pub(crate) fn epoch_from_civil(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<u32> {
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_index = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
    u32::try_from(seconds).ok()
}

/// Inverse of [`epoch_from_civil`] for the year of a day count.
fn civil_from_days(days: i64) -> Option<(i64, u32, u32)> {
    let days = days.checked_add(719_468)?;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    Some((
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::{ParsedEntry, SftpFileType, epoch_from_civil, parse_line, parse_mode};

    fn parse(line: &str) -> ParsedEntry {
        parse_line(line).unwrap_or_else(|| panic!("unparsed listing line: {line}"))
    }

    #[test]
    fn toybox_long_listing_yields_typed_entries() {
        let directory = parse("drwxr-xr-x 2 root root 4096 2024-05-06 07:08 files");
        assert_eq!(directory.file_type, SftpFileType::Directory);
        assert_eq!(directory.name, "files");
        assert_eq!(directory.size, Some(4096));
        assert_eq!(directory.owner, "root");
        assert_eq!(directory.mode, Some(0o755));
        assert_eq!(directory.modified_at, Some(1_714_979_280));

        let file = parse("-rw-r--r-- 1 20010012 20010012 12 2024-05-06 07:08:09 a.txt");
        assert_eq!(file.name, "a.txt");
        assert_eq!(file.size, Some(12));
        assert_eq!(file.mode, Some(0o644));
        assert_eq!(file.modified_at, Some(1_714_979_289));
    }

    #[test]
    fn names_with_spaces_and_symlink_targets_survive_parsing() {
        let spaced = parse("drwxrwx--x 3 shell shell 3452 2024-01-02 03:04 my documents");
        assert_eq!(spaced.name, "my documents");

        let link =
            parse("lrwxrwxrwx 1 root root 21 2024-01-02 03:04 sdcard -> /storage/self/primary");
        assert_eq!(link.file_type, SftpFileType::Symlink);
        assert_eq!(link.name, "sdcard");
        assert_eq!(
            link.symlink_target.as_deref(),
            Some("/storage/self/primary")
        );

        // A symlink whose name itself contains the separator keeps the first split.
        let awkward = parse("lrwxrwxrwx 1 root root 4 2024-01-02 03:04 a -> b -> c");
        assert_eq!(awkward.name, "a");
        assert_eq!(awkward.symlink_target.as_deref(), Some("b -> c"));
    }

    #[test]
    fn device_nodes_and_special_permission_bits_are_understood() {
        let device = parse("crw-rw---- 1 root shell 1, 3 2024-01-02 03:04 null");
        assert_eq!(device.file_type, SftpFileType::Other);
        assert_eq!(device.size, None);
        assert_eq!(device.name, "null");

        // `s` and `t` set both the special bit and execute.
        assert_eq!(parse_mode("drwxr-sr-t").unwrap().1, 0o3755);
        assert_eq!(parse_mode("drwx------").unwrap().1, 0o700);
        assert_eq!(parse_mode("-rws------").unwrap().1, 0o4700);
        assert_eq!(parse_mode("-rw-r--r--.").unwrap().1, 0o644);
        assert!(parse_mode("garbage").is_none());
    }

    #[test]
    fn headers_and_dot_entries_are_skipped() {
        for line in ["", "   ", "total 42", ".", ".."] {
            assert!(parse_line(line).is_none(), "{line}");
        }
        assert!(parse_line("drwxr-xr-x 2 root root 4096 2024-05-06 07:08 .").is_none());
        assert!(parse_line("drwxr-xr-x 2 root root 4096 2024-05-06 07:08 ..").is_none());
    }

    #[test]
    fn gnu_style_timestamps_are_accepted() {
        let entry = parse("-rw-r--r-- 1 root root 5 May  6 07:08 legacy.txt");
        assert_eq!(entry.name, "legacy.txt");
        assert!(entry.modified_at.is_some());

        let aged = parse("-rw-r--r-- 1 root root 5 May  6  2021 legacy.txt");
        assert_eq!(aged.name, "legacy.txt");
        assert_eq!(aged.modified_at, epoch_from_civil(2021, 5, 6, 0, 0, 0));
    }

    #[test]
    fn civil_dates_convert_to_epoch_seconds() {
        assert_eq!(epoch_from_civil(1970, 1, 1, 0, 0, 0), Some(0));
        assert_eq!(epoch_from_civil(2024, 5, 6, 0, 0, 0), Some(1_714_953_600));
        assert_eq!(epoch_from_civil(2024, 2, 29, 12, 0, 0), Some(1_709_208_000));
        assert_eq!(epoch_from_civil(1969, 12, 31, 23, 59, 59), None);
        assert_eq!(epoch_from_civil(2024, 13, 1, 0, 0, 0), None);
    }

    #[test]
    fn unreadable_timestamps_degrade_instead_of_losing_the_entry() {
        let entry = parse("-rw-r--r-- 1 root root 5 2024-13-45 99:99 broken");
        assert_eq!(entry.name, "broken");
        assert_eq!(entry.modified_at, None);
    }
}

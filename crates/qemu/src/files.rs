// SPDX-License-Identifier: MPL-2.0

//! The file list of a file clipboard, as Windows and RDP write it: a CLIPRDR_FILELIST
//! (MS-RDPECLIP 2.2.5.2.3), a little-endian item count followed by one FILEDESCRIPTORW
//! (592 bytes) per file or directory.

/// One file or directory of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// Relative to the copied set, `/` separated (Windows writes `\`).
    pub path: String,
    pub directory: bool,
    pub size: u64,
    /// Last modification, in milliseconds since 1970-01-01 UTC; 0 if unknown.
    pub modified_ms: i64,
}

/// The size of a FILEDESCRIPTORW.
const DESCRIPTOR: usize = 592;
/// Where its fields are.
const FLAGS: usize = 0;
const ATTRIBUTES: usize = 36;
const WRITE_TIME: usize = 56;
const SIZE_HIGH: usize = 64;
const SIZE_LOW: usize = 68;
const NAME: usize = 72;
/// cFileName: 260 UTF-16 code units, the last one a terminating zero.
const NAME_UNITS: usize = 260;

/// dwFlags: which fields are valid.
const FD_ATTRIBUTES: u32 = 0x4;
const FD_WRITESTIME: u32 = 0x20;
const FD_FILESIZE: u32 = 0x40;
/// Show the copy progress dialog: Windows shows none for a paste without it.
const FD_PROGRESSUI: u32 = 0x4000;
const FD_UNICODE: u32 = 0x8000_0000;
/// dwFileAttributes.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Milliseconds between 1601-01-01 (FILETIME) and 1970-01-01.
const FILETIME_TO_UNIX_MS: i64 = 11_644_473_600_000;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Read a CLIPRDR_FILELIST. `None` if it is malformed.
pub fn parse(list: &[u8]) -> Option<Vec<File>> {
    let count = u32_at(list.get(..4)?, 0) as usize;
    let descriptors = list.get(4..)?;
    if descriptors.len() != count.checked_mul(DESCRIPTOR)? {
        return None;
    }
    descriptors
        .as_chunks::<DESCRIPTOR>()
        .0
        .iter()
        .map(|d| {
            let flags = u32_at(d, FLAGS);
            let directory =
                flags & FD_ATTRIBUTES != 0 && u32_at(d, ATTRIBUTES) & FILE_ATTRIBUTE_DIRECTORY != 0;
            let size = if flags & FD_FILESIZE != 0 {
                (u64::from(u32_at(d, SIZE_HIGH)) << 32) | u64::from(u32_at(d, SIZE_LOW))
            } else {
                0
            };
            let modified_ms = match u64_at(d, WRITE_TIME) {
                0 => 0,
                filetime if flags & FD_WRITESTIME != 0 => {
                    (filetime / 10_000) as i64 - FILETIME_TO_UNIX_MS
                }
                _ => 0,
            };
            let name: Vec<u16> = d[NAME..NAME + NAME_UNITS * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_le_bytes(c))
                .take_while(|&c| c != 0)
                .collect();
            let path = String::from_utf16(&name).ok()?.replace('\\', "/");
            Some(File {
                path,
                directory,
                size,
                modified_ms,
            })
        })
        .collect()
}

/// A file list written for Windows.
#[derive(Debug)]
pub struct Built {
    /// The CLIPRDR_FILELIST.
    pub list: Vec<u8>,
    /// For each of its entries, the index of the file in the list it was built from.
    pub kept: Vec<usize>,
}

/// Write a CLIPRDR_FILELIST. `None` if a path is empty, absolute, or goes through `.` or `..`,
/// or holds what Windows reads as a separator, a drive or a stream (`\`, `:`) or a control
/// character: a list with one is broken or hostile, and Windows would write where the list
/// says. A path too long for a FILEDESCRIPTORW (259 UTF-16 code
/// units, a limit of the Windows format) is left out, with everything under it if it is a
/// directory: the rest of the list still goes.
pub fn build(files: &[File]) -> Option<Built> {
    let escapes = |path: &str| {
        path.is_empty()
            || path
                .chars()
                .any(|c| c == '\\' || c == ':' || c.is_control())
            || path
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
    };
    if files.iter().any(|f| escapes(&f.path)) {
        return None;
    }
    let mut descriptors = Vec::with_capacity(files.len() * DESCRIPTOR);
    let mut kept = Vec::with_capacity(files.len());
    let mut left_out: Vec<&str> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let under = |dir: &&str| {
            file.path
                .strip_prefix(*dir)
                .is_some_and(|rest| rest.starts_with('/'))
        };
        if left_out.iter().any(under) {
            continue;
        }
        let name: Vec<u16> = file.path.replace('/', "\\").encode_utf16().collect();
        if name.len() >= NAME_UNITS {
            if file.directory {
                left_out.push(&file.path);
            }
            continue;
        }
        let mut d = [0u8; DESCRIPTOR];
        let mut flags = FD_ATTRIBUTES | FD_FILESIZE | FD_PROGRESSUI | FD_UNICODE;
        let attributes = if file.directory {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        d[ATTRIBUTES..ATTRIBUTES + 4].copy_from_slice(&attributes.to_le_bytes());
        // A time that does not fit a FILETIME is left out, as an unknown one.
        let filetime = (file.modified_ms > 0)
            .then(|| file.modified_ms.checked_add(FILETIME_TO_UNIX_MS))
            .flatten()
            .and_then(|ms| u64::try_from(ms).ok()?.checked_mul(10_000));
        if let Some(filetime) = filetime {
            flags |= FD_WRITESTIME;
            d[WRITE_TIME..WRITE_TIME + 8].copy_from_slice(&filetime.to_le_bytes());
        }
        let size = if file.directory { 0 } else { file.size };
        d[SIZE_HIGH..SIZE_HIGH + 4].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
        d[SIZE_LOW..SIZE_LOW + 4].copy_from_slice(&(size as u32).to_le_bytes());
        d[FLAGS..FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        for (i, unit) in name.iter().enumerate() {
            d[NAME + i * 2..NAME + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        descriptors.extend_from_slice(&d);
        kept.push(index);
    }
    let mut list = Vec::with_capacity(4 + descriptors.len());
    list.extend_from_slice(&u32::try_from(kept.len()).ok()?.to_le_bytes());
    list.extend_from_slice(&descriptors);
    Some(Built { list, kept })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<File> {
        vec![
            File {
                path: "Photos".into(),
                directory: true,
                size: 0,
                modified_ms: 1_700_000_000_000,
            },
            File {
                path: "Photos/été 2026.jpg".into(),
                directory: false,
                size: 5_000_000_000,
                modified_ms: 1_762_000_000_123,
            },
            File {
                path: "notes.txt".into(),
                directory: false,
                size: 12,
                modified_ms: 0,
            },
        ]
    }

    #[test]
    fn hostile_paths_are_refused() {
        let file = |path: &str| File {
            path: path.into(),
            directory: false,
            size: 1,
            modified_ms: 0,
        };
        for path in [
            "a/../b",
            "/etc/passwd",
            "a//b",
            "a\\..\\..\\Windows\\x",
            "C:\\Windows\\x",
            "file.txt:stream",
            "a\u{0}b",
            "a\nb",
        ] {
            assert!(build(&[file(path)]).is_none(), "{path:?}");
        }
        assert!(build(&[file("a/b c/d.txt")]).is_some());
    }

    #[test]
    fn times_out_of_range_are_left_out() {
        let built = build(&[File {
            path: "f".into(),
            directory: false,
            size: 1,
            modified_ms: i64::MAX,
        }])
        .unwrap();
        assert_eq!(parse(&built.list).unwrap()[0].modified_ms, 0);
    }

    #[test]
    fn round_trip() {
        let built = build(&sample()).unwrap();
        assert_eq!(built.list.len(), 4 + 3 * DESCRIPTOR);
        assert_eq!(built.kept, [0, 1, 2]);
        assert_eq!(parse(&built.list).unwrap(), sample());
    }

    /// The bytes Windows reads: backslashes, sizes over 4 GiB split in two words.
    #[test]
    fn windows_layout() {
        let list = build(&sample()).unwrap().list;
        let second = &list[4 + DESCRIPTOR..4 + 2 * DESCRIPTOR];
        assert_eq!(u32_at(second, SIZE_HIGH), 1);
        assert_eq!(
            u32_at(second, SIZE_LOW),
            (5_000_000_000u64 & 0xFFFF_FFFF) as u32
        );
        assert_ne!(u32_at(second, FLAGS) & FD_PROGRESSUI, 0);
        let backslash = &second[NAME + 6 * 2..NAME + 7 * 2];
        assert_eq!(backslash, b"\\\0");
    }

    #[test]
    fn refuses_escaping_paths() {
        for path in ["../x", "a/../../x", "/abs", "a//b", ""] {
            let file = File {
                path: path.into(),
                directory: false,
                size: 0,
                modified_ms: 0,
            };
            assert!(build(&[file]).is_none(), "{path}");
        }
    }

    /// Too long for Windows: left out, with what a left-out directory holds; the rest goes, and
    /// `kept` says where each entry came from.
    #[test]
    fn leaves_out_long_paths() {
        let long = "d".repeat(300);
        let file = |path: String, directory| File {
            path,
            directory,
            size: 1,
            modified_ms: 0,
        };
        let files = vec![
            file("a.txt".into(), false),
            file(long.clone(), true),
            file(format!("{long}/inner.txt"), false),
            file(format!("{long}x.txt"), false),
            file("b.txt".into(), false),
        ];
        let built = build(&files).unwrap();
        assert_eq!(built.kept, [0, 4]);
        let paths: Vec<String> = parse(&built.list)
            .unwrap()
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(paths, ["a.txt", "b.txt"]);
    }

    #[test]
    fn refuses_malformed_lists() {
        assert!(parse(&[1, 0, 0, 0]).is_none());
        assert!(parse(&[]).is_none());
    }
}

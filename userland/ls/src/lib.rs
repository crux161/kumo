#![no_std]
//j548
//j558

/// The options shared by `ls` and Sora's choice of filesystem capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options<'a> {
    pub path: &'a [u8],
    pub show_all: bool,
    pub long: bool,
}

impl<'a> Options<'a> {
    /// Parse arguments after the program name: `[-al] [--] [path]`.
    pub fn parse(args: impl Iterator<Item = &'a [u8]>) -> Option<Self> {
        let mut path = None;
        let mut show_all = false;
        let mut long = false;
        let mut flags = true;
        for arg in args {
            match arg {
                b"--" if flags => flags = false,
                [b'-', rest @ ..] if flags && !rest.is_empty() => {
                    for flag in rest {
                        match flag {
                            b'a' => show_all = true,
                            b'l' => long = true,
                            _ => return None,
                        }
                    }
                }
                _ if path.is_none() => path = Some(arg),
                _ => return None,
            }
        }
        Some(Self {
            path: path.unwrap_or(b"."),
            show_all,
            long,
        })
    }

    pub fn includes(&self, name: &[u8]) -> bool {
        self.show_all || name.first() != Some(&b'.')
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Directory,
    File(u64),
}

/// Classify a path in the flat initrd table as a file or an implicit directory.
/// Match whole components: `/binning` is not `/bin`. — KESTREL
pub fn initrd_kind<'a>(
    path: &str,
    entries: impl Iterator<Item = (&'a str, u64)>,
) -> Option<NodeKind> {
    let path = path.trim_matches('/');
    if path.is_empty() || path == "." {
        return Some(NodeKind::Directory);
    }
    for (entry, size) in entries {
        if entry == path {
            return Some(NodeKind::File(size));
        }
        if child(path, entry).is_some() {
            return Some(NodeKind::Directory);
        }
    }
    None
}

fn child<'a>(directory: &str, entry: &'a str) -> Option<(&'a str, bool)> {
    let rest = if directory.is_empty() {
        entry
    } else {
        entry.strip_prefix(directory)?.strip_prefix('/')?
    };
    if rest.is_empty() {
        return None;
    }
    Some(match rest.split_once('/') {
        Some((name, _)) => (name, true),
        None => (rest, false),
    })
}

/// List immediate children once, preserving table order without allocating a second index.
/// The Stage-A initrd table is small and bounded by its caller. — KESTREL
pub fn list_initrd<'a, I: Iterator<Item = (&'a str, u64)>>(
    path: &str,
    entries: impl Fn() -> I,
    mut emit: impl FnMut(&'a str, NodeKind),
) -> bool {
    match initrd_kind(path, entries()) {
        None => return false,
        Some(NodeKind::File(size)) => {
            if let Some((name, _)) = entries().find(|(name, _)| *name == path.trim_matches('/')) {
                emit(
                    name.rsplit('/').next().unwrap_or(name),
                    NodeKind::File(size),
                );
            }
        }
        Some(NodeKind::Directory) => {
            let directory = path.trim_matches('/');
            let directory = if directory == "." { "" } else { directory };
            for (index, (entry, size)) in entries().enumerate() {
                let Some((name, is_dir)) = child(directory, entry) else {
                    continue;
                };
                if entries().take(index).any(|(earlier, _)| {
                    child(directory, earlier).is_some_and(|(previous, _)| previous == name)
                }) {
                    continue;
                }
                emit(
                    name,
                    if is_dir {
                        NodeKind::Directory
                    } else {
                        NodeKind::File(size)
                    },
                );
            }
        }
    }
    true
}

/// Both initrd and volume listings use the same familiar short/long presentation.
pub fn write_entry(
    options: Options<'_>,
    name: &[u8],
    kind: NodeKind,
    mut write: impl FnMut(&[u8]),
) {
    if !options.includes(name) {
        return;
    }
    if options.long {
        let mut column = [b' '; 10];
        match kind {
            NodeKind::Directory => column[5..].copy_from_slice(b"<dir>"),
            NodeKind::File(mut size) => {
                for digit in column.iter_mut().rev() {
                    *digit = b'0' + (size % 10) as u8;
                    size /= 10;
                    if size == 0 {
                        break;
                    }
                }
            }
        }
        write(&column);
        write(b"  ");
    }
    write(name);
    write(if kind == NodeKind::Directory {
        b"/\n"
    } else {
        b"\n"
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_before_or_after_the_path_select_the_same_volume() {
        let before: [&[u8]; 2] = [b"-a", b"/media/KUMO/Boot Files"];
        let after: [&[u8]; 2] = [before[1], before[0]];
        let options = Options::parse(before.into_iter()).unwrap();
        assert_eq!(options.path, b"/media/KUMO/Boot Files");
        assert!(options.show_all);
        assert_eq!(Options::parse(after.into_iter()), Some(options));
        assert!(Options::parse([b"-x".as_slice()].into_iter()).is_none());
        assert!(Options::parse([b"/one".as_slice(), b"/two"].into_iter()).is_none());
    }

    #[test]
    fn dotfiles_are_hidden_until_a_is_given() {
        let ordinary = Options::parse(core::iter::empty()).unwrap();
        let all = Options::parse([b"-a".as_slice()].into_iter()).unwrap();
        assert_eq!(ordinary.path, b".");
        for name in [b".Spotlight-V100".as_slice(), b"._photo.jpg"] {
            assert!(!ordinary.includes(name));
            assert!(all.includes(name));
        }
        assert!(ordinary.includes(b"photo.jpg"));
        assert!(all.includes(b"photo.jpg"));
    }

    #[test]
    fn initrd_navigation_lists_only_immediate_children() {
        extern crate std;
        let table = [
            ("bin/ls", 1),
            ("bin/cat", 2),
            ("etc/autoexec", 3),
            ("etc/.hidden", 4),
        ];
        let options = Options::parse(core::iter::empty()).unwrap();
        let mut output = std::vec::Vec::new();
        assert!(list_initrd(
            "/",
            || table.into_iter(),
            |name, kind| {
                write_entry(options, name.as_bytes(), kind, |bytes| {
                    output.extend_from_slice(bytes)
                });
            }
        ));
        assert_eq!(output, b"bin/\netc/\n");
        output.clear();
        assert!(list_initrd(
            "/etc",
            || table.into_iter(),
            |name, kind| {
                write_entry(options, name.as_bytes(), kind, |bytes| {
                    output.extend_from_slice(bytes)
                });
            }
        ));
        assert_eq!(output, b"autoexec\n");
        assert_eq!(
            initrd_kind("/etc/autoexec", table.into_iter()),
            Some(NodeKind::File(3))
        );
        assert_eq!(initrd_kind("/binning", table.into_iter()), None);
        assert_eq!(initrd_kind("/missing", table.into_iter()), None);
        assert!(!list_initrd(
            "/missing",
            || table.into_iter(),
            |_, _| panic!()
        ));
        output.clear();
        assert!(list_initrd(
            "/etc/autoexec",
            || table.into_iter(),
            |name, kind| {
                write_entry(options, name.as_bytes(), kind, |bytes| {
                    output.extend_from_slice(bytes)
                });
            }
        ));
        assert_eq!(output, b"autoexec\n");
    }

    #[test]
    fn short_long_hidden_and_dash_names_share_one_formatter() {
        extern crate std;
        let options = Options::parse([b"-al".as_slice(), b"--", b"-name"].into_iter()).unwrap();
        assert_eq!(options.path, b"-name");
        let mut output = std::vec::Vec::new();
        write_entry(options, b".hidden", NodeKind::File(42), |bytes| {
            output.extend_from_slice(bytes)
        });
        write_entry(options, b"sub", NodeKind::Directory, |bytes| {
            output.extend_from_slice(bytes)
        });
        assert_eq!(output, b"        42  .hidden\n     <dir>  sub/\n");
    }
}

#![no_std]
//j548

/// The options shared by `ls` and Sora's choice of filesystem capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options<'a> {
    pub path: &'a [u8],
    pub show_all: bool,
}

impl<'a> Options<'a> {
    /// Parse arguments after the program name: `[-a] [path]`.
    pub fn parse(args: impl Iterator<Item = &'a [u8]>) -> Option<Self> {
        let mut path = None;
        let mut show_all = false;
        for arg in args {
            match arg {
                b"-a" => show_all = true,
                [b'-', ..] => return None,
                _ if path.is_none() => path = Some(arg),
                _ => return None,
            }
        }
        Some(Self {
            path: path.unwrap_or(b"/"),
            show_all,
        })
    }

    pub fn includes(&self, name: &[u8]) -> bool {
        self.show_all || name.first() != Some(&b'.')
    }
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
        assert_eq!(ordinary.path, b"/");
        for name in [b".Spotlight-V100".as_slice(), b"._photo.jpg"] {
            assert!(!ordinary.includes(name));
            assert!(all.includes(name));
        }
        assert!(ordinary.includes(b"photo.jpg"));
        assert!(all.includes(b"photo.jpg"));
    }
}

//! The matcher: Can find substrings in a string that match any compiled regex

#[cfg(feature = "no_std")]
use alloc::collections::BTreeSet as HashSet;

#[cfg(not(feature = "no_std"))]
use std::collections::HashSet;

use alloc::{borrow::Cow, boxed::Box, rc::Rc, vec, vec::Vec};
use core::{cell::RefCell, fmt};

use crate::{
    compile::{Range, Token},
    ctype,
    immut_vec::ImmutVec,
    tree::{Node as TreeNode, *},
};

/// A regex matcher, ready to match stuff
#[derive(Clone)]
pub struct PosixRegex<'a> {
    tree: Cow<'a, Tree>,
    case_insensitive: bool,
    newline: bool,
    no_start: bool,
    no_end: bool,
}
impl<'a> PosixRegex<'a> {
    /// Create a new matcher instance from the specified alternations. This
    /// should probably not be used and instead an instance should be obtained
    /// from `PosixRegexBuilder`, which also compiles a string into regex.
    pub fn new(tree: Cow<'a, Tree>) -> Self {
        Self {
            tree,
            case_insensitive: false,
            newline: false,
            no_start: false,
            no_end: false,
        }
    }
    /// Chainable function to enable/disable case insensitivity. Default: false.
    /// When enabled, single characters match both their upper and lowercase
    /// representations.
    pub fn case_insensitive(mut self, value: bool) -> Self {
        self.case_insensitive = value;
        self
    }
    /// Chainable function to enable/disable newline mode. Default: false.
    /// When enabled, ^ and $ match newlines as well as start/end.
    /// This behavior overrides both no_start and no_end.
    pub fn newline(mut self, value: bool) -> Self {
        self.newline = value;
        self
    }
    /// Chainable function to enable/disable no_start mode. Default: false.
    /// When enabled, ^ doesn't actually match the start of a string.
    pub fn no_start(mut self, value: bool) -> Self {
        self.no_start = value;
        self
    }
    /// Chainable function to enable/disable no_start mode. Default: false.
    /// When enabled, $ doesn't actually match the end of a string.
    pub fn no_end(mut self, value: bool) -> Self {
        self.no_end = value;
        self
    }
    /// Return the total number of matches that **will** be returned by
    /// `matches_exact` or in each match in `matches`.
    pub fn count_groups(&self) -> usize {
        let mut count = 1;
        let mut cursor = self.tree[self.tree.root].child;
        while let Some(node) = cursor {
            // Walk tree
            let node = &self.tree[node];
            if node.child.is_some() {
                cursor = node.child;
            } else {
                let mut node = Some(node);
                while node
                    .map(|node| node.next_sibling.is_none())
                    .unwrap_or(false)
                {
                    node = node.unwrap().parent.map(|node| &self.tree[node]);
                }
                cursor = node.and_then(|node| node.next_sibling);
            }

            // Count groups
            if let Token::Group(_) = node.token {
                count += 1;
            }
        }
        count
    }
    /// Match the string starting at the current position. This does not find
    /// substrings.
    #[allow(clippy::type_complexity)]
    pub fn matches_exact(&self, input: &[u8]) -> Option<Box<[Option<(usize, usize)>]>> {
        let mut matcher = PosixRegexMatcher {
            base: self,
            input,
            offset: 0,
            max_groups: self.count_groups(),
        };
        let internal_prev = RefCell::new(Vec::new());
        let prev = ImmutVec::new(&internal_prev);
        let tree = self.tree[self.tree.root]
            .children(&self.tree)
            .filter_map(|node| {
                self.tree[node]
                    .child
                    .map(|child| Node::new(&self.tree, child, prev))
            })
            .collect();

        let start = matcher.offset;
        match matcher.matches_exact(tree) {
            None => None,
            Some(mut groups) => {
                assert_eq!(groups[0], None);
                groups[0] = Some((start, matcher.offset));
                Some(groups)
            }
        }
    }
    /// Match any substrings in the string, but optionally no more than `max`
    #[allow(clippy::type_complexity)]
    pub fn matches(
        &self,
        input: &[u8],
        mut max: Option<usize>,
    ) -> Vec<Box<[Option<(usize, usize)>]>> {
        let mut matcher = PosixRegexMatcher {
            base: self,
            input,
            offset: 0,
            max_groups: self.count_groups(),
        };

        let mut arena = self.tree.arena.to_vec();

        let root = self.tree[self.tree.root].child;

        // Wrap everything in group
        let group_id = NodeId::from(arena.len());
        arena.push(TreeNode {
            token: Token::Group(0),
            range: Range(1, Some(1)),
            parent: None,
            next_sibling: None,
            child: root,
        });

        // Update parents
        let mut cursor = root;
        while let Some(node) = cursor {
            let node = &mut arena[usize::from(node)];
            cursor = node.next_sibling;
            node.parent = Some(group_id);
        }

        // Push leading start
        let start_id = NodeId::from(arena.len());
        arena.push(TreeNode {
            token: Token::InternalStart,
            range: Range(0, None),
            parent: None,
            next_sibling: Some(group_id),
            child: None,
        });

        let tree = Tree {
            arena: arena.into_boxed_slice(),
            root: start_id,
        };
        let internal_prev = RefCell::new(Vec::new());
        let prev = ImmutVec::new(&internal_prev);
        let tree = vec![Node::new(&tree, tree.root, prev)];

        let mut matches = Vec::new();
        while max.map(|max| max > 0).unwrap_or(true) && matcher.offset <= matcher.input.len() {
            match matcher.matches_exact(tree.clone()) {
                Some(groups) => {
                    if groups[0].unwrap().0 == groups[0].unwrap().1 {
                        matcher.offset += 1;
                    }
                    matches.push(groups)
                }
                None => break,
            }
            max = max.map(|max| max - 1);
        }
        matches
    }
}

#[derive(Clone, Copy, Debug)]
struct GroupEvent {
    open: bool,
    id: usize,
    offset: usize,
}
#[derive(Clone, Copy)]
struct BackRef {
    offset: usize,
    index: usize,
    len: usize,
}

#[derive(Clone)]
struct Node<'a> {
    tree: &'a Tree,
    parent: Option<Rc<Node<'a>>>,
    node: NodeId,
    prev: ImmutVec<'a, GroupEvent>,
    repeated: u32,
    backref: Option<BackRef>,
}
impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut range = self.node().range;
        range.0 = range.0.saturating_sub(self.repeated);
        range.1 = range.1.map(|max| max.saturating_sub(self.repeated));
        write!(f, "{:?}", (&self.node().token, range))
    }
}
impl<'a> Node<'a> {
    /// Prepare a new node, such as linking back references
    fn prepare(mut me: Self) -> Self {
        me.repeated = 0;
        me.backref = None;
        if let Token::BackRef(id) = me.node().token {
            let mut start = None;
            let mut end = None;
            for event in me.prev.iter_rev() {
                if event.id != id as usize {
                    continue;
                }
                if event.open {
                    start = Some(event.offset);
                    break;
                } else {
                    end = end.or(Some(event.offset));
                }
            }
            if let (Some(start), Some(end)) = (start, end) {
                me.backref = Some(BackRef {
                    offset: start,
                    index: 0,
                    len: end - start,
                });
                if start == end {
                    // Empty group, mark as repeated enough times
                    let Range(min, _) = me.node().range;
                    me.repeated += min;
                }
            }
        }
        me
    }
    /// Create a new node. This is only called from the main function to start each alternative path
    fn new(tree: &'a Tree, node: NodeId, prev: ImmutVec<'a, GroupEvent>) -> Self {
        Self::prepare(Self {
            tree,
            parent: None,
            node,
            prev,
            repeated: 0,
            backref: None,
        })
    }
    /// Expand this group node into its children
    fn into_children(mut self, branches: &mut Vec<Node<'a>>, offset: usize) {
        let id = match self.tree[self.node].token {
            Token::Group(id) => id,
            _ => return,
        };
        self.repeated += 1;
        let mut parent = Rc::new(self);
        let mut empty = true;
        for alternative in parent.tree[parent.node].children(parent.tree) {
            if let Some(node) = parent.tree[alternative].child {
                empty = false;
                branches.push(Self::prepare(Self {
                    tree: parent.tree,
                    parent: Some(Rc::clone(&parent)),
                    node,
                    prev: parent.prev.push(GroupEvent {
                        open: true,
                        id,
                        offset,
                    }),
                    repeated: 0,
                    backref: None,
                }));
            }
        }
        if empty {
            let parent = Rc::get_mut(&mut parent)
                .expect("group empty but still there's a dangling reference");
            for &open in &[true, false] {
                parent.prev = parent.prev.push(GroupEvent { open, id, offset });
            }
            parent.add_branches(branches, offset);
        }
    }
    /// Get the internal token node without additional state metadata
    fn node(&self) -> &TreeNode {
        &self.tree[self.node]
    }
    /// Get a list of all capturing groups
    fn get_capturing_groups(
        &self,
        max_count: usize,
        offset: usize,
    ) -> Box<[Option<(usize, usize)>]> {
        let mut prev = self.prev;

        // Close all currently open groups
        let mut parent = self.node().parent;
        while let Some(group) = parent {
            let group = &self.tree[group];
            parent = group.parent;
            if let Token::Group(id) = group.token {
                prev = prev.push(GroupEvent {
                    open: false,
                    id,
                    offset,
                })
            }
        }

        // Go backwards through the immutable list and add groups
        let mut groups: Vec<(Option<usize>, Option<usize>)> = vec![(None, None); max_count];
        for event in prev.iter_rev() {
            let group = &mut groups[event.id];
            if event.open {
                group.0 = group.0.or(Some(event.offset));
            } else {
                group.1 = group.1.or(Some(event.offset));
            }
        }
        groups
            .into_iter()
            .map(|(start, end)| Some((start?, end?)))
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }
    /// Increment this branch, such as moving a back reference or increasing the number of times repeated
    fn increment(&mut self) {
        if let Some(ref mut backref) = self.backref {
            backref.index += 1;
            if backref.index >= backref.len {
                backref.index = 0;
                self.repeated += 1;
            }
        } else {
            self.repeated += 1;
        }
    }
    /// Add all possible branches from this node, such as the next node or
    /// possibly repeat the parent
    fn add_branches(&self, branches: &mut Vec<Node<'a>>, offset: usize) {
        let Range(min, _) = self.node().range;
        if self
            .backref
            .map(|backref| backref.index > 0 || self.repeated < min)
            .unwrap_or(false)
        {
            // Wait for back reference to complete
        } else if let Some(next) = self.node().next_sibling {
            branches.push(Self::prepare(Self {
                node: next,
                ..self.clone()
            }));
        } else {
            let parent = match self.parent {
                Some(ref parent) => parent,
                None => return,
            };
            let Range(min, _) = parent.node().range;

            // Get list of ids
            let mut ids = Vec::new();
            {
                let mut parent = Some(parent);
                while let Some(node) = parent {
                    if let Token::Group(id) = node.node().token {
                        ids.push(id);
                    }
                    parent = node.parent.as_ref();
                }
            }

            if parent.repeated >= min {
                // Group is closing, migrate previous & current groups to next.
                let mut parent = Some(parent);
                while parent
                    .map(|parent| parent.node().next_sibling.is_none())
                    .unwrap_or(false)
                {
                    parent = parent.unwrap().parent.as_ref();
                }
                if let Some((node, next)) =
                    parent.and_then(|parent| parent.node().next_sibling.map(|node| (parent, node)))
                {
                    let clone = (**node).clone();
                    let mut prev = self.prev;
                    for &id in &ids {
                        prev = prev.push(GroupEvent {
                            open: false,
                            id,
                            offset,
                        });
                    }
                    branches.push(Self::prepare(Self {
                        node: next,
                        prev,
                        ..clone
                    }));
                }
            }

            // Add repetitions
            let mut parent = Some(parent);
            while let Some(node) = parent {
                parent = node.parent.as_ref();
                let Range(_, max) = node.node().range;
                if max.map(|max| node.repeated < max).unwrap_or(true) {
                    let mut clone = (**node).clone();
                    let mut prev = self.prev;
                    for &id in &ids {
                        prev = prev.push(GroupEvent {
                            open: false,
                            id,
                            offset,
                        });
                    }
                    clone.prev = prev;
                    clone.into_children(branches, offset);
                }
            }
        }
    }
    /// Returns true if this node is the final node in the branch
    fn is_final(&self) -> bool {
        let Range(min, _) = self.node().range;
        if self.repeated < min {
            return false;
        }

        let mut next = Some(self);
        while let Some(current) = next {
            let mut node = current.node();
            if node.token == Token::Alternative {
                // Don't explore other alternatives
                next = current.parent.as_deref();
                node = &self.tree[node.parent.expect("found root alternative")];
            }
            if let Token::Group(_) = node.token {
                let Range(min, _) = node.range;
                if current.repeated < min {
                    return false;
                }
            }
            if node.next_sibling.is_some() {
                break;
            }
            next = current.parent.as_deref();
        }
        next.and_then(|node| self.tree[node.node].next_sibling)
            .is_none()
    }
}

struct PosixRegexMatcher<'a> {
    base: &'a PosixRegex<'a>,
    input: &'a [u8],
    offset: usize,
    max_groups: usize,
}
impl PosixRegexMatcher<'_> {
    fn expand<'b>(
        &mut self,
        skip: &mut HashSet<NodeId>,
        branches: &mut [Node<'b>],
    ) -> Vec<Node<'b>> {
        let mut insert = Vec::new();

        for branch in &mut *branches {
            if skip.contains(&branch.node) {
                continue;
            }

            let node = branch.node();

            if let Token::Group(_) = node.token {
                branch.clone().into_children(&mut insert, self.offset);
            }

            let Range(min, _) = node.range;
            if branch.repeated >= min {
                // Push the next element as a new branch
                branch.add_branches(&mut insert, self.offset);
            }
        }

        if !insert.is_empty() {
            for branch in &mut *branches {
                skip.insert(branch.node);
            }
            let mut new = self.expand(skip, &mut insert);
            insert.append(&mut new);
        }
        insert
    }

    #[allow(clippy::type_complexity)]
    fn matches_exact(&mut self, mut branches: Vec<Node>) -> Option<Box<[Option<(usize, usize)>]>> {
        // Whether or not any branch, at any point, got fully explored. This
        // means at least one path of the regex successfully completed!
        let mut succeeded = None;
        let mut prev = self
            .offset
            .checked_sub(1)
            .and_then(|index| self.input.get(index).cloned());

        let mut set = HashSet::new();

        loop {
            let next = self.input.get(self.offset).cloned();

            set.clear();
            let mut insert = self.expand(&mut set, &mut branches);
            branches.append(&mut insert);

            // Handle zero-width stuff
            loop {
                let mut index = 0;
                let mut remove = 0;

                while index < branches.len() {
                    if remove > 0 {
                        branches.swap(index, index - remove);
                    }
                    let branch = &mut branches[index - remove];
                    index += 1;

                    let node = branch.node();

                    match node.token {
                        Token::End | Token::Start | Token::WordEnd | Token::WordStart => {
                            let accepts = match node.token {
                                Token::End => {
                                    (!self.base.no_end && next.is_none())
                                        || (self.base.newline && next == Some(b'\n'))
                                }
                                Token::Start => {
                                    (!self.base.no_start && self.offset == 0)
                                        || (self.base.newline && prev == Some(b'\n'))
                                }
                                Token::WordEnd => next.map(ctype::is_word_boundary).unwrap_or(true),
                                Token::WordStart => {
                                    prev.map(ctype::is_word_boundary).unwrap_or(true)
                                }
                                _ => unreachable!(),
                            };
                            if accepts {
                                branch.increment();
                                branch.add_branches(&mut insert, self.offset);
                            }
                            if branch.is_final() {
                                succeeded =
                                    Some(branch.get_capturing_groups(self.max_groups, self.offset));
                            }
                            remove += 1;
                        }
                        _ => (),
                    }
                }
                branches.truncate(branches.len() - remove);

                if insert.is_empty() {
                    break;
                }
                set.clear();
                let mut insert2 = self.expand(&mut set, &mut insert);
                branches.append(&mut insert);
                branches.append(&mut insert2);
            }

            let mut index = 0;
            let mut remove = 0;

            // Handle stuff
            while index < branches.len() {
                if remove > 0 {
                    // Just like Rust's `retain` function, shift all elements I
                    // want to keep back and `truncate` when I'm done.
                    branches.swap(index, index - remove);
                }
                let branch = &mut branches[index - remove];
                index += 1;

                let node = branch.node();
                let Range(_, max) = node.range;

                // Step 3: Check if the token matches
                let accepts = max.map(|max| branch.repeated < max).unwrap_or(true)
                    && match node.token {
                        Token::InternalStart => next.is_some(),
                        Token::Group { .. } => false, // <- content is already expanded and handled

                        Token::Any => next
                            .map(|c| !self.base.newline || c != b'\n')
                            .unwrap_or(false),
                        Token::BackRef(_) => {
                            if let Some(ref backref) = branch.backref {
                                next == Some(self.input[backref.offset + backref.index])
                            } else {
                                false
                            }
                        }
                        Token::Char(c) => {
                            if self.base.case_insensitive {
                                next.map(|c2| c & !32 == c2 & !32).unwrap_or(false)
                            } else {
                                next == Some(c)
                            }
                        }
                        Token::OneOf { invert, ref list } => {
                            if let Some(next) = next {
                                (!invert || !self.base.newline || next != b'\n')
                                    && list
                                        .iter()
                                        .any(|c| c.matches(next, self.base.case_insensitive))
                                        != invert
                            } else {
                                false
                            }
                        }

                        Token::Alternative
                        | Token::End
                        | Token::Root
                        | Token::Start
                        | Token::WordEnd
                        | Token::WordStart => unreachable!(),
                    };

                if accepts {
                    branch.increment();
                } else {
                    if branch.is_final() {
                        let groups = branch.get_capturing_groups(self.max_groups, self.offset);

                        let mut add = true;
                        if let Some((new_start, new_end)) = groups[0] {
                            if let Some(previous) = succeeded.as_ref() {
                                if let Some((prev_start, prev_end)) = previous[0] {
                                    if new_end - new_start <= prev_end - prev_start {
                                        add = false;
                                    }
                                }
                            }
                        }
                        if add {
                            succeeded = Some(groups);
                        }
                    }
                    remove += 1;
                }
            }
            let end = branches.len() - remove;
            branches.truncate(end);

            if branches.is_empty() ||
                    // The internal start thing is lazy, not greedy:
                    (succeeded.is_some() && branches.iter().all(|t| t.node().token == Token::InternalStart))
            {
                return succeeded;
            }

            if next.is_some() {
                self.offset += 1;
                prev = next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "bench")]
    extern crate test;

    #[cfg(feature = "bench")]
    use self::test::Bencher;

    use super::*;
    use crate::PosixRegexBuilder;

    // FIXME: Workaround to coerce a Box<[T; N]> into a Box<[T]>. Use type
    // ascription when stabilized.
    fn boxed_slice<T>(slice: Box<[T]>) -> Box<[T]> {
        slice
    }

    macro_rules! abox {
        ($($item:expr),*) => {
            boxed_slice(Box::new([$($item),*]))
        }
    }

    fn compile(regex: &str) -> PosixRegex {
        PosixRegexBuilder::new(regex.as_bytes())
            .with_default_classes()
            .compile()
            .expect("error compiling regex")
    }
    fn matches(regex: &str, input: &str) -> Vec<Box<[Option<(usize, usize)>]>> {
        compile(regex).matches(input.as_bytes(), None)
    }
    fn matches_exact(regex: &str, input: &str) -> Option<Box<[Option<(usize, usize)>]>> {
        compile(regex).matches_exact(input.as_bytes())
    }

    #[test]
    fn basic() {
        assert!(matches_exact("abc", "abc").is_some());
        assert!(matches_exact("abc", "bbc").is_none());
        assert!(matches_exact("abc", "acc").is_none());
        assert!(matches_exact("abc", "abd").is_none());
    }
    #[test]
    fn repetitions() {
        assert!(matches_exact("abc*", "ab").is_some());
        assert!(matches_exact("abc*", "abc").is_some());
        assert!(matches_exact("abc*", "abccc").is_some());

        assert!(matches_exact(r"a\{1,2\}b", "b").is_none());
        assert!(matches_exact(r"a\{1,2\}b", "ab").is_some());
        assert!(matches_exact(r"a\{1,2\}b", "aab").is_some());
        assert!(matches_exact(r"a\{1,2\}b", "aaab").is_none());

        assert!(matches_exact(r"[abc]\{3\}", "abcTRAILING").is_some());
        assert!(matches_exact(r"[abc]\{3\}", "abTRAILING").is_none());
    }
    #[test]
    fn any() {
        assert!(matches_exact(".*", "").is_some());
        assert!(matches_exact(".*b", "b").is_some());
        assert!(matches_exact(".*b", "ab").is_some());
        assert!(matches_exact(".*b", "aaaaab").is_some());
        assert!(matches_exact(".*b", "HELLO WORLD").is_none());
        assert!(matches_exact(".*b", "HELLO WORLDb").is_some());
        assert!(matches_exact("H.*O WORLD", "HELLO WORLD").is_some());
        assert!(matches_exact("H.*ORLD", "HELLO WORLD").is_some());
    }
    #[test]
    fn brackets() {
        assert!(matches_exact("[abc]*d", "abcd").is_some());
        assert!(matches_exact("[0-9]*d", "1234d").is_some());
        assert!(matches_exact("[[:digit:]]*d", "1234d").is_some());
        assert!(matches_exact("[[:digit:]]*d", "abcd").is_none());
    }
    #[test]
    fn alternations() {
        assert!(matches_exact(r"abc\|bcd", "abc").is_some());
        assert!(matches_exact(r"abc\|bcd", "bcd").is_some());
        assert!(matches_exact(r"abc\|bcd", "cde").is_none());
        assert!(matches_exact(r"[A-Z]\+\|yee", "").is_none());
        assert!(matches_exact(r"[A-Z]\+\|yee", "HELLO").is_some());
        assert!(matches_exact(r"[A-Z]\+\|yee", "yee").is_some());
        assert!(matches_exact(r"[A-Z]\+\|yee", "hello").is_none());
    }
    #[test]
    fn offsets() {
        assert_eq!(matches_exact("abc", "abcd"), Some(abox![Some((0, 3))]));
        assert_eq!(
            matches_exact(r"[[:alpha:]]\+", "abcde12345"),
            Some(abox![Some((0, 5))])
        );
        assert_eq!(
            matches_exact(r"a\(bc\)\+d", "abcbcd"),
            Some(abox![Some((0, 6)), Some((3, 5))])
        );
        assert_eq!(
            matches_exact(r"hello\( \(world\|universe\) :D\)\?!", "hello world :D!"),
            Some(abox![Some((0, 15)), Some((5, 14)), Some((6, 11))])
        );
        assert_eq!(
            matches_exact(r"hello\( \(world\|universe\) :D\)\?", "hello world :D"),
            Some(abox![Some((0, 14)), Some((5, 14)), Some((6, 11))])
        );
        assert_eq!(
            matches_exact(r"\(\<hello\>\) world", "hello world"),
            Some(abox![Some((0, 11)), Some((0, 5))])
        );
        assert_eq!(
            matches_exact(r".*d", "hid howd ared youd"),
            Some(abox![Some((0, 18))])
        );
        assert_eq!(
            matches_exact(r".*\(a\)", "bbbbba"),
            Some(abox![Some((0, 6)), Some((5, 6))])
        );
        assert_eq!(
            matches_exact(r"\(a \(b\) \(c\)\) \(d\)", "a b c d"),
            Some(abox![
                Some((0, 7)),
                Some((0, 5)),
                Some((2, 3)),
                Some((4, 5)),
                Some((6, 7))
            ])
        );
        assert_eq!(
            matches_exact(r"\(.\)*", "hello"),
            Some(abox![Some((0, 5)), Some((4, 5))])
        );
        assert_eq!(
            matches(r"h\(i\)", "hello hi lol"),
            vec![abox![Some((6, 8)), Some((7, 8))]]
        );
        assert_eq!(
            matches_exact(r"\(\([[:alpha:]]\)*\)", "abcdefg"),
            Some(abox![Some((0, 7)), Some((0, 7)), Some((6, 7))])
        );
        assert_eq!(
            matches_exact(r"\(\.\([[:alpha:]]\)\)*", ".a.b.c.d.e.f.g"),
            Some(abox![Some((0, 14)), Some((12, 14)), Some((13, 14))])
        );
        assert_eq!(
            matches_exact(r"\(a\|\(b\)\)*\(c\)", "bababac"),
            Some(abox![
                Some((0, 7)),
                Some((5, 6)),
                Some((4, 5)),
                Some((6, 7))
            ])
        );
        assert_eq!(
            matches_exact(r"\(a\|\(b\)\)*\(c\)", "aaac"),
            Some(abox![Some((0, 4)), Some((2, 3)), None, Some((3, 4))])
        );
        assert_eq!(
            matches_exact(r"a\(\)bc", "abc"),
            Some(abox![Some((0, 3)), Some((1, 1))])
        );
    }
    #[test]
    fn matches_is_lazy() {
        assert_eq!(
            matches(r"\(hi\)\+", "hello hihi kek"),
            vec![abox![Some((6, 10)), Some((8, 10))]]
        );
        assert_eq!(
            matches(r"o\+", "helloooooooo woooorld, hooow are you?"),
            vec![
                abox![Some((4, 12))],
                abox![Some((14, 18))],
                abox![Some((24, 27))],
                abox![Some((34, 35))]
            ]
        );
        assert_eq!(
            matches(r"z*", "abc"),
            vec![
                abox![Some((0, 0))],
                abox![Some((1, 1))],
                abox![Some((2, 2))],
                abox![Some((3, 3))]
            ]
        );
    }
    #[test]
    fn start_and_end() {
        assert!(matches_exact("^abc$", "abc").is_some());
        assert!(matches_exact("^bcd", "bcde").is_some());
        assert!(matches_exact("^bcd", "abcd").is_none());
        assert!(matches_exact("abc$", "abc").is_some());
        assert!(matches_exact("abc$", "abcd").is_none());

        assert!(matches_exact(r".*\(^\|a\)c", "c").is_some());
        assert!(matches_exact(r".*\(^\|a\)c", "ac").is_some());
        assert!(matches_exact(r".*\(^\|a\)c", "bc").is_none());

        // Tests if ^ can be repeated without issues
        assert!(matches_exact(".*^^a", "helloabc").is_none());
        assert!(matches_exact(".*^^a", "abc").is_some());
    }
    #[test]
    fn word_boundaries() {
        assert!(matches_exact(r"hello\>.world", "hello world").is_some());
        assert!(matches_exact(r"hello\>.world", "hello!world").is_some());
        assert!(matches_exact(r"hello\>.world", "hellooworld").is_none());

        assert!(matches_exact(r"hello.\<world", "hello world").is_some());
        assert!(matches_exact(r"hello.\<world", "hello!world").is_some());
        assert!(matches_exact(r"hello.\<world", "hellooworld").is_none());

        assert!(matches_exact(r".*\<hello\>", "hihello").is_none());
        assert!(matches_exact(r".*\<hello\>", "hi_hello").is_none());
        assert!(matches_exact(r".*\<hello\>", "hi hello").is_some());
    }
    #[test]
    fn groups() {
        assert!(matches_exact(r"\(a*\)*", "aaaaa").is_some());
        assert!(matches_exact(r"\(hello\) world", "hello world").is_some());
        assert!(matches_exact(r"\(a*\|b\|c\)d", "d").is_some());
        assert!(matches_exact(r"\(a*\|b\|c\)d", "aaaad").is_some());
        assert!(matches_exact(r"\(a*\|b\|c\)d", "bd").is_some());
        assert!(matches_exact(r"\(a*\|b\|c\)d", "bbbbbd").is_none());
    }
    #[test]
    fn repeating_groups() {
        assert!(matches_exact(r"\(a\|b\|c\)*d", "d").is_some());
        assert!(matches_exact(r"\(a\|b\|c\)*d", "aaaad").is_some());
        assert!(matches_exact(r"\(a\|b\|c\)*d", "bbbbd").is_some());
        assert!(matches_exact(r"\(a\|b\|c\)*d", "aabbd").is_some());

        assert!(matches_exact(r"\(a\|b\|c\)\{1,2\}d", "d").is_none());
        assert!(matches_exact(r"\(a\|b\|c\)\{1,2\}d", "ad").is_some());
        assert!(matches_exact(r"\(a\|b\|c\)\{1,2\}d", "abd").is_some());
        assert!(matches_exact(r"\(a\|b\|c\)\{1,2\}d", "abcd").is_none());
        assert!(matches_exact(r"\(\(a\|b\|c\)\)\{1,2\}d", "abd").is_some());
        assert!(matches_exact(r"\(\(a\|b\|c\)\)\{1,2\}d", "abcd").is_none());
        assert!(matches_exact(r"\(\(a\|b\|c\)\{1,2\}\)\{1,2\}d", "abad").is_some());
        assert!(matches_exact(r"\(\(a\|b\|c\)\{1,2\}\)\{1,2\}d", "ababd").is_some());
        assert!(matches_exact(r"\(\(a\|b\|c\)\{1,2\}\)\{1,2\}d", "ababad").is_none());

        assert!(matches_exact(r"\(a\|b\|c\)\{4\}d", "ababad").is_none());
        assert!(matches_exact(r"\(a\|b\|c\)\{4\}d", "ababd").is_some());
        assert!(matches_exact(r"\(a\|b\|c\)\{4\}d", "abad").is_none());

        assert!(matches_exact(r"\(\([abc]\)\)\{3\}", "abcTRAILING").is_some());
        assert!(matches_exact(r"\(\([abc]\)\)\{3\}", "abTRAILING").is_none());
    }
    #[test]
    fn backref() {
        assert!(matches_exact(r"\([abc]\)\1d", "aad").is_some());
        assert!(matches_exact(r"\([abc]\)\1d", "abd").is_none());
        assert!(matches_exact(r"\([abc]\{2,3\}\)\1d", "abcabcd").is_some());
        assert!(matches_exact(r"\([abc]\{2,3\}\)\1d", "abcbcd").is_none());
        assert!(matches_exact(r"\([abc]\{2,3\}\)\1d", "ababd").is_some());
        assert!(matches_exact(r"\([abc]\{2,3\}\)\1d", "abacd").is_none());

        assert!(matches_exact(r"\([[:alpha:]]\).*\1d", "hellohd").is_some());
        assert!(matches_exact(r"\([[:alpha:]]\).*\1d", "hellod").is_none());
        assert!(matches_exact(r"\([[:alpha:]]\).*\1", "hello").is_none());
        assert!(matches_exact(r"\([[:alpha:]]\).*\1", "helloh").is_some());

        assert!(matches_exact(r"\(\)-\?\1d", "d").is_some());
        assert!(matches_exact(r"\(\)-\?\1", "").is_some());

        // Just make sure this doesn't crash it (even though it should error
        // but I'm too lazy)
        assert!(matches_exact(r"\(\1\)", "a").is_none());

        assert!(matches_exact(r"\(h.\)\1\+!", "hihihi!").is_some());
        assert!(matches_exact(r"\(h.\)\1\+!", "hehehe!").is_some());
        assert!(matches_exact(r"\(h.\)\1\+!", "hahehe!").is_none());

        assert!(matches_exact(
            r"\(hello \(\<.*\>\) \)*how are you \2",
            "hello world how are you world"
        )
        .is_some());
        assert!(matches_exact(
            r"\(hello \(\<.*\>\) \)*how are you \2",
            "hello universe hello world how are you world"
        )
        .is_some());
        assert!(matches_exact(
            r"\(hello \(\<.*\>\) \)*how are you \2",
            "hello world hello universe how are you world"
        )
        .is_none());
    }
    #[test]
    fn case_insensitive() {
        assert!(compile(r"abc[de]")
            .case_insensitive(true)
            .matches_exact(b"ABCD")
            .is_some());
        assert!(compile(r"abc[de]")
            .case_insensitive(true)
            .matches_exact(b"ABCF")
            .is_none());
    }
    #[test]
    fn newline() {
        assert_eq!(
            compile(r"^hello$")
                .newline(true)
                .matches(b"hi\nhello\ngreetings", None)
                .len(),
            1
        );
        assert!(compile(r"^hello$")
            .newline(true)
            .matches(b"hi\ngood day\ngreetings", None)
            .is_empty());
    }
    #[test]
    fn no_start_end() {
        assert!(compile(r"^hello")
            .no_start(true)
            .matches_exact(b"hello")
            .is_none());
        assert!(compile(r"hello$")
            .no_end(true)
            .matches_exact(b"hello")
            .is_none());
    }

    #[cfg(feature = "bench")]
    #[bench]
    fn speed_matches_exact(b: &mut Bencher) {
        b.iter(|| {
            assert!(matches_exact(r"\(\(a*\|b\|c\) test\|yee\)", "aaaaa test").is_some());
        })
    }
    #[cfg(feature = "bench")]
    #[bench]
    fn speed_matches(b: &mut Bencher) {
        b.iter(|| {
            assert_eq!(
                matches(r"\(\(a*\|b\|c\) test\|yee\)", "oooo aaaaa test").len(),
                1
            );
        })
    }
}

//! Paste buffers: a server-global stack of copied text (tmux's paste buffers).
//!
//! Every yank from copy-mode pushes onto this stack as the new "top" buffer.
//! tmux auto-names them `buffer0`, `buffer1`, … with `buffer0` always the most
//! recent. Pasting (prefix `]`) inserts the top buffer's text into the active
//! pane; the buffer chooser (prefix `=`) lists them so the user can pick an
//! older one. A bounded depth keeps memory from growing without limit.

/// Maximum number of buffers retained (tmux's `buffer-limit` default is 50).
const BUFFER_LIMIT: usize = 50;

/// One stored buffer: its text plus the auto-assigned ordinal used to build a
/// stable tmux-style name (`buffer<N>`). The ordinal is monotonic, so a buffer
/// keeps its name as newer ones are added in front of it. A buffer may also
/// carry an explicit user-assigned name (tmux `set-buffer -b name`), which takes
/// precedence over the ordinal name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    /// Monotonic id assigned at push time; never reused within a run.
    pub ordinal: u64,
    pub text: String,
    /// User-assigned name (tmux `-b`), if any. When set, this is the buffer's
    /// name instead of `buffer<ordinal>`.
    pub explicit_name: Option<String>,
}

impl Buffer {
    /// The buffer's name: the explicit name if set, else the tmux-style
    /// `buffer<ordinal>` (e.g. `buffer3`).
    pub fn name(&self) -> String {
        self.explicit_name
            .clone()
            .unwrap_or_else(|| format!("buffer{}", self.ordinal))
    }
}

/// A bounded, most-recent-first stack of paste buffers.
#[derive(Debug, Clone, Default)]
pub struct PasteBuffers {
    /// Front (index 0) is the most recent. Bounded to [`BUFFER_LIMIT`].
    buffers: Vec<Buffer>,
    /// Next ordinal to assign.
    next_ordinal: u64,
}

impl PasteBuffers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there are no buffers (nothing to paste).
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    /// Push `text` as the new most-recent buffer (tmux `set-buffer` / a yank).
    /// Empty text is ignored. Returns the new buffer's name. The oldest buffer
    /// is dropped once the limit is exceeded.
    pub fn push(&mut self, text: impl Into<String>) -> Option<String> {
        let text = text.into();
        if text.is_empty() {
            return None;
        }
        let buf = Buffer {
            ordinal: self.next_ordinal,
            text,
            explicit_name: None,
        };
        self.next_ordinal += 1;
        let name = buf.name();
        self.buffers.insert(0, buf);
        self.buffers.truncate(BUFFER_LIMIT);
        Some(name)
    }

    /// Store `text` under an explicit `name` (tmux `set-buffer -b name`). If a
    /// buffer with that name already exists it is replaced in place (keeping its
    /// position); otherwise a new most-recent buffer is created. Empty text is
    /// ignored. Returns the name.
    pub fn push_named(&mut self, name: impl Into<String>, text: impl Into<String>) -> Option<String> {
        let name = name.into();
        let text = text.into();
        if text.is_empty() {
            return None;
        }
        if let Some(b) = self.buffers.iter_mut().find(|b| b.name() == name) {
            b.text = text;
            return Some(name);
        }
        let buf = Buffer {
            ordinal: self.next_ordinal,
            text,
            explicit_name: Some(name.clone()),
        };
        self.next_ordinal += 1;
        self.buffers.insert(0, buf);
        self.buffers.truncate(BUFFER_LIMIT);
        Some(name)
    }

    /// The text of the buffer named `name` (explicit or `buffer<N>`), if present.
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.buffers.iter().find(|b| b.name() == name).map(|b| b.text.as_str())
    }

    /// Delete the buffer named `name`. Returns true if one was removed.
    pub fn delete_named(&mut self, name: &str) -> bool {
        if let Some(pos) = self.buffers.iter().position(|b| b.name() == name) {
            self.buffers.remove(pos);
            true
        } else {
            false
        }
    }

    /// The most-recent buffer's text (tmux's default `paste-buffer` target).
    pub fn top(&self) -> Option<&str> {
        self.buffers.first().map(|b| b.text.as_str())
    }

    /// The buffer at display index `i` (0 = most recent), for the chooser.
    pub fn get(&self, i: usize) -> Option<&Buffer> {
        self.buffers.get(i)
    }

    /// All buffers, most-recent first (for the chooser list).
    pub fn iter(&self) -> impl Iterator<Item = &Buffer> {
        self.buffers.iter()
    }

    /// Delete the buffer at display index `i`. Returns true if one was removed.
    pub fn delete(&mut self, i: usize) -> bool {
        if i < self.buffers.len() {
            self.buffers.remove(i);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_makes_most_recent_the_top() {
        let mut b = PasteBuffers::new();
        b.push("first");
        b.push("second");
        assert_eq!(b.top(), Some("second"));
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn empty_push_is_ignored() {
        let mut b = PasteBuffers::new();
        assert_eq!(b.push(""), None);
        assert!(b.is_empty());
        assert_eq!(b.top(), None);
    }

    #[test]
    fn names_are_stable_and_monotonic() {
        let mut b = PasteBuffers::new();
        assert_eq!(b.push("a"), Some("buffer0".to_string()));
        assert_eq!(b.push("b"), Some("buffer1".to_string()));
        // The older buffer keeps buffer0 even though it's now at index 1.
        assert_eq!(b.get(1).unwrap().name(), "buffer0");
        assert_eq!(b.get(0).unwrap().name(), "buffer1");
    }

    #[test]
    fn delete_removes_by_index() {
        let mut b = PasteBuffers::new();
        b.push("a");
        b.push("b");
        b.push("c"); // order (idx): c(0) b(1) a(2)
        assert!(b.delete(1)); // remove b
        assert_eq!(b.len(), 2);
        assert_eq!(b.get(0).unwrap().text, "c");
        assert_eq!(b.get(1).unwrap().text, "a");
        assert!(!b.delete(5)); // out of range
    }

    #[test]
    fn stack_is_bounded() {
        let mut b = PasteBuffers::new();
        for i in 0..(BUFFER_LIMIT + 10) {
            b.push(format!("line{i}"));
        }
        assert_eq!(b.len(), BUFFER_LIMIT);
        // The most-recent push is still on top.
        assert_eq!(b.top(), Some(format!("line{}", BUFFER_LIMIT + 9).as_str()));
    }

    #[test]
    fn named_buffer_stores_and_looks_up_by_name() {
        let mut b = PasteBuffers::new();
        assert_eq!(b.push_named("greeting", "hello"), Some("greeting".to_string()));
        assert_eq!(b.text_of("greeting"), Some("hello"));
        // The explicit name is used, not buffer<N>.
        assert_eq!(b.get(0).unwrap().name(), "greeting");
    }

    #[test]
    fn named_buffer_replaces_in_place() {
        let mut b = PasteBuffers::new();
        b.push_named("x", "one");
        b.push("other");
        b.push_named("x", "two"); // replace, not add
        assert_eq!(b.text_of("x"), Some("two"));
        // Only two buffers: "other" and the replaced "x".
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn delete_named_removes_the_named_buffer() {
        let mut b = PasteBuffers::new();
        b.push_named("keep", "a");
        b.push_named("drop", "b");
        assert!(b.delete_named("drop"));
        assert_eq!(b.text_of("drop"), None);
        assert_eq!(b.text_of("keep"), Some("a"));
        assert!(!b.delete_named("nope"));
    }

    #[test]
    fn text_of_finds_auto_named_buffers_too() {
        let mut b = PasteBuffers::new();
        b.push("auto"); // buffer0
        assert_eq!(b.text_of("buffer0"), Some("auto"));
    }
}

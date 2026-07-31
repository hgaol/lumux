//! Keymap + prefix state machine + copy-mode state. (Phase 6)

mod bindings;
mod key;
mod machine;

pub use bindings::{encode_key, Action, Bindings, MenuScope};
pub use key::{decode_key, Key, KeyCode};
pub use machine::{
    BufferKey, CopyKey, HelpKey, Keymap, Mode, ModeKeys, PromptKey, Reaction, SearchKey,
    SessionKey, PASTE_DISABLE, PASTE_ENABLE, PASTE_END, PASTE_START,
};

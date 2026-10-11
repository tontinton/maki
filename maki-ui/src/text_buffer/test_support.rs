use maki_lua::TextEdit;

pub(crate) fn text_edit(start: usize, stop: usize, text: &str) -> TextEdit {
    TextEdit {
        start,
        stop,
        text: text.into(),
    }
}

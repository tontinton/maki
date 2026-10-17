use maki_lua::{CodeBlockRender, EventHandle};

const MAX_ENTRIES: usize = 128;
const MAX_SOURCE_BYTES: usize = 32 * 1024;

enum RenderedBlock {
    Pending(CodeBlockRender),
    Ready(Option<Vec<String>>),
}

struct Entry {
    language: String,
    source: String,
    rendered: RenderedBlock,
}

pub(super) struct CodeBlockRenders {
    entries: Vec<Entry>,
    width: u16,
    theme: u64,
    generation: u64,
    layered: bool,
}

impl CodeBlockRenders {
    pub fn new(handle: &EventHandle) -> Self {
        Self {
            entries: Vec::new(),
            width: 0,
            theme: 0,
            generation: handle.code_block_generation(),
            layered: handle.code_block_layered(),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn set_layout(&mut self, width: u16, theme: u64) -> bool {
        if self.width == width && self.theme == theme {
            return false;
        }
        let changed = !self.entries.is_empty();
        self.width = width;
        self.theme = theme;
        self.clear();
        changed
    }

    pub fn poll(&mut self, handle: &EventHandle) -> bool {
        let generation = handle.code_block_generation();
        let layered = handle.code_block_layered();
        if self.generation != generation || self.layered != layered {
            let changed = self.layered || layered;
            self.generation = generation;
            self.layered = layered;
            self.clear();
            return changed;
        }
        let mut changed = false;
        for entry in &mut self.entries {
            let RenderedBlock::Pending(request) = &entry.rendered else {
                continue;
            };
            let result = match request.try_recv() {
                Ok(result) => result,
                Err(flume::TryRecvError::Disconnected) => None,
                Err(flume::TryRecvError::Empty) => continue,
            };
            entry.rendered = RenderedBlock::Ready(result);
            changed = true;
        }
        changed
    }

    pub fn has_pending(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.rendered, RenderedBlock::Pending(_)))
    }

    pub fn render(
        &mut self,
        handle: &EventHandle,
        language: &str,
        source: &str,
        width: u16,
    ) -> Option<Vec<String>> {
        if !self.layered || width == 0 || source.len() > MAX_SOURCE_BYTES {
            return None;
        }
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.language == language && entry.source == source)
        {
            return match &entry.rendered {
                RenderedBlock::Ready(lines) => lines.clone(),
                RenderedBlock::Pending(_) => None,
            };
        }
        if self.entries.len() == MAX_ENTRIES {
            return None;
        }
        let request = handle.render_code_block(language.to_owned(), source.to_owned(), width);
        self.entries.push(Entry {
            language: language.to_owned(),
            source: source.to_owned(),
            rendered: RenderedBlock::Pending(request),
        });
        None
    }
}

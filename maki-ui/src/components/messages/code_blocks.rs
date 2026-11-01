use maki_lua::{CodeBlockRender, EventHandle, MAX_CODE_BLOCK_SOURCE_BYTES};

pub(super) const MAX_ENTRIES: usize = 128;

enum RenderedBlock {
    Pending(CodeBlockRender),
    Ready(Option<Vec<String>>),
}

struct Entry {
    language: String,
    source: String,
    position: (usize, usize),
    rendered: RenderedBlock,
}

pub(super) struct CodeBlockRenders {
    entries: Vec<Entry>,
    width: u16,
    theme: u64,
    generation: u64,
}

impl CodeBlockRenders {
    pub fn new(handle: &EventHandle) -> Self {
        Self {
            entries: Vec::new(),
            width: 0,
            theme: 0,
            generation: handle.code_block_generation(),
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
        if self.generation != generation {
            self.generation = generation;
            self.clear();
            return true;
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
        position: (usize, usize),
        language: &str,
        source: &str,
        width: u16,
    ) -> Option<Vec<String>> {
        if width == 0
            || source.len() > MAX_CODE_BLOCK_SOURCE_BYTES
            || !handle.code_block_layered(language)
        {
            return None;
        }
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.language == language && entry.source == source)
        {
            entry.position = entry.position.max(position);
            return match &entry.rendered {
                RenderedBlock::Ready(lines) => lines.clone(),
                RenderedBlock::Pending(_) => None,
            };
        }
        if self.entries.len() == MAX_ENTRIES {
            let (oldest, entry) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.position)?;
            // Reflow must not let an evicted fence displace newer fences.
            if position <= entry.position {
                return None;
            }
            self.entries.swap_remove(oldest);
        }
        let request = handle.render_code_block(language.to_owned(), source.to_owned(), width);
        self.entries.push(Entry {
            language: language.to_owned(),
            source: source.to_owned(),
            position,
            rendered: RenderedBlock::Pending(request),
        });
        None
    }
}

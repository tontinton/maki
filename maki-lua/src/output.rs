use std::collections::VecDeque;
use std::io::BufRead;
use std::sync::{Arc, Mutex};

use maki_config::{DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_OUTPUT_LINES};
use mlua::{
    FromLuaMulti, IntoLuaMulti, Lua, LuaString, ObjectLike, Result as LuaResult, Table, UserData,
    UserDataMethods, Value,
};

pub(crate) const DEFAULT_MAX_LINE_BYTES: usize = 500;
const UTF8_MAX_BYTES: usize = 4;
const PREVIEW_TRUNCATION: &str = "…";

#[derive(Default)]
struct Limits {
    lines: Option<usize>,
    bytes: Option<usize>,
}

fn prefix(text: &str, bytes: usize) -> &str {
    &text[..text.floor_char_boundary(bytes.min(text.len()))]
}

fn truncate(text: &str, limits: &Limits, total: usize) -> String {
    let max_lines = limits.lines.unwrap_or(usize::MAX);
    let max_bytes = limits.bytes.unwrap_or(usize::MAX);
    let kept = if total <= max_bytes && text.split('\n').count() <= max_lines {
        text
    } else {
        let mut used = 0usize;
        let mut end = 0;
        for (index, line) in text.split('\n').take(max_lines).enumerate() {
            let next = used.saturating_add(line.len()).saturating_add(1);
            if next > max_bytes {
                if index == 0 {
                    end = prefix(line, max_bytes).len();
                }
                break;
            }
            end += usize::from(index > 0) + line.len();
            used = next;
        }
        &text[..end]
    };
    with_notice(kept, total)
}

fn with_notice(kept: &str, total: usize) -> String {
    if kept.len() < total {
        format!("{kept}\n\n[truncated {} bytes]", total - kept.len())
    } else {
        kept.to_owned()
    }
}

fn tail(text: &str, lines: usize) -> &str {
    if lines == 0 {
        return text;
    }
    text.rmatch_indices('\n')
        .nth(lines.saturating_sub(1))
        .map_or(text, |(index, _)| &text[index + 1..])
}

fn call_method<A: IntoLuaMulti, R: FromLuaMulti>(ctx: &Value, name: &str, args: A) -> LuaResult<R> {
    match ctx {
        Value::Table(ctx) => ctx.call_method(name, args),
        Value::UserData(ctx) => ctx.call_method(name, args),
        _ => Err(mlua::Error::runtime("output limits require a tool context")),
    }
}

fn resolve(opts: &Table, ctx: &Value) -> LuaResult<Limits> {
    if call_method::<_, String>(ctx, "output_mode", ())? == "full" {
        return Ok(Limits::default());
    }
    Ok(Limits {
        lines: Some(match opts.get::<Option<usize>>("max_output_lines")? {
            Some(value) => value,
            None => call_method(
                ctx,
                "config",
                ("max_output_lines", DEFAULT_MAX_OUTPUT_LINES),
            )?,
        }),
        bytes: Some(match opts.get::<Option<usize>>("max_output_bytes")? {
            Some(value) => value,
            None => call_method(
                ctx,
                "config",
                ("max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES),
            )?,
        }),
    })
}

struct Line {
    text: Option<String>,
    bytes: usize,
}

struct Captured {
    limits: Limits,
    tail: Option<usize>,
    lines: VecDeque<Line>,
    retained: usize,
    total: usize,
    line_count: usize,
}

impl Captured {
    fn capacity(&self) -> usize {
        self.limits
            .bytes
            .map_or(usize::MAX, |n| n.saturating_add(UTF8_MAX_BYTES))
    }

    fn append(&mut self, text: &str, bytes: usize) {
        self.total = self
            .total
            .saturating_add(bytes)
            .saturating_add(usize::from(self.line_count > 0));
        self.line_count = self.line_count.saturating_add(1);
        let capacity = self.capacity();
        let max_lines = self.limits.lines.unwrap_or(usize::MAX).min(
            self.limits
                .bytes
                .map_or(usize::MAX, |bytes| bytes.saturating_add(1)),
        );
        if self.tail.is_none() && (self.lines.len() >= max_lines || self.retained >= capacity) {
            return;
        }
        let available = if self.tail.is_some() {
            capacity
        } else {
            capacity.saturating_sub(self.retained + usize::from(!self.lines.is_empty()))
        };
        let text = prefix(text, available).to_owned();
        self.retained += text.len();
        self.lines.push_back(Line {
            text: Some(text),
            bytes,
        });
        if let Some(tail) = self.tail {
            while self.lines.len() > tail.min(max_lines) {
                if let Some(line) = self.lines.pop_front() {
                    self.retained -= line.text.map_or(0, |text| text.len());
                }
            }
            let budget = self.limits.bytes.unwrap_or(usize::MAX);
            let mut present = self.lines.iter().filter(|line| line.text.is_some()).count();
            for line in &mut self.lines {
                if present <= 1 || self.retained.saturating_add(present - 1) <= budget {
                    break;
                }
                if let Some(text) = line.text.take() {
                    self.retained -= text.len();
                    present -= 1;
                }
            }
        }
    }

    fn result(&self) -> String {
        let text = self
            .lines
            .iter()
            .filter_map(|line| line.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        let total = if self.tail.is_some() && !self.lines.is_empty() {
            self.lines.iter().map(|line| line.bytes).sum::<usize>()
                + self.lines.len().saturating_sub(1)
        } else {
            self.total
        };
        if let Some(tail) = self.tail {
            let mut result = with_notice(
                prefix(&text, self.limits.bytes.unwrap_or(usize::MAX)),
                total,
            );
            let dropped_lines = self.line_count.min(tail).saturating_sub(self.lines.len());
            if dropped_lines > 0 {
                result.push_str(&format!(
                    "\n\n[truncated {dropped_lines} lines from requested tail]"
                ));
            }
            result
        } else {
            truncate(&text, &self.limits, total)
        }
    }
}

#[derive(Clone)]
pub(crate) struct OutputCapture(Arc<Mutex<Captured>>);

impl OutputCapture {
    fn new(limits: Limits, tail: Option<usize>) -> Self {
        Self(Arc::new(Mutex::new(Captured {
            limits,
            tail,
            lines: VecDeque::new(),
            retained: 0,
            total: 0,
            line_count: 0,
        })))
    }

    pub(crate) fn read_lines(
        &self,
        mut reader: impl BufRead,
        mut preview: impl FnMut(String) -> bool,
    ) {
        let mut line = Vec::new();
        let mut bytes = 0usize;
        let mut last = None;
        let limit = self.line_limit();
        while let Ok(chunk) = reader.fill_buf() {
            let newline = chunk.iter().position(|byte| *byte == b'\n');
            let end = newline.unwrap_or(chunk.len());
            let take = end.min(limit.saturating_sub(line.len()));
            line.extend_from_slice(&chunk[..take]);
            bytes = bytes.saturating_add(end);
            last = chunk[..end].last().copied().or(last);
            let eof = chunk.is_empty();
            reader.consume(end + usize::from(newline.is_some()));
            if newline.is_none() && !eof {
                continue;
            }
            if eof && bytes == 0 {
                break;
            }
            let text_bytes =
                bytes.saturating_sub(usize::from(newline.is_some() && last == Some(b'\r')));
            line.truncate(text_bytes);
            let text = String::from_utf8_lossy(&line);
            self.append(&text, text_bytes);
            let preview_end = text.floor_char_boundary(DEFAULT_MAX_LINE_BYTES.min(text.len()));
            let mut shown = text[..preview_end].to_owned();
            if text_bytes > preview_end {
                shown.push_str(PREVIEW_TRUNCATION);
            }
            if !preview(shown) {
                break;
            }
            line.clear();
            bytes = 0;
            last = None;
            if eof {
                break;
            }
        }
    }

    fn line_limit(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).capacity()
    }

    fn append(&self, text: &str, bytes: usize) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(text, bytes);
    }

    fn result(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).result()
    }
}

impl UserData for OutputCapture {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("append", |_, this, text: LuaString| {
            let text = text.to_str()?;
            this.append(&text, text.len());
            Ok(())
        });
        methods.add_method("result", |_, this, ()| Ok(this.result()));
    }
}

pub(crate) fn module(lua: &Lua, name: &str) -> Option<LuaResult<Value>> {
    match name {
        "maki.truncate" => Some(
            lua.create_function(
                |_, (text, lines, bytes): (LuaString, Option<usize>, Option<usize>)| {
                    let text = text.to_str()?;
                    Ok(truncate(&text, &Limits { lines, bytes }, text.len()))
                },
            )
            .map(Value::Function),
        ),
        "maki.output_limits" => Some(limits_module(lua).map(Value::Table)),
        _ => None,
    }
}

fn limits_module(lua: &Lua) -> LuaResult<Table> {
    let module = lua.create_table()?;
    let specs = lua.create_table()?;
    for name in ["max_output_lines", "max_output_bytes"] {
        let spec = lua.create_table()?;
        spec.set("type", "integer")?;
        spec.set("desc", format!("Override `agent.{name}` for this tool."))?;
        specs.set(name, spec)?;
    }
    module.set("DEFAULT_MAX_LINE_BYTES", DEFAULT_MAX_LINE_BYTES)?;
    module.set("specs", specs.clone())?;
    module.set(
        "extend",
        lua.create_function(move |_, spec: Table| {
            for pair in specs.pairs::<Value, Value>() {
                let (key, value) = pair?;
                spec.set(key, value)?;
            }
            Ok(spec)
        })?,
    )?;
    module.set(
        "resolve",
        lua.create_function(|_, (opts, ctx): (Table, Value)| {
            let limits = resolve(&opts, &ctx)?;
            Ok((limits.lines, limits.bytes))
        })?,
    )?;
    module.set(
        "line_bytes",
        lua.create_function(|_, (opts, ctx): (Table, Value)| {
            if call_method::<_, String>(&ctx, "output_mode", ())? == "full" {
                Ok(Some(0))
            } else {
                opts.get::<Option<usize>>("max_line_bytes")
            }
        })?,
    )?;
    module.set(
        "tail",
        lua.create_function(|_, (text, lines): (LuaString, usize)| {
            Ok(tail(&text.to_str()?, lines).to_owned())
        })?,
    )?;
    module.set(
        "collect",
        lua.create_function(|_, (opts, ctx, tail): (Table, Value, Option<usize>)| {
            if tail == Some(0) {
                return Err(mlua::Error::runtime(
                    "output tail must be at least one line",
                ));
            }
            Ok(OutputCapture::new(resolve(&opts, &ctx)?, tail))
        })?,
    )?;
    Ok(module)
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};
    use std::thread;

    use mlua::{Function, Lua, Table, Value};
    use test_case::test_case;

    use super::{
        DEFAULT_MAX_LINE_BYTES, Limits, OutputCapture, PREVIEW_TRUNCATION, module, truncate,
    };

    const TRUNCATED: &str = "[truncated";
    const FIRST_LINE: &str = "first";

    #[test_case("hello", 100, 1000, "hello" ; "within_limits")]
    #[test_case("aaa\nbbb\nccc\nddd", 2, 1000, "aaa\nbbb\n\n[truncated 8 bytes]" ; "line_limit")]
    #[test_case("éééééééééé", 1000, 5, "éé\n\n[truncated 16 bytes]" ; "utf8_prefix")]
    #[test_case("short\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\nlast", 1000, 20, "short\n\n[truncated 106 bytes]" ; "drop_later_oversized_line")]
    #[test_case("a\nb", 2, 3, "a\nb" ; "exact_bytes")]
    #[test_case("", 1, 1, "" ; "empty")]
    fn truncation(text: &str, lines: usize, bytes: usize, expected: &str) {
        let limits = Limits {
            lines: Some(lines),
            bytes: Some(bytes),
        };
        assert_eq!(truncate(text, &limits, text.len()), expected);
    }

    #[test_case(None ; "head")]
    #[test_case(Some(3) ; "tail")]
    fn retained_storage_is_bounded(tail: Option<usize>) {
        const BYTES: usize = 1024;
        const LINES: usize = 8;
        let capture = OutputCapture::new(
            Limits {
                lines: Some(LINES),
                bytes: Some(BYTES),
            },
            tail,
        );
        for _ in 0..10000 {
            capture.append(&"x".repeat(BYTES), BYTES);
        }
        let state = capture.0.lock().unwrap();
        assert!(state.retained <= state.capacity());
        assert!(state.lines.len() <= LINES);
        assert!(state.result().contains(TRUNCATED));
    }

    #[test_case("a\nb\nc", None, "a\nb\nc" ; "prefix")]
    #[test_case("a\nb\nc", Some(2), "b\nc" ; "tail")]
    #[test_case("a\nb\n", Some(1), "" ; "empty_tail")]
    fn collection(text: &str, tail: Option<usize>, expected: &str) {
        let capture = OutputCapture::new(
            Limits {
                lines: Some(8),
                bytes: Some(1024),
            },
            tail,
        );
        for line in text.split('\n') {
            capture.append(line, line.len());
        }
        assert_eq!(capture.result(), expected);
    }

    #[test_case("old-aaaa\nnew-bbbb", "new-bbbb\n\n[truncated 9 bytes]" ; "oversized_lines")]
    #[test_case("old-aa\nnew-bb", "new-bb\n\n[truncated 7 bytes]" ; "separator_boundary")]
    #[test_case("old-aa\nnew-bb\nx", "new-bb\nx\n\n[truncated 7 bytes]" ; "exact_budget")]
    fn tail_byte_limit_keeps_the_latest_lines(input: &str, expected: &str) {
        const BYTES: usize = 8;
        let capture = OutputCapture::new(
            Limits {
                lines: Some(8),
                bytes: Some(BYTES),
            },
            Some(3),
        );
        for line in input.split('\n') {
            capture.append(line, line.len());
        }
        assert_eq!(capture.result(), expected);
    }

    #[test_case("a\nb\nc", 1, "c" ; "below_limit")]
    #[test_case("a\nb\nc", 2, "b\nc" ; "at_limit")]
    #[test_case("a\nb\nc", 3, "b\nc\n\n[truncated 1 lines from requested tail]" ; "above_limit")]
    #[test_case("a\nb\nc", 5, "b\nc\n\n[truncated 1 lines from requested tail]" ; "short_input")]
    #[test_case("a\nb\nc\nd\ne", 3, "d\ne\n\n[truncated 1 lines from requested tail]" ; "exclude_earlier_lines")]
    #[test_case("a\nb\nc\nd\ne", 5, "d\ne\n\n[truncated 3 lines from requested tail]" ; "several_dropped_lines")]
    fn tail_reports_only_lines_dropped_within_the_requested_window(
        input: &str,
        tail: usize,
        expected: &str,
    ) {
        let capture = OutputCapture::new(
            Limits {
                lines: Some(2),
                bytes: Some(1024),
            },
            Some(tail),
        );
        for line in input.split('\n') {
            capture.append(line, line.len());
        }
        assert_eq!(capture.result(), expected);
    }

    #[test_case(None ; "prefix")]
    #[test_case(Some(usize::MAX) ; "tail")]
    fn empty_lines_cannot_bypass_byte_storage_limit(tail: Option<usize>) {
        const BYTES: usize = 16;
        let capture = OutputCapture::new(
            Limits {
                lines: None,
                bytes: Some(BYTES),
            },
            tail,
        );
        for _ in 0..10000 {
            capture.append("", 0);
        }
        let state = capture.0.lock().unwrap();
        assert!(state.lines.len() <= BYTES + 1);
        assert!(state.result().len() <= BYTES || state.result().contains(TRUNCATED));
    }

    #[test_case("a\nb\n", "a\nb" ; "line_feed")]
    #[test_case("a\r\nb\r\n", "a\nb" ; "carriage_return_line_feed")]
    #[test_case("a\rb\r", "a\rb\r" ; "literal_carriage_return")]
    #[test_case("a\n\n", "a\n" ; "empty_line")]
    #[test_case("éé\n世界", "éé\n世界" ; "split_utf8")]
    fn pipe_reading_preserves_line_semantics(input: &str, expected: &str) {
        let capture = OutputCapture::new(Limits::default(), None);
        capture.read_lines(BufReader::with_capacity(1, Cursor::new(input)), |_| true);
        assert_eq!(capture.result(), expected);
    }

    #[test]
    fn pipe_previews_are_bounded_before_lua() {
        let capture = OutputCapture::new(
            Limits {
                lines: Some(8),
                bytes: Some(1024),
            },
            None,
        );
        let input = "x".repeat(8192);
        let mut preview = String::new();
        capture.read_lines(Cursor::new(input), |line| {
            preview = line;
            true
        });
        assert_eq!(
            preview.len(),
            DEFAULT_MAX_LINE_BYTES + PREVIEW_TRUNCATION.len()
        );
        assert!(preview.ends_with(PREVIEW_TRUNCATION));
        assert!(capture.result().contains(TRUNCATED));
    }

    #[test]
    fn closing_a_full_preview_queue_stops_the_reader() {
        let capture = OutputCapture::new(Limits::default(), None);
        let reader_capture = capture.clone();
        let (tx, rx) = flume::bounded(1);
        tx.send(String::new()).unwrap();
        let (entered_tx, entered_rx) = flume::bounded(1);
        let reader = thread::spawn(move || {
            reader_capture.read_lines(Cursor::new("first\nsecond"), |line| {
                entered_tx.send(()).unwrap();
                tx.send(line).is_ok()
            });
        });
        entered_rx.recv().unwrap();
        drop(rx);
        reader.join().unwrap();
        assert_eq!(capture.result(), FIRST_LINE);
    }

    #[test_case("compact", Some(3), Some(10) ; "overrides")]
    #[test_case("full", None, None ; "full_mode")]
    fn native_module_preserves_context_and_option_contract(
        mode: &str,
        lines: Option<usize>,
        bytes: Option<usize>,
    ) {
        let lua = Lua::new();
        let Value::Table(module) = module(&lua, "maki.output_limits").unwrap().unwrap() else {
            panic!()
        };
        let ctx: Table = lua
            .load("return { output_mode = function(self) return self.mode end }")
            .eval()
            .unwrap();
        ctx.set("mode", mode).unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("max_output_lines", 3).unwrap();
        opts.set("max_output_bytes", 10).unwrap();
        let resolve: Function = module.get("resolve").unwrap();
        let resolved: (Option<usize>, Option<usize>) = resolve.call((opts, ctx)).unwrap();
        assert_eq!(resolved, (lines, bytes));
    }
}

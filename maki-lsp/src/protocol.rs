use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize)]
pub struct JsonRpcRequest<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: Value,
}

impl<'a> JsonRpcRequest<'a> {
    pub fn new(id: u64, method: &'a str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

#[derive(Serialize)]
pub struct JsonRpcNotification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    pub params: Value,
}

impl<'a> JsonRpcNotification<'a> {
    pub fn new(method: &'a str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse {
    pub id: Option<u64>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
    pub method: Option<String>,
    pub params: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentPositionParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverResult {
    pub contents: HoverContents,
    pub range: Option<Range>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HoverContents {
    Markup(MarkupContent),
    Plain(String),
    Array(Vec<MarkedString>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkupContent {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MarkedString {
    Simple(String),
    LanguageValue { language: String, value: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Serialize for DiagnosticSeverity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let n: u8 = match self {
            Self::Error => 1,
            Self::Warning => 2,
            Self::Information => 3,
            Self::Hint => 4,
        };
        serializer.serialize_u8(n)
    }
}

impl<'de> Deserialize<'de> for DiagnosticSeverity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let n = u8::deserialize(deserializer)?;
        match n {
            1 => Ok(Self::Error),
            2 => Ok(Self::Warning),
            3 => Ok(Self::Information),
            4 => Ok(Self::Hint),
            other => Err(serde::de::Error::custom(format!(
                "unknown severity: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Option<DiagnosticSeverity>,
    pub message: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    pub text_document: TextDocumentItem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem {
    pub uri: String,
    pub language_id: String,
    pub version: i32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeTextDocumentParams {
    pub text_document: VersionedTextDocumentIdentifier,
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionedTextDocumentIdentifier {
    pub uri: String,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextDocumentContentChangeEvent {
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct SymbolKind(pub u8);

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self.0 {
            1 => "file",
            2 => "module",
            3 => "namespace",
            4 => "package",
            5 => "class",
            6 => "method",
            7 => "property",
            8 => "field",
            9 => "constructor",
            10 => "enum",
            11 => "interface",
            12 => "function",
            13 => "variable",
            14 => "constant",
            15 => "string",
            16 => "number",
            17 => "boolean",
            18 => "array",
            19 => "object",
            20 => "key",
            21 => "null",
            22 => "enum_member",
            23 => "struct",
            24 => "event",
            25 => "operator",
            26 => "type_parameter",
            _ => "unknown",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range,
    pub selection_range: Range,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub children: Vec<DocumentSymbol>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInformation {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    #[serde(default)]
    pub container_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: SymbolKind,
    pub uri: String,
    pub range: Range,
    pub selection_range: Range,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyIncomingCall {
    pub from: CallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyOutgoingCall {
    pub to: CallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn position_round_trip() {
        let pos = Position {
            line: 10,
            character: 5,
        };
        let json = serde_json::to_value(&pos).unwrap();
        let back: Position = serde_json::from_value(json).unwrap();
        assert_eq!(pos, back);
    }

    #[test]
    fn range_round_trip() {
        let range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 1,
                character: 10,
            },
        };
        let json = serde_json::to_value(&range).unwrap();
        let back: Range = serde_json::from_value(json).unwrap();
        assert_eq!(range, back);
    }

    #[test]
    fn location_round_trip() {
        let loc = Location {
            uri: "file:///src/main.rs".into(),
            range: Range {
                start: Position {
                    line: 5,
                    character: 0,
                },
                end: Position {
                    line: 5,
                    character: 10,
                },
            },
        };
        let json = serde_json::to_value(&loc).unwrap();
        let back: Location = serde_json::from_value(json).unwrap();
        assert_eq!(loc, back);
    }

    #[test]
    fn diagnostic_deserializes() {
        let raw = json!({
            "range": {
                "start": {"line": 3, "character": 0},
                "end": {"line": 3, "character": 5}
            },
            "severity": 1,
            "message": "expected `;`",
            "source": "rustc"
        });
        let d: Diagnostic = serde_json::from_value(raw).unwrap();
        assert_eq!(d.severity, Some(DiagnosticSeverity::Error));
        assert_eq!(d.message, "expected `;`");
        assert_eq!(d.source.as_deref(), Some("rustc"));
    }

    #[test]
    fn hover_result_markup() {
        let raw = json!({
            "contents": {"kind": "markdown", "value": "```rust\nfn foo()\n```"},
            "range": null
        });
        let h: HoverResult = serde_json::from_value(raw).unwrap();
        match h.contents {
            HoverContents::Markup(m) => {
                assert_eq!(m.kind, "markdown");
                assert!(m.value.contains("fn foo()"));
            }
            _ => panic!("expected markup"),
        }
    }

    #[test]
    fn publish_diagnostics_params() {
        let raw = json!({
            "uri": "file:///src/lib.rs",
            "diagnostics": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "severity": 2,
                "message": "unused import"
            }]
        });
        let p: PublishDiagnosticsParams = serde_json::from_value(raw).unwrap();
        assert_eq!(p.uri, "file:///src/lib.rs");
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].severity, Some(DiagnosticSeverity::Warning));
    }

    #[test]
    fn document_symbol_deserializes() {
        let raw = json!({
            "name": "MyStruct",
            "kind": 23,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 1}},
            "selectionRange": {"start": {"line": 0, "character": 11}, "end": {"line": 0, "character": 19}},
            "detail": "pub struct",
            "children": [{
                "name": "field",
                "kind": 8,
                "range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 15}},
                "selectionRange": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 9}},
                "children": []
            }]
        });
        let s: DocumentSymbol = serde_json::from_value(raw).unwrap();
        assert_eq!(s.name, "MyStruct");
        assert_eq!(s.kind.label(), "struct");
        assert_eq!(s.detail.as_deref(), Some("pub struct"));
        assert_eq!(s.children.len(), 1);
        assert_eq!(s.children[0].name, "field");
        assert_eq!(s.children[0].kind.label(), "field");
    }

    #[test]
    fn symbol_information_deserializes() {
        let raw = json!({
            "name": "main",
            "kind": 12,
            "location": {
                "uri": "file:///src/main.rs",
                "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7}}
            },
            "containerName": "crate"
        });
        let s: SymbolInformation = serde_json::from_value(raw).unwrap();
        assert_eq!(s.name, "main");
        assert_eq!(s.kind.label(), "function");
        assert_eq!(s.container_name.as_deref(), Some("crate"));
    }

    #[test]
    fn call_hierarchy_item_round_trip() {
        let item = CallHierarchyItem {
            name: "foo".into(),
            kind: SymbolKind(12),
            uri: "file:///src/lib.rs".into(),
            range: Range {
                start: Position { line: 5, character: 0 },
                end: Position { line: 10, character: 1 },
            },
            selection_range: Range {
                start: Position { line: 5, character: 3 },
                end: Position { line: 5, character: 6 },
            },
            detail: Some("fn()".into()),
        };
        let json = serde_json::to_value(&item).unwrap();
        let back: CallHierarchyItem = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, "foo");
        assert_eq!(back.kind.label(), "function");
    }

    #[test]
    fn symbol_kind_labels() {
        assert_eq!(SymbolKind(5).label(), "class");
        assert_eq!(SymbolKind(12).label(), "function");
        assert_eq!(SymbolKind(23).label(), "struct");
        assert_eq!(SymbolKind(99).label(), "unknown");
    }
}

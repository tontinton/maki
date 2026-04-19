use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_lock::Mutex;
use serde_json::{Value, json};
use tracing::info;

use crate::config::LspServerConfig;
use crate::error::LspError;
use crate::protocol::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Diagnostic,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbol, HoverResult, Location,
    Position, SymbolInformation, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, VersionedTextDocumentIdentifier,
};
use crate::transport::{DiagnosticsCache, LspTransport};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct LspServer {
    name: String,
    transport: LspTransport,
    documents: Mutex<HashMap<String, i32>>,
    diagnostics: DiagnosticsCache,
}

impl LspServer {
    pub async fn start(
        name: &str,
        config: &LspServerConfig,
        root_uri: &str,
    ) -> Result<Arc<Self>, LspError> {
        let diagnostics: DiagnosticsCache = Arc::new(Mutex::new(HashMap::new()));

        let transport = LspTransport::spawn(
            name,
            &config.command,
            DEFAULT_TIMEOUT,
            Arc::clone(&diagnostics),
        )?;

        info!(server = name, "LSP server spawned, sending initialize");

        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "dynamicRegistration": false },
                    "references": { "dynamicRegistration": false },
                    "implementation": { "dynamicRegistration": false },
                    "documentSymbol": {
                        "dynamicRegistration": false,
                        "hierarchicalDocumentSymbolSupport": true
                    },
                    "callHierarchy": { "dynamicRegistration": false },
                    "publishDiagnostics": { "relatedInformation": true }
                },
                "workspace": {
                    "symbol": { "dynamicRegistration": false }
                }
            }
        });

        transport.send_request("initialize", init_params).await?;
        transport
            .send_notification("initialized", json!({}))
            .await?;

        info!(server = name, "LSP server initialized");

        Ok(Arc::new(Self {
            name: name.to_owned(),
            transport,
            documents: Mutex::new(HashMap::new()),
            diagnostics,
        }))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }

    pub async fn goto_definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>, LspError> {
        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.into() },
            position: Position { line, character },
        };
        let result = self
            .transport
            .send_request(
                "textDocument/definition",
                serde_json::to_value(&params).unwrap(),
            )
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }

        // Response can be Location, Location[], or LocationLink[]
        if let Ok(loc) = serde_json::from_value::<Location>(result.clone()) {
            return Ok(vec![loc]);
        }
        if let Ok(locs) = serde_json::from_value::<Vec<Location>>(result.clone()) {
            return Ok(locs);
        }

        Ok(vec![])
    }

    pub async fn find_references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>, LspError> {
        let params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character},
            "context": {"includeDeclaration": true}
        });
        let result = self
            .transport
            .send_request("textDocument/references", params)
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }

        serde_json::from_value::<Vec<Location>>(result).map_err(|e| LspError::InvalidResponse {
            server: self.name.clone(),
            reason: e.to_string(),
        })
    }

    pub async fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<HoverResult>, LspError> {
        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.into() },
            position: Position { line, character },
        };
        let result = self
            .transport
            .send_request("textDocument/hover", serde_json::to_value(&params).unwrap())
            .await?;

        if result.is_null() {
            return Ok(None);
        }

        serde_json::from_value::<HoverResult>(result)
            .map(Some)
            .map_err(|e| LspError::InvalidResponse {
                server: self.name.clone(),
                reason: e.to_string(),
            })
    }

    pub async fn diagnostics(&self, uri: &str) -> Vec<Diagnostic> {
        self.diagnostics
            .lock()
            .await
            .get(uri)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn did_open(&self, uri: &str, language_id: &str, text: &str) -> Result<(), LspError> {
        let mut docs = self.documents.lock().await;
        if docs.contains_key(uri) {
            return Ok(());
        }
        let version = 1;
        docs.insert(uri.to_owned(), version);

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.into(),
                language_id: language_id.into(),
                version,
                text: text.into(),
            },
        };
        self.transport
            .send_notification(
                "textDocument/didOpen",
                serde_json::to_value(&params).unwrap(),
            )
            .await
    }

    pub async fn did_change(&self, uri: &str, text: &str) -> Result<(), LspError> {
        let mut docs = self.documents.lock().await;
        let version = docs.entry(uri.to_owned()).or_insert(0);
        *version += 1;
        let v = *version;

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri.into(),
                version: v,
            },
            content_changes: vec![TextDocumentContentChangeEvent { text: text.into() }],
        };
        self.transport
            .send_notification(
                "textDocument/didChange",
                serde_json::to_value(&params).unwrap(),
            )
            .await
    }

    pub async fn goto_implementation(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>, LspError> {
        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.into() },
            position: Position { line, character },
        };
        let result = self
            .transport
            .send_request(
                "textDocument/implementation",
                serde_json::to_value(&params).unwrap(),
            )
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }

        if let Ok(loc) = serde_json::from_value::<Location>(result.clone()) {
            return Ok(vec![loc]);
        }
        if let Ok(locs) = serde_json::from_value::<Vec<Location>>(result.clone()) {
            return Ok(locs);
        }

        Ok(vec![])
    }

    pub async fn document_symbol(
        &self,
        uri: &str,
    ) -> Result<Vec<DocumentSymbol>, LspError> {
        let params = json!({ "textDocument": { "uri": uri } });
        let result = self
            .transport
            .send_request("textDocument/documentSymbol", params)
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }

        if let Ok(syms) = serde_json::from_value::<Vec<DocumentSymbol>>(result.clone()) {
            return Ok(syms);
        }

        if let Ok(infos) = serde_json::from_value::<Vec<SymbolInformation>>(result) {
            return Ok(infos
                .into_iter()
                .map(|si| DocumentSymbol {
                    name: si.name,
                    kind: si.kind,
                    range: si.location.range.clone(),
                    selection_range: si.location.range,
                    detail: si.container_name,
                    children: vec![],
                })
                .collect());
        }

        Ok(vec![])
    }

    pub async fn workspace_symbol(
        &self,
        query: &str,
    ) -> Result<Vec<SymbolInformation>, LspError> {
        let params = json!({ "query": query });
        let result = self
            .transport
            .send_request("workspace/symbol", params)
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }

        serde_json::from_value::<Vec<SymbolInformation>>(result).map_err(|e| {
            LspError::InvalidResponse {
                server: self.name.clone(),
                reason: e.to_string(),
            }
        })
    }

    pub async fn prepare_call_hierarchy(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<CallHierarchyItem>, LspError> {
        let params = TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.into() },
            position: Position { line, character },
        };
        let result = self
            .transport
            .send_request(
                "textDocument/prepareCallHierarchy",
                serde_json::to_value(&params).unwrap(),
            )
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }

        serde_json::from_value::<Vec<CallHierarchyItem>>(result).map_err(|e| {
            LspError::InvalidResponse {
                server: self.name.clone(),
                reason: e.to_string(),
            }
        })
    }

    pub async fn incoming_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> Result<Vec<CallHierarchyIncomingCall>, LspError> {
        let params = json!({ "item": item });
        let result = self
            .transport
            .send_request("callHierarchy/incomingCalls", params)
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }

        serde_json::from_value::<Vec<CallHierarchyIncomingCall>>(result).map_err(|e| {
            LspError::InvalidResponse {
                server: self.name.clone(),
                reason: e.to_string(),
            }
        })
    }

    pub async fn outgoing_calls(
        &self,
        item: &CallHierarchyItem,
    ) -> Result<Vec<CallHierarchyOutgoingCall>, LspError> {
        let params = json!({ "item": item });
        let result = self
            .transport
            .send_request("callHierarchy/outgoingCalls", params)
            .await?;

        if result.is_null() {
            return Ok(vec![]);
        }

        serde_json::from_value::<Vec<CallHierarchyOutgoingCall>>(result).map_err(|e| {
            LspError::InvalidResponse {
                server: self.name.clone(),
                reason: e.to_string(),
            }
        })
    }

    pub async fn shutdown(&self) {
        let _ = self.transport.send_request("shutdown", Value::Null).await;
        let _ = self.transport.send_notification("exit", json!({})).await;
        self.transport.shutdown().await;
    }
}

pub fn file_uri(path: &str) -> String {
    format!("file://{path}")
}

pub fn path_from_uri(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

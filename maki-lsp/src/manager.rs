use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_lock::Mutex;
use tracing::{info, warn};

use crate::config::{LspConfig, language_from_extension};
use crate::error::LspError;
use crate::protocol::{
    CallHierarchyIncomingCall, CallHierarchyOutgoingCall, Diagnostic, DiagnosticSeverity,
    DocumentSymbol, HoverContents, HoverResult, Location, MarkedString, SymbolInformation,
};
use crate::server::{LspServer, file_uri, path_from_uri};

pub struct LspManager {
    config: LspConfig,
    servers: Mutex<HashMap<String, Arc<LspServer>>>,
    root_uri: String,
}

impl LspManager {
    pub fn new(config: LspConfig, root_path: &str) -> Self {
        Self {
            config,
            servers: Mutex::new(HashMap::new()),
            root_uri: file_uri(root_path),
        }
    }

    fn language_for_path(&self, path: &str) -> Result<String, LspError> {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let language = language_from_extension(ext);
        Ok(language.to_owned())
    }

    async fn ensure_server(&self, language: &str) -> Result<Arc<LspServer>, LspError> {
        let mut servers = self.servers.lock().await;

        let (server_name, server_config) =
            self.config.server_for_language(language).ok_or_else(|| {
                LspError::ServerNotConfigured {
                    language: language.into(),
                }
            })?;

        if let Some(server) = servers.get(server_name) {
            if server.is_alive() {
                return Ok(Arc::clone(server));
            }
            warn!(server = server_name, "LSP server died, restarting");
            servers.remove(server_name);
        }

        info!(server = server_name, language, "starting LSP server");
        let server = LspServer::start(server_name, server_config, &self.root_uri).await?;
        servers.insert(server_name.to_owned(), Arc::clone(&server));
        Ok(server)
    }

    async fn open_if_needed(
        &self,
        server: &LspServer,
        path: &str,
        language: &str,
    ) -> Result<(), LspError> {
        let uri = file_uri(path);
        let text = std::fs::read_to_string(path).map_err(|e| LspError::RequestFailed {
            server: server.name().into(),
            message: format!("cannot read {path}: {e}"),
        })?;
        server.did_open(&uri, language, &text).await
    }

    pub async fn goto_definition(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;
        self.open_if_needed(&server, path, &language).await?;

        let uri = file_uri(path);
        let locations = server.goto_definition(&uri, line, character).await?;

        if locations.is_empty() {
            return Ok("No definition found".into());
        }

        Ok(format_locations(&locations))
    }

    pub async fn find_references(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;
        self.open_if_needed(&server, path, &language).await?;

        let uri = file_uri(path);
        let locations = server.find_references(&uri, line, character).await?;

        if locations.is_empty() {
            return Ok("No references found".into());
        }

        Ok(format_locations(&locations))
    }

    pub async fn hover(&self, path: &str, line: u32, character: u32) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;
        self.open_if_needed(&server, path, &language).await?;

        let uri = file_uri(path);
        let result = server.hover(&uri, line, character).await?;

        match result {
            None => Ok("No hover information available".into()),
            Some(hover) => Ok(format_hover(&hover)),
        }
    }

    pub async fn diagnostics(&self, path: &str) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;
        self.open_if_needed(&server, path, &language).await?;

        let uri = file_uri(path);
        let diags = server.diagnostics(&uri).await;

        if diags.is_empty() {
            return Ok("No diagnostics".into());
        }

        Ok(format_diagnostics(&diags))
    }

    pub async fn goto_implementation(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;
        self.open_if_needed(&server, path, &language).await?;

        let uri = file_uri(path);
        let locations = server.goto_implementation(&uri, line, character).await?;

        if locations.is_empty() {
            return Ok("No implementations found".into());
        }

        Ok(format_locations(&locations))
    }

    pub async fn document_symbol(&self, path: &str) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;
        self.open_if_needed(&server, path, &language).await?;

        let uri = file_uri(path);
        let symbols = server.document_symbol(&uri).await?;

        if symbols.is_empty() {
            return Ok("No symbols found".into());
        }

        Ok(format_document_symbols(&symbols))
    }

    pub async fn workspace_symbol(
        &self,
        path: &str,
        query: &str,
    ) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;

        let symbols = server.workspace_symbol(query).await?;

        if symbols.is_empty() {
            return Ok("No symbols found".into());
        }

        Ok(format_symbol_informations(&symbols))
    }

    pub async fn incoming_calls(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;
        self.open_if_needed(&server, path, &language).await?;

        let uri = file_uri(path);
        let items = server.prepare_call_hierarchy(&uri, line, character).await?;

        let Some(item) = items.into_iter().next() else {
            return Ok("No call hierarchy available at this position".into());
        };

        let calls = server.incoming_calls(&item).await?;

        if calls.is_empty() {
            return Ok("No incoming calls found".into());
        }

        Ok(format_incoming_calls(&calls))
    }

    pub async fn outgoing_calls(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        let language = self.language_for_path(path)?;
        let server = self.ensure_server(&language).await?;
        self.open_if_needed(&server, path, &language).await?;

        let uri = file_uri(path);
        let items = server.prepare_call_hierarchy(&uri, line, character).await?;

        let Some(item) = items.into_iter().next() else {
            return Ok("No call hierarchy available at this position".into());
        };

        let calls = server.outgoing_calls(&item).await?;

        if calls.is_empty() {
            return Ok("No outgoing calls found".into());
        }

        Ok(format_outgoing_calls(&calls))
    }

    pub async fn shutdown(&self) {
        let servers: Vec<Arc<LspServer>> = self.servers.lock().await.values().cloned().collect();
        for server in servers {
            server.shutdown().await;
        }
    }
}

fn format_locations(locations: &[Location]) -> String {
    locations
        .iter()
        .map(|loc| {
            let path = path_from_uri(&loc.uri);
            let line = loc.range.start.line + 1;
            let col = loc.range.start.character + 1;
            format!("{path}:{line}:{col}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_hover(hover: &HoverResult) -> String {
    match &hover.contents {
        HoverContents::Markup(m) => m.value.clone(),
        HoverContents::Plain(s) => s.clone(),
        HoverContents::Array(arr) => arr
            .iter()
            .map(|ms| match ms {
                MarkedString::Simple(s) => s.clone(),
                MarkedString::LanguageValue { language, value } => {
                    format!("```{language}\n{value}\n```")
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

fn format_document_symbol(sym: &DocumentSymbol, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let line = sym.selection_range.start.line + 1;
    let kind = sym.kind.label();
    let detail = sym.detail.as_deref().unwrap_or("");
    if detail.is_empty() {
        out.push_str(&format!("{indent}[{line}] {kind} {}", sym.name));
    } else {
        out.push_str(&format!("{indent}[{line}] {kind} {} - {detail}", sym.name));
    }
    out.push('\n');
    for child in &sym.children {
        format_document_symbol(child, depth + 1, out);
    }
}

fn format_document_symbols(symbols: &[DocumentSymbol]) -> String {
    let mut out = String::new();
    for sym in symbols {
        format_document_symbol(sym, 0, &mut out);
    }
    out.trim_end().to_owned()
}

fn format_symbol_informations(symbols: &[SymbolInformation]) -> String {
    symbols
        .iter()
        .map(|si| {
            let path = path_from_uri(&si.location.uri);
            let line = si.location.range.start.line + 1;
            let kind = si.kind.label();
            let container = si
                .container_name
                .as_deref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            format!("{path}:{line} {kind} {}{container}", si.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_incoming_calls(calls: &[CallHierarchyIncomingCall]) -> String {
    calls
        .iter()
        .map(|c| {
            let path = path_from_uri(&c.from.uri);
            let line = c.from.selection_range.start.line + 1;
            let kind = c.from.kind.label();
            format!("{path}:{line} {kind} {}", c.from.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_outgoing_calls(calls: &[CallHierarchyOutgoingCall]) -> String {
    calls
        .iter()
        .map(|c| {
            let path = path_from_uri(&c.to.uri);
            let line = c.to.selection_range.start.line + 1;
            let kind = c.to.kind.label();
            format!("{path}:{line} {kind} {}", c.to.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_diagnostics(diags: &[Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| {
            let severity = match d.severity {
                Some(DiagnosticSeverity::Error) => "error",
                Some(DiagnosticSeverity::Warning) => "warning",
                Some(DiagnosticSeverity::Information) => "info",
                Some(DiagnosticSeverity::Hint) => "hint",
                None => "unknown",
            };
            let line = d.range.start.line + 1;
            let col = d.range.start.character + 1;
            let source = d.source.as_deref().unwrap_or("");
            let source_prefix = if source.is_empty() {
                String::new()
            } else {
                format!("[{source}] ")
            };
            format!("L{line}:{col} {severity}: {source_prefix}{}", d.message)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone)]
pub struct LspHandle {
    inner: Arc<LspManager>,
}

impl LspHandle {
    pub fn new(config: LspConfig, root_path: &str) -> Self {
        Self {
            inner: Arc::new(LspManager::new(config, root_path)),
        }
    }

    pub async fn goto_definition(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        self.inner.goto_definition(path, line, character).await
    }

    pub async fn find_references(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        self.inner.find_references(path, line, character).await
    }

    pub async fn hover(&self, path: &str, line: u32, character: u32) -> Result<String, LspError> {
        self.inner.hover(path, line, character).await
    }

    pub async fn diagnostics(&self, path: &str) -> Result<String, LspError> {
        self.inner.diagnostics(path).await
    }

    pub async fn goto_implementation(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        self.inner.goto_implementation(path, line, character).await
    }

    pub async fn document_symbol(&self, path: &str) -> Result<String, LspError> {
        self.inner.document_symbol(path).await
    }

    pub async fn workspace_symbol(
        &self,
        path: &str,
        query: &str,
    ) -> Result<String, LspError> {
        self.inner.workspace_symbol(path, query).await
    }

    pub async fn incoming_calls(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        self.inner.incoming_calls(path, line, character).await
    }

    pub async fn outgoing_calls(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<String, LspError> {
        self.inner.outgoing_calls(path, line, character).await
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

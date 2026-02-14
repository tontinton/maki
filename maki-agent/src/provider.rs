use std::sync::mpsc::Sender;
use std::thread;

use serde_json::Value;

use crate::model::Model;
use crate::{AgentError, AgentEvent, Message, StreamResponse};

pub trait Provider: Send + Sync {
    fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<AgentEvent>,
    ) -> Result<StreamResponse, AgentError>;

    fn list_models(&self) -> Result<Vec<String>, AgentError>;
}

const PROVIDERS: &[&str] = &["anthropic", "zai"];

pub fn from_model(model: &Model) -> Result<Box<dyn Provider>, AgentError> {
    from_name(model.provider.as_str())
}

fn from_name(name: &str) -> Result<Box<dyn Provider>, AgentError> {
    match name {
        "anthropic" => Ok(Box::new(crate::anthropic::Anthropic::new()?)),
        "zai" => Ok(Box::new(crate::zai::Zai::new()?)),
        other => Err(AgentError::Api {
            status: 0,
            message: format!("unsupported provider: {other}"),
        }),
    }
}

pub fn fetch_all_models(mut on_ready: impl FnMut(Vec<String>)) {
    let (tx, rx) = std::sync::mpsc::channel();

    for &name in PROVIDERS {
        let Ok(provider) = from_name(name) else {
            continue;
        };
        let tx = tx.clone();
        thread::spawn(move || {
            let models = match provider.list_models() {
                Ok(ids) => ids.into_iter().map(|id| format!("{name}/{id}")).collect(),
                Err(e) => {
                    eprintln!("warning: {name}: {e}");
                    Vec::new()
                }
            };
            let _ = tx.send(models);
        });
    }
    drop(tx);

    while let Ok(models) = rx.recv() {
        on_ready(models);
    }
}

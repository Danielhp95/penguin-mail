//! The model's tool calls arrive on a tokio thread; the window lives on the
//! GTK thread. [`Host`] is the mail tools' source: it carries each call
//! across and waits for the answer.

use std::path::PathBuf;
use std::sync::Arc;

use mailrs_ai::{BoxFuture, ToolOutcome, ToolSpec};
use serde_json::Value;

use super::sources::Source;

/// One tool call for the GTK thread to run.
pub struct ToolRequest {
    pub name: String,
    pub input: Value,
    pub picked_files: Arc<[PathBuf]>,
    pub reply: async_channel::Sender<ToolOutcome>,
}

pub struct Host {
    specs: Vec<ToolSpec>,
    requests: async_channel::Sender<ToolRequest>,
    picked_files: Arc<[PathBuf]>,
}

impl Host {
    pub fn new(specs: Vec<ToolSpec>, requests: async_channel::Sender<ToolRequest>) -> Host {
        Host {
            specs,
            requests,
            picked_files: Arc::default(),
        }
    }
    /// Scopes the picker grants to calls from this request.
    pub fn with_picked_files(mut self, files: Arc<[PathBuf]>) -> Self {
        self.picked_files = files;
        self
    }
}

impl Source for Host {
    fn id(&self) -> String {
        "mail".into()
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    /// Never asks here. A mail tool's question names what the window and
    /// Gmail hold when the call runs, such as the message it would send or
    /// the automatic reply it would change, so the tool asks from inside
    /// the call, on the GTK thread. See `run::catalog`.
    fn ask(&self, _name: &str, _input: &Value) -> Option<String> {
        None
    }

    fn call(&self, name: String, input: Value) -> BoxFuture<ToolOutcome> {
        let requests = self.requests.clone();
        let picked_files = Arc::clone(&self.picked_files);
        Box::pin(async move {
            let (reply, answer) = async_channel::bounded(1);
            if requests
                .send(ToolRequest {
                    name,
                    input,
                    picked_files,
                    reply,
                })
                .await
                .is_err()
            {
                return ToolOutcome::Err("The mail window closed.".into());
            }
            answer
                .recv()
                .await
                .unwrap_or_else(|_| ToolOutcome::Err("The mail window closed.".into()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn picker_grants_travel_separately_from_model_arguments() {
        let (requests, received) = async_channel::unbounded();
        let picked: Arc<[PathBuf]> =
            vec![PathBuf::from("/run/user/1000/doc/grant/lease.pdf")].into();
        let host = Host::new(Vec::new(), requests).with_picked_files(Arc::clone(&picked));
        let call = host.call(
            "send_email".into(),
            serde_json::json!({"path": "/etc/shadow"}),
        );
        let receive = async {
            let request = received.recv().await.unwrap();
            assert_eq!(request.picked_files, picked);
            assert_eq!(request.input["path"], "/etc/shadow");
            request
                .reply
                .send(ToolOutcome::Ok(serde_json::json!(true)))
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(call, receive);
        assert!(matches!(result, ToolOutcome::Ok(_)));
    }
}

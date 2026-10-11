//! One JMAP request: method calls in order, each with an id a later call
//! can reference (RFC 8620 section 3.7), and `using` built from the
//! capabilities the calls name. The answer hands back each call's
//! arguments by id and name, or the method error in their place.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::JmapError;
use crate::error::method_error;
use crate::session::CORE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallId(pub(crate) String);

impl CallId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A back-reference to `path` in this call's `name` answer, for an
    /// argument whose key carries a leading `#`.
    pub fn result_of(&self, name: &str, path: &str) -> Value {
        json!({ "resultOf": self.0, "name": name, "path": path })
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    using: BTreeSet<&'static str>,
    calls: Vec<(String, Value, String)>,
}

impl Default for Request {
    fn default() -> Self {
        Request::new()
    }
}

impl Request {
    pub fn new() -> Request {
        Request { using: BTreeSet::from([CORE]), calls: Vec::new() }
    }

    /// Adds a call that needs `capability`, and answers its id.
    pub fn call(&mut self, capability: &'static str, name: &str, args: Value) -> CallId {
        let id = format!("c{}", self.calls.len());
        self.using.insert(capability);
        self.calls.push((name.to_string(), args, id.clone()));
        CallId(id)
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Core first, then the rest in a stable order.
    pub fn using(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(CORE).chain(self.using.iter().copied().filter(|c| *c != CORE))
    }

    pub fn body(&self) -> Value {
        let calls: Vec<Value> = self.calls.iter().map(|(name, args, id)| json!([name, args, id])).collect();
        json!({ "using": self.using().collect::<Vec<_>>(), "methodCalls": calls })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    method_responses: Vec<(String, Value, String)>,
    #[serde(default)]
    session_state: String,
}

#[derive(Debug)]
pub struct Response {
    /// The session's state as the server answered; a change means the
    /// session should be fetched again.
    pub session_state: String,
    answers: Vec<(String, Value, String)>,
}

impl Response {
    pub fn parse(bytes: &[u8]) -> Result<Response, JmapError> {
        let wire: Wire = serde_json::from_slice(bytes).map_err(|e| JmapError::Decode(format!("the answer: {e}")))?;
        Ok(Response { session_state: wire.session_state, answers: wire.method_responses })
    }

    /// The `name` answer to call `id`, or the error the server put in its
    /// place. Each answer is taken once.
    pub fn take<T: DeserializeOwned>(&mut self, id: &CallId, name: &str) -> Result<T, JmapError> {
        let at = self
            .answers
            .iter()
            .position(|(n, _, i)| *i == id.0 && (n == name || n == "error"))
            .ok_or_else(|| JmapError::Decode(format!("no answer to {name}")))?;
        let (answered, args, _) = self.answers.remove(at);
        if answered == "error" {
            return Err(method_error(
                args.get("type").and_then(Value::as_str).unwrap_or("serverFail"),
                args.get("description").and_then(Value::as_str),
            ));
        }
        serde_json::from_value(args).map_err(|e| JmapError::Decode(format!("{name}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::session::{MAIL, SUBMISSION};

    #[test]
    fn using_names_core_and_only_what_the_calls_need() {
        let mut request = Request::new();
        request.call(MAIL, "Mailbox/get", json!({"accountId": "A1", "ids": null}));
        let using: Vec<&str> = request.using().collect();
        assert_eq!(using, [CORE, MAIL]);
        request.call(SUBMISSION, "Identity/get", json!({"accountId": "A1"}));
        assert_eq!(request.using().count(), 3);
    }

    #[test]
    fn calls_get_ids_in_order_and_a_back_reference_names_one() {
        let mut request = Request::new();
        let query = request.call(MAIL, "Email/query", json!({"accountId": "A1"}));
        request.call(MAIL, "Email/get", json!({"accountId": "A1", "#ids": query.result_of("Email/query", "/ids")}));
        let body = request.body();
        assert_eq!(body["methodCalls"][0][2], "c0");
        assert_eq!(body["methodCalls"][1][0], "Email/get");
        assert_eq!(body["methodCalls"][1][1]["#ids"], json!({"resultOf": "c0", "name": "Email/query", "path": "/ids"}));
        assert_eq!(body["using"], json!([CORE, MAIL]));
    }

    #[test]
    fn an_answer_is_taken_by_its_call_and_name() {
        let text = br#"{"methodResponses":[["Email/query",{"ids":["M1"]},"c0"],["Email/get",{"list":[]},"c1"]],"sessionState":"s1"}"#;
        let mut response = Response::parse(text).unwrap();
        assert_eq!(response.session_state, "s1");
        let ids: serde_json::Value = response.take(&CallId("c0".into()), "Email/query").unwrap();
        assert_eq!(ids["ids"][0], "M1");
        assert!(response.take::<serde_json::Value>(&CallId("c0".into()), "Email/query").is_err(), "taken once");
    }

    #[test]
    fn a_method_error_comes_back_as_its_kind() {
        let text = br#"{"methodResponses":[["error",{"type":"cannotCalculateChanges"},"c0"]],"sessionState":"s1"}"#;
        let mut response = Response::parse(text).unwrap();
        let taken = response.take::<serde_json::Value>(&CallId("c0".into()), "Email/changes");
        assert_eq!(taken, Err(JmapError::CannotCalculateChanges));
    }

    #[test]
    fn two_answers_under_one_call_are_told_apart_by_name() {
        // EmailSubmission/set with onSuccessUpdateEmail answers an implicit
        // Email/set under the same call id (RFC 8621 section 7.5).
        let text = br#"{"methodResponses":[["EmailSubmission/set",{"created":{"s":{"id":"S1"}}},"c1"],["Email/set",{"updated":{"M1":null}},"c1"]],"sessionState":"s"}"#;
        let mut response = Response::parse(text).unwrap();
        let set: serde_json::Value = response.take(&CallId("c1".into()), "Email/set").unwrap();
        assert!(set["updated"].get("M1").is_some());
        let submission: serde_json::Value = response.take(&CallId("c1".into()), "EmailSubmission/set").unwrap();
        assert_eq!(submission["created"]["s"]["id"], "S1");
    }
}

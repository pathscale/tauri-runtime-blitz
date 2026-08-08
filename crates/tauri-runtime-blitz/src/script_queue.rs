use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use blitz_script::ScriptDocument;

/// Thread-safe ingress for scripts emitted by Tauri command and event responders.
///
/// `ScriptDocument` is intentionally single-threaded. Dispatchers clone this queue and push from
/// any thread; the native event loop drains it into Boa on the document thread.
type EvalCallback = Box<dyn FnOnce(String) + Send + 'static>;
type DocumentTask = Box<dyn FnOnce() + Send + 'static>;

enum ScriptTask {
    Evaluate(String),
    EvaluateWithCallback(String, EvalCallback),
    RunOnDocumentThread(DocumentTask),
}

/// Queue shared by the thread-safe webview dispatcher and the Boa document thread.
#[derive(Clone, Default)]
pub struct ScriptQueue(Arc<Mutex<VecDeque<ScriptTask>>>);

impl fmt::Debug for ScriptQueue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScriptQueue")
            .field("pending", &self.pending())
            .finish()
    }
}

impl ScriptQueue {
    pub fn enqueue(&self, script: impl Into<String>) {
        self.0
            .lock()
            .unwrap()
            .push_back(ScriptTask::Evaluate(script.into()));
    }

    pub fn enqueue_with_callback(
        &self,
        script: impl Into<String>,
        callback: impl FnOnce(String) + Send + 'static,
    ) {
        self.0
            .lock()
            .unwrap()
            .push_back(ScriptTask::EvaluateWithCallback(
                script.into(),
                Box::new(callback),
            ));
    }

    pub fn enqueue_task(&self, task: impl FnOnce() + Send + 'static) {
        self.0
            .lock()
            .unwrap()
            .push_back(ScriptTask::RunOnDocumentThread(Box::new(task)));
    }

    pub fn pending(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn drain_into(&self, document: &mut ScriptDocument) -> usize {
        let tasks: Vec<ScriptTask> = self.0.lock().unwrap().drain(..).collect();
        let count = tasks.len();
        for task in tasks {
            match task {
                ScriptTask::Evaluate(script) => document.eval(&script),
                ScriptTask::EvaluateWithCallback(script, callback) => {
                    let result = document
                        .eval_json(&script)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|_| "null".into());
                    callback(result);
                }
                ScriptTask::RunOnDocumentThread(task) => task(),
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::{Document, DocumentConfig};

    #[test]
    fn responses_run_on_the_document_thread_in_order() {
        let mut document = ScriptDocument::from_html(
            r#"<div id="result">waiting</div>"#,
            DocumentConfig::default(),
        );
        let queue = ScriptQueue::default();
        let producer = queue.clone();
        std::thread::spawn(move || {
            producer.enqueue("window.first = 'hello';");
            producer.enqueue(
                "document.getElementById('result').textContent = window.first + ' from Rust';",
            );
        })
        .join()
        .unwrap();

        assert_eq!(queue.pending(), 2);
        assert_eq!(queue.drain_into(&mut document), 2);
        assert_eq!(queue.pending(), 0);
        let inner = document.inner();
        let result = inner.query_selector("#result").unwrap().unwrap();
        assert_eq!(
            inner.get_node(result).unwrap().text_content(),
            "hello from Rust"
        );
    }

    #[test]
    fn evaluation_result_returns_to_an_async_dispatcher_callback() {
        let mut document = ScriptDocument::from_html(
            r#"<div id="result">waiting</div>"#,
            DocumentConfig::default(),
        );
        let queue = ScriptQueue::default();
        let producer = queue.clone();
        let (result_sender, result_receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            producer.enqueue_with_callback(
                "({ message: 'Hello from greet', requestId: 7 })",
                move |result| result_sender.send(result).unwrap(),
            );
        })
        .join()
        .unwrap();

        assert_eq!(queue.drain_into(&mut document), 1);
        assert_eq!(
            result_receiver.recv().unwrap(),
            r#"{"message":"Hello from greet","requestId":7}"#
        );
    }
}

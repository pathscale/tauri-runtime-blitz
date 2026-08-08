use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Waker};

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

#[derive(Default)]
struct ScriptQueueState {
    tasks: VecDeque<ScriptTask>,
    waker: Option<Waker>,
}

/// Queue shared by the thread-safe webview dispatcher and the Boa document thread.
#[derive(Clone, Default)]
pub struct ScriptQueue(Arc<Mutex<ScriptQueueState>>);

impl fmt::Debug for ScriptQueue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScriptQueue")
            .field("pending", &self.pending())
            .finish()
    }
}

impl ScriptQueue {
    fn push(&self, task: ScriptTask) {
        let waker = {
            let mut state = self.0.lock().unwrap();
            state.tasks.push_back(task);
            state.waker.clone()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub fn enqueue(&self, script: impl Into<String>) {
        self.push(ScriptTask::Evaluate(script.into()));
    }

    pub fn enqueue_with_callback(
        &self,
        script: impl Into<String>,
        callback: impl FnOnce(String) + Send + 'static,
    ) {
        self.push(ScriptTask::EvaluateWithCallback(
            script.into(),
            Box::new(callback),
        ));
    }

    pub fn enqueue_task(&self, task: impl FnOnce() + Send + 'static) {
        self.push(ScriptTask::RunOnDocumentThread(Box::new(task)));
    }

    pub fn pending(&self) -> usize {
        self.0.lock().unwrap().tasks.len()
    }

    /// Attach this queue to the document's native poll cycle.
    pub fn attach_to(&self, document: &mut ScriptDocument) {
        let queue = self.clone();
        document.set_poll_hook(move |document, task_context| queue.poll(document, task_context));
    }

    fn poll(&self, document: &mut ScriptDocument, task_context: Option<&TaskContext<'_>>) -> bool {
        if let Some(task_context) = task_context {
            let mut state = self.0.lock().unwrap();
            let stale = state
                .waker
                .as_ref()
                .map(|old| !old.will_wake(task_context.waker()))
                .unwrap_or(true);
            if stale {
                state.waker = Some(task_context.waker().clone());
            }
        }
        self.drain_into(document) > 0
    }

    pub fn drain_into(&self, document: &mut ScriptDocument) -> usize {
        let tasks: Vec<ScriptTask> = self.0.lock().unwrap().tasks.drain(..).collect();
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake, Waker};

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

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

    #[test]
    fn attached_queue_wakes_and_drains_from_document_poll() {
        let mut document = ScriptDocument::from_html(
            r#"<div id="result">waiting</div>"#,
            DocumentConfig::default(),
        );
        let queue = ScriptQueue::default();
        queue.attach_to(&mut document);

        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&counter));
        assert!(document.poll(Some(Context::from_waker(&waker))));

        queue.enqueue("document.getElementById('result').textContent = 'drained';");
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert!(document.poll(Some(Context::from_waker(&waker))));

        let inner = document.inner();
        let result = inner.query_selector("#result").unwrap().unwrap();
        assert_eq!(inner.get_node(result).unwrap().text_content(), "drained");
    }
}

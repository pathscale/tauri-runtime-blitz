use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use blitz_script::ScriptDocument;

/// Thread-safe ingress for scripts emitted by Tauri command and event responders.
///
/// `ScriptDocument` is intentionally single-threaded. Dispatchers clone this queue and push from
/// any thread; the native event loop drains it into Boa on the document thread.
#[derive(Debug, Clone, Default)]
pub struct ScriptQueue(Arc<Mutex<VecDeque<String>>>);

impl ScriptQueue {
    pub fn enqueue(&self, script: impl Into<String>) {
        self.0.lock().unwrap().push_back(script.into());
    }

    pub fn pending(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn drain_into(&self, document: &mut ScriptDocument) -> usize {
        let scripts: Vec<String> = self.0.lock().unwrap().drain(..).collect();
        let count = scripts.len();
        for script in scripts {
            document.eval(&script);
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
}

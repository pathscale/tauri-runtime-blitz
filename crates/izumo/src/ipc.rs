use blitz_script::ScriptDocument;
use http::{Method, Request};
use tauri_runtime::webview::{DetachedWebview, WebviewIpcHandler};
use tauri_runtime::{Runtime, UserEvent};

fn ipc_request(page_url: &str, body: String) -> Result<Request<String>, http::Error> {
    Request::builder()
        .method(Method::POST)
        .uri(page_url)
        .body(body)
}

/// Connect Boa's `window.ipc.postMessage` host hook to Tauri's existing IPC handler.
///
/// JavaScript invokes the handler on the document's owning thread. Tauri may finish a command
/// asynchronously; its response returns through the webview dispatcher's `eval_script` queue.
pub fn attach_ipc_handler<T, R>(
    document: &mut ScriptDocument,
    page_url: String,
    webview: DetachedWebview<T, R>,
    handler: WebviewIpcHandler<T, R>,
) where
    T: UserEvent,
    R: Runtime<T>,
{
    document.set_ipc_handler(move |body| match ipc_request(&page_url, body) {
        Ok(request) => handler(webview.clone(), request),
        Err(error) => eprintln!("tauri-runtime-blitz: could not construct IPC request: {error}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_request_preserves_body_and_page_url() {
        let request = ipc_request("tauri://localhost/settings", r#"{"cmd":"greet"}"#.into())
            .expect("valid request");
        assert_eq!(request.method(), Method::POST);
        assert_eq!(request.uri(), "tauri://localhost/settings");
        assert_eq!(request.body(), r#"{"cmd":"greet"}"#);
    }
}

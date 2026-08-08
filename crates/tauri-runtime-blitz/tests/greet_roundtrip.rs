use blitz_dom::{Document, DocumentConfig};
use blitz_script::ScriptDocument;
use tauri::ipc::CallbackFn;
use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder, mock_context, noop_assets};
use tauri::webview::InvokeRequest;
use tauri_runtime_blitz::ScriptQueue;

#[tauri::command]
fn greet(name: String) -> String {
    format!("Hello, {name}!")
}

#[test]
fn real_tauri_greet_response_reaches_boa() {
    let app = mock_builder()
        .invoke_handler(tauri::generate_handler![greet])
        .build(mock_context(noop_assets()))
        .expect("Tauri test app should build");
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("test webview should build");

    let response = get_ipc_response(
        &webview,
        InvokeRequest {
            cmd: "greet".into(),
            callback: CallbackFn(7),
            error: CallbackFn(8),
            url: "tauri://localhost".parse().unwrap(),
            body: serde_json::json!({ "name": "Boa" }).into(),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.into(),
        },
    )
    .expect("greet command should succeed")
    .deserialize::<String>()
    .expect("greet response should be a string");

    let mut document = ScriptDocument::from_html(
        r#"
        <div id="result">waiting</div>
        <script>
          window.__TAURI_INTERNALS__ = {
            runCallback(id, value) {
              document.getElementById("result").textContent = `${id}:${value}`;
            }
          };
        </script>
        "#,
        DocumentConfig::default(),
    );
    document.execute_scripts();

    let queue = ScriptQueue::default();
    queue.attach_to(&mut document);
    let response_json = serde_json::to_string(&response).unwrap();
    queue.enqueue(format!(
        "window.__TAURI_INTERNALS__.runCallback(7, {response_json})"
    ));

    assert!(document.poll(None));
    let inner = document.inner();
    let result = inner.query_selector("#result").unwrap().unwrap();
    assert_eq!(
        inner.get_node(result).unwrap().text_content(),
        "7:Hello, Boa!"
    );
}

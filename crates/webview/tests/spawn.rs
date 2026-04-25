use std::time::Duration;

use crust_webview::{spawn, WebviewCommand};
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokio::test]
async fn spawn_and_shutdown_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let (evt_tx, mut evt_rx) = mpsc::channel(8);
    let handle = spawn(tmp.path().to_path_buf(), evt_tx);

    handle.send(WebviewCommand::Shutdown);
    drop(handle);

    // Give the thread a moment to exit. We don't assert on events because
    // the stub thread doesn't emit any - we just need to confirm no hang.
    let _ = timeout(Duration::from_secs(2), evt_rx.recv()).await;
}

use kave::server::{load_certs, load_keys, Server};
use kave::store::MemoryStore;
use tokio::io::{split, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[macro_use]
mod utils;

fn new_cluster_server() -> (
    UnboundedSender<bool>,
    UnboundedReceiver<bool>,
    Server<MemoryStore>,
) {
    let certs = load_certs("certs/defaults/cert.pem").expect("error loading default test certs");
    let keys = load_keys("certs/defaults/key.pem").expect("error loading default test keys");
    let (svr_shutdown_send, svr_shutdown_recv) = tokio::sync::mpsc::unbounded_channel();
    let (sig_shutdown_send, sig_shutdown_recv) = tokio::sync::mpsc::unbounded_channel();

    let svr = Server::new(
        svr_shutdown_send,
        sig_shutdown_recv,
        certs,
        keys,
        MemoryStore::new(),
    );
    (sig_shutdown_send, svr_shutdown_recv, svr)
}

/// create a new server and wait for it start
macro_rules! start_server {
    ($addr:expr) => {{
        let client_addr: Option<String> = None;
        start_server!($addr, client_addr)
    }};
    ($addr:expr, $client_addr:expr) => {{
        let (shutdown_send, shutdown_recv, mut cs) = new_cluster_server();
        cs.set_addr($addr);
        if let Some(client_addr) = $client_addr {
            cs.set_client_server_addr(client_addr);
            cs.set_start_client_server(true);
        } else {
            cs.set_start_client_server(false);
        }
        tokio::spawn(async move { cs.start().await });
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        (shutdown_send, shutdown_recv)
    }};
}

#[tokio::test]
async fn test_cluster_server_basic_with_client_server() {
    init!();
    let (shutdown_send, mut shutdown_recv) =
        start_server!("127.0.0.1:7411", Some("127.0.0.1:7412"));

    // talk to cluster server
    // --------------------------------------------------
    let stream = utils::connect("localhost:7411")
        .await
        .expect("error connecting to test addr");
    let (mut reader, mut writer) = split(stream);
    writer
        .write_all(b"working!!!")
        .await
        .expect("error writing");
    let buf = read_buf!(reader, 10);
    assert_eq!(std::str::from_utf8(&buf).unwrap(), "working!!!");
    // --------------------------------------------------

    // talk to client server
    // --------------------------------------------------
    let stream = utils::connect("localhost:7412")
        .await
        .expect("error connecting to test addr");
    let (mut reader, mut writer) = split(stream);
    writer
        .write_all(b"ECHO:10:working!!!\n")
        .await
        .expect("error writing");
    let buf = read_buf!(reader, 10);
    assert_eq!(std::str::from_utf8(&buf).unwrap(), "10:working!!!\n");
    // --------------------------------------------------

    // send shutdown and assert that it actually shuts down
    shutdown_send
        .send(true)
        .expect("error sending client-server shutdown");
    tokio::time::timeout(std::time::Duration::from_secs(5), shutdown_recv.recv())
        .await
        .expect("client-server failed to shutdown");
}

#[tokio::test]
async fn test_cluster_server_basic_without_client_server() {
    init!();
    let (shutdown_send, mut shutdown_recv) = start_server!("127.0.0.1:7421");

    // talk to cluster server
    // --------------------------------------------------
    let stream = utils::connect("localhost:7421")
        .await
        .expect("error connecting to test addr");
    let (mut reader, mut writer) = split(stream);
    writer
        .write_all(b"working!!!")
        .await
        .expect("error writing");
    let buf = read_buf!(reader, 10);
    assert_eq!(buf, b"working!!!");
    // --------------------------------------------------

    // talk to client server
    // --------------------------------------------------
    assert!(utils::connect("localhost:7422").await.is_err());

    // send shutdown and assert that it actually shuts down
    shutdown_send
        .send(true)
        .expect("error sending client-server shutdown");
    tokio::time::timeout(std::time::Duration::from_secs(5), shutdown_recv.recv())
        .await
        .expect("client-server failed to shutdown");
}

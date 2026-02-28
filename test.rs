use axum::{routing::get, Router};
use tokio::net::UnixListener;
#[tokio::main]
async fn main() {
    let app = Router::new().route("/", get(|| async { "Hello" }));
    let listener = UnixListener::bind("/tmp/test.sock").unwrap();
    axum::serve(listener, app).await;
}

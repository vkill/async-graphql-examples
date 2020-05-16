use async_graphql::http::playground_source;
use async_graphql::{Schema, SubscriptionStream, WebSocketTransport};
use async_std::net::{TcpListener, TcpStream};
use async_std::task;
use async_tungstenite::tungstenite::Message;
use books::{MutationRoot, QueryRoot, Storage, SubscriptionRoot};
use bytes::Bytes;
use futures::channel::mpsc;
use futures::select;
use futures::{SinkExt, StreamExt};
use std::env;
use tide::{
    http::{headers, mime},
    Request, Response, StatusCode,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type GQLSchema = Schema<QueryRoot, MutationRoot, SubscriptionRoot>;

#[derive(Clone)]
struct AppState {
    schema: GQLSchema,
}

fn main() -> Result<()> {
    task::block_on(run())
}

async fn run() -> Result<()> {
    let schema = Schema::build(QueryRoot, MutationRoot, SubscriptionRoot)
        .data(Storage::default())
        .finish();
    let (mut stx, srx) = schema
        .clone()
        .subscription_connection(WebSocketTransport::default());

    let app_state = AppState { schema };
    let mut app = tide::with_state(app_state);

    async fn graphql(req: Request<AppState>) -> tide::Result<Response> {
        let schema = req.state().schema.clone();
        async_graphql_tide::graphql(req, schema, |query_builder| query_builder).await
    }

    app.at("/graphql").post(graphql).get(graphql);
    app.at("/").get(|_| async move {
        let resp = Response::new(StatusCode::Ok)
            .body_string(playground_source("/graphql", None))
            .set_header(headers::CONTENT_TYPE, mime::HTML.to_string());

        Ok(resp)
    });

    task::spawn(async move { run_ws(stx, srx).await });

    let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "localhost:8000".to_owned());
    println!("Playground: http://{}", listen_addr);
    app.listen(listen_addr).await?;

    Ok(())
}

async fn run_ws(
    stx: mpsc::Sender<Bytes>,
    srx: SubscriptionStream<QueryRoot, MutationRoot, SubscriptionRoot, WebSocketTransport>,
) -> Result<()> {
    let listen_addr = env::var("WS_LISTEN_ADDR").unwrap_or_else(|_| "localhost:8001".to_owned());
    println!("WS Server: ws://{}", listen_addr);
    let listener = TcpListener::bind(&listen_addr).await?;

    while let Ok((stream, _)) = listener.accept().await {
        // TODO
        // task::spawn(accept_ws(stream, stx.clone(), srx));
    }

    Ok(())
}

async fn accept_ws(
    stream: TcpStream,
    mut stx: mpsc::Sender<Bytes>,
    srx: SubscriptionStream<QueryRoot, MutationRoot, SubscriptionRoot, WebSocketTransport>,
) {
    let ws_stream = async_tungstenite::accept_async(stream)
        .await
        .expect("Error during the websocket handshake occurred");

    let (mut tx, rx) = ws_stream.split();

    let mut rx = rx.fuse();
    let mut srx = srx.fuse();

    loop {
        select! {
            bytes = srx.next() => {
                if let Some(bytes) = bytes {
                    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                        println!("ws srx text: {}", text);
                        if tx.send(Message::Text(text)).await.is_err()
                        {
                            return;
                        }
                    }
                } else {
                    return;
                }
            }
            msg = rx.next() => {
                if let Some(Ok(msg)) = msg {
                    if msg.is_text() {
                        println!("ws rx text: {}", msg);
                        if stx.send(Bytes::copy_from_slice(&msg.into_data())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::prelude::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn sample() -> Result<()> {
        task::block_on(async {
            let listen_addr = find_listen_addr().await;
            env::set_var("LISTEN_ADDR", format!("{}", listen_addr));
            let ws_listen_addr = find_listen_addr().await;
            env::set_var("WS_LISTEN_ADDR", format!("{}", ws_listen_addr));
            env::set_var("WS_LISTEN_ADDR_PORT", format!("{}", ws_listen_addr.port()));

            let server: task::JoinHandle<Result<()>> = task::spawn(async move {
                run().await?;

                Ok(())
            });

            let ws_client: task::JoinHandle<Result<()>> = task::spawn(async move {
                use async_tungstenite::async_std::connect_async;

                let ws_listen_addr_port = env::var("WS_LISTEN_ADDR_PORT").unwrap();

                task::sleep(Duration::from_millis(100)).await;

                let (mut ws_stream, _) =
                    connect_async(format!("ws://localhost:{}/graphql", ws_listen_addr_port))
                        .await?;
                while let Some(msg) = ws_stream.next().await {
                    let msg = msg?;
                    if msg.is_text() {
                        println!("{:?}", msg);
                    }
                }

                Ok(())
            });

            let client: task::JoinHandle<Result<()>> = task::spawn(async move {
                let listen_addr = env::var("LISTEN_ADDR").unwrap();

                task::sleep(Duration::from_millis(300)).await;

                let string = surf::post(format!("http://{}/graphql", listen_addr))
                    .body_string(
                        r#"{"query":"mutation { createBook(name: \"name1\", author: \"author1\") }"}"#
                            .to_owned(),
                    )
                    .set_header("Content-Type".parse().unwrap(), "application/json")
                    .recv_string()
                    .await?;

                println!("{}", string);
                assert_eq!(string, json!({"data":{"createBook": "0"}}).to_string());

                Ok(())
            });

            server.race(ws_client).await?;

            Ok(())
        })
    }

    async fn find_listen_addr() -> async_std::net::SocketAddr {
        async_std::net::TcpListener::bind("localhost:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
    }
}

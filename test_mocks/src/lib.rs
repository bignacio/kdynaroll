extern crate http;
extern crate kube;
extern crate kube_client;
extern crate tower_test;

use http::{Request, Response};
use kube::Client;
use kube_client::client::Body;
use std::collections::HashMap;
use std::pin::pin;
use tower_test::mock;

pub fn make_mock_kube_client<F>(response_map: HashMap<String, serde_json::Value>, on_request_received: F, count: i32) -> (tokio::task::JoinHandle<()>, tower_test::mock::Mock<Request<Body>, Response<Body>>)
where
    F: Fn(Request<Body>) + Send + 'static,
{
    let (mock_service, mock_handle) = mock::pair::<Request<Body>, Response<Body>>();

    let spawned = tokio::spawn(async move {
        let mut handle = pin!(mock_handle);
        for _ in 0..count {
            let (request, sender) = handle.next_request().await.expect("service not called");

            let content_key = request.uri().path_and_query().unwrap().as_str();
            let response_content = response_map
                .get(content_key)
                .expect(&("no response found for uri ".to_owned() + &content_key));

            on_request_received(request);

            sender.send_response(
                Response::builder()
                    .body(Body::from(serde_json::to_vec(&response_content).unwrap()))
                    .unwrap(),
            );
        }
    });

    return (spawned, mock_service);
}

pub fn make_mock_client_noop() -> kube::Client {
    let (mock_service, _) = mock::pair::<Request<Body>, Response<Body>>();
    let client = Client::new(mock_service, "default");

    return client;
}

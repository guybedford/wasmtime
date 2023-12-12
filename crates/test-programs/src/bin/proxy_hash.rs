use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::wasi::{
    http::{
        outgoing_handler::{self, OutgoingRequest},
        types::{Headers, OutgoingBody, Scheme},
    },
    io::streams,
};

wit_bindgen::generate!({
    generate_all,
    path: "../wasi-http/wit",
    world: "test-command",
});

fn main() {
    let server_authority1 = std::env::var("HANDLER_API_PROXY_STREAMING1").expect(
        "Expected HANDLER_API_PROXY_STREAMING1 env var server authority for api_proxy_streaming incoming handler",
    );
    let server_authority2 = std::env::var("HANDLER_API_PROXY_STREAMING2").expect(
        "Expected HANDLER_API_PROXY_STREAMING2 env var server authority for api_proxy_streaming incoming handler",
    );

    do_wasi_http_hash_all(&server_authority1, &server_authority2);
}

fn do_wasi_http_hash_all(server: &str, other_server: &str) {
    let bodies = HashMap::from([
        ("/a", "’Twas brillig, and the slithy toves"),
        ("/b", "Did gyre and gimble in the wabe:"),
        ("/c", "All mimsy were the borogoves,"),
        ("/d", "And the mome raths outgrabe."),
    ]);

    let headers = Headers::new();
    for path in bodies.keys() {
        headers
            .append(
                &"url".to_string(),
                &format!("http://{other_server}{path}").as_bytes().to_vec(),
            )
            .unwrap();
    }

    let request = OutgoingRequest::new(headers);

    request.set_scheme(Some(&Scheme::Http)).unwrap();
    request.set_authority(Some(&server)).unwrap();
    request.set_path_with_query(Some("/hash-all")).unwrap();

    let outgoing_body = request.body().unwrap();
    _ = outgoing_body.write().unwrap();

    OutgoingBody::finish(outgoing_body, None).unwrap();

    let future_response = outgoing_handler::handle(request, None).unwrap();

    let incoming_response = match future_response.get() {
        Some(result) => result.expect("response already taken"),
        None => {
            let pollable = future_response.subscribe();
            pollable.block();
            future_response
                .get()
                .expect("incoming response available")
                .expect("response already taken")
        }
    }
    .unwrap();

    drop(future_response);

    assert_eq!(incoming_response.status(), 200);

    let headers_handle = incoming_response.headers();

    drop(headers_handle);

    let incoming_body = incoming_response
        .consume()
        .expect("incoming response has no body stream");

    drop(incoming_response);

    let input_stream = incoming_body.stream().unwrap();
    let input_stream_pollable = input_stream.subscribe();

    let mut received = Vec::new();
    loop {
        input_stream_pollable.block();

        let mut body_chunk = match input_stream.read(1024 * 1024) {
            Ok(c) => c,
            Err(streams::StreamError::Closed) => break,
            Err(e) => panic!("input_stream read failed: {e:?}"),
        };

        if !body_chunk.is_empty() {
            received.append(&mut body_chunk);
        }
    }

    assert!(received.len() > 0);
    let body = std::str::from_utf8(&received).unwrap();
    for line in body.lines() {
        let (url, hash) = line
            .split_once(": ")
            .ok_or_else(|| panic!("expected string of form `<url>: <sha-256>`; got {line}"))
            .unwrap();

        let path = url
            .strip_prefix(&format!("http://{other_server}"))
            .ok_or_else(|| panic!("expected string with prefix {other_server}; got {url}"))
            .unwrap();

        let mut hasher = Sha256::new();
        hasher.update(
            bodies
                .get(path)
                .ok_or_else(|| panic!("unexpected path: {path}"))
                .unwrap(),
        );

        use base64::Engine;
        assert_eq!(
            hash,
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(hasher.finalize())
        );
    }
}
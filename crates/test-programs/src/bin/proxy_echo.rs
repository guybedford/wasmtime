wit_bindgen::generate!({
  path: "../wasi-http/wit",
  world: "test-command"
});

use std::iter;

use crate::wasi::http::outgoing_handler;
use crate::wasi::http::types::{Headers, Method, OutgoingBody, OutgoingRequest, Scheme};

use crate::wasi::io::streams;

fn main() {
    let server_authority1 = std::env::var("HANDLER_API_PROXY_STREAMING1").expect(
        "Expected HANDLER_API_PROXY_STREAMING env var server authority for api_proxy_streaming incoming handler",
    );

    let server_authority2 = std::env::var("HANDLER_API_PROXY_STREAMING2").expect(
        "Expected HANDLER_API_PROXY_STREAMING env var server authority for api_proxy_streaming incoming handler",
    );

    do_wasi_http_echo(&server_authority1, "/echo", None);
    let url_header = format!("http://{server_authority1}/echo");
    do_wasi_http_echo(&server_authority2, "/double-echo", Some(&url_header));
}

fn do_wasi_http_echo(server: &str, path: &str, url_header: Option<&str>) {
    let body = {
        // A sorta-random-ish megabyte
        let mut n = 0_u8;
        iter::repeat_with(move || {
            n = n.wrapping_add(251);
            n
        })
        .take(1024 * 1024)
        .collect::<Vec<_>>()
    };

    let headers = if let Some(url_header) = url_header {
        Headers::from_list(&[
            ("url".into(), url_header.as_bytes().into()),
            ("content-type".into(), b"application/octet-stream".into()),
        ])
        .unwrap()
    } else {
        Headers::from_list(&[("content-type".into(), b"application/octet-stream".into())]).unwrap()
    };

    let request = OutgoingRequest::new(headers);

    request.set_method(&Method::Post).unwrap();
    request.set_scheme(Some(&Scheme::Http)).unwrap();
    request.set_authority(Some(server)).unwrap();
    request.set_path_with_query(Some(path)).unwrap();

    let outgoing_body = request.body().unwrap();
    let outgoing_body_stream = outgoing_body.write().unwrap();

    // write as much to the outgoing body as we can
    let body_chunks = body.chunks(16 * 1024);
    let mut body_chunks_iter = body_chunks.into_iter().peekable();
    while let Some(chunk) = body_chunks_iter.peek() {
        if outgoing_body_stream.check_write().unwrap() >= chunk.len() as u64 {
            outgoing_body_stream
                .write(body_chunks_iter.next().unwrap())
                .unwrap();
        } else {
            break;
        }
    }

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

    assert_eq!(
        headers_handle.get(&String::from("content-type"))[0],
        b"application/octet-stream"
    );

    drop(headers_handle);

    let incoming_body = incoming_response
        .consume()
        .expect("incoming response has no body stream");

    drop(incoming_response);

    let input_stream = incoming_body.stream().unwrap();
    let mut received = Vec::new();

    while let Some(chunk) = body_chunks_iter.peek() {
        if outgoing_body_stream.check_write().unwrap() >= chunk.len() as u64 {
            outgoing_body_stream
                .write(body_chunks_iter.next().unwrap())
                .unwrap();
        } else {
            panic!("Unable to write a single chunk");
        }
        input_stream.subscribe().block();
        let read_result = input_stream.read(1024 * 1024);
        let mut body_chunk = match read_result {
            Ok(c) => c,
            Err(streams::StreamError::Closed) => {
                panic!("input stream finished before output stream")
            }
            Err(e) => panic!("input_stream read failed: {e:?}"),
        };
        if !body_chunk.is_empty() {
            received.append(&mut body_chunk);
        }
        outgoing_body_stream.subscribe().block();
    }

    drop(outgoing_body_stream);

    OutgoingBody::finish(outgoing_body, None).unwrap();

    loop {
        input_stream.subscribe().block();
        let read_result = input_stream.read(1024 * 1024);
        let mut body_chunk = match read_result {
            Ok(c) => c,
            Err(streams::StreamError::Closed) => break,
            Err(e) => panic!("input_stream read failed: {e:?}"),
        };
        if !body_chunk.is_empty() {
            received.append(&mut body_chunk);
        }
    }

    assert!(matches!(
        input_stream.read(1024 * 1024),
        Err(streams::StreamError::Closed)
    ));

    if body != received {
        panic!(
            "body content mismatch (expected length {}; actual length {})",
            body.len(),
            received.len()
        );
    }
}

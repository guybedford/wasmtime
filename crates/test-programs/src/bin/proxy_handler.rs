use crate::{
    wasi::http::{
        outgoing_handler::{self, OutgoingRequest},
        types::{Headers, OutgoingBody, Scheme},
    },
    wasi::io::streams,
};

wit_bindgen::generate!({
    generate_all,
    path: "../wasi-http/wit",
    world: "bindings",
});

fn main() {
    let server_authority = std::env::var("HANDLER_API_PROXY").expect(
        "Expected HANDLER_API_PROXY env var server authority for api_proxy incoming handler",
    );

    let headers = Headers::new();
    headers
        .append(&"custom-forbidden-header".to_string(), &b"yes".to_vec())
        .unwrap();

    let request = OutgoingRequest::new(headers);

    request.set_scheme(Some(&Scheme::Http)).unwrap();
    request.set_authority(Some(&server_authority)).unwrap();
    match request.set_path_with_query(Some("/test-path")) {
        Ok(_) => {}
        Err(e) => dbg!(e),
    };

    let outgoing_body = request.body().unwrap();
    // _ = outgoing_body.write().unwrap();

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

    assert_eq!(received, b"hello, world!");
}

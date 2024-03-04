use anyhow::{bail, Context, Result};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{body::Bytes, service::service_fn, Request, Response};
use std::{
    future::Future,
    net::{SocketAddr, TcpStream},
    sync::Arc,
    thread::JoinHandle,
};
use tokio::net::TcpListener;
use wasmtime::{
    component::{Component, Linker, ResourceTable},
    Engine, InstancePre, Store,
};
use wasmtime_wasi::preview2::{pipe::MemoryOutputPipe, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::{
    bindings::http::types,
    body::{HyperIncomingBody, HyperOutgoingBody},
    hyper_response_error,
};
use wasmtime_wasi_http::{io::TokioIo, proxy::Proxy, WasiHttpCtx, WasiHttpView};

use crate::Ctx;

async fn test(
    mut req: Request<hyper::body::Incoming>,
) -> http::Result<Response<BoxBody<Bytes, std::convert::Infallible>>> {
    tracing::debug!("preparing mocked response",);
    let method = req.method().to_string();
    let body = req.body_mut().collect().await.unwrap();
    let buf = body.to_bytes();
    tracing::trace!("hyper request body size {:?}", buf.len());

    Response::builder()
        .status(http::StatusCode::OK)
        .header("x-wasmtime-test-method", method)
        .header("x-wasmtime-test-uri", req.uri().to_string())
        .body(Full::<Bytes>::from(buf).boxed())
}

pub struct Server {
    addr: SocketAddr,
    worker: Option<JoinHandle<Result<()>>>,
}

impl Server {
    fn new<F>(
        run: impl FnOnce(TokioIo<tokio::net::TcpStream>) -> F + Send + 'static,
    ) -> Result<Self>
    where
        F: Future<Output = Result<()>>,
    {
        let thread = std::thread::spawn(|| -> Result<_> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to start tokio runtime")?;
            let listener = rt.block_on(async move {
                let addr = SocketAddr::from(([127, 0, 0, 1], 0));
                TcpListener::bind(addr).await.context("failed to bind")
            })?;
            Ok((rt, listener))
        });
        let (rt, listener) = thread.join().unwrap()?;
        let addr = listener.local_addr().context("failed to get local addr")?;
        let worker = std::thread::spawn(move || {
            tracing::debug!("dedicated thread to start listening");
            rt.block_on(async move {
                tracing::debug!("preparing to accept connection");
                let (stream, _) = listener.accept().await.map_err(anyhow::Error::from)?;
                run(TokioIo::new(stream)).await
            })
        });
        Ok(Self {
            worker: Some(worker),
            addr,
        })
    }

    pub async fn new_from_component(engine: Engine, component: Component) -> Result<Self> {
        tracing::debug!("initializing incoming handler server component");

        let mut linker: Linker<Ctx> = Linker::new(&engine);
        wasmtime_wasi::preview2::command::add_to_linker(&mut linker)?;
        wasmtime_wasi_http::proxy::add_only_http_to_linker(&mut linker)?;

        let instance_pre = linker.instantiate_pre(&component)?;

        Self::new(|io| async move {
            let mut builder = hyper::server::conn::http1::Builder::new();
            let http = builder.keep_alive(false).pipeline_flush(true);

            tracing::debug!("preparing to bind connection to service");
            let conn = http
                .serve_connection(
                    io,
                    service_fn(move |req: Request<hyper::body::Incoming>| async {
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        let engine = engine.clone();
                        let instance_pre = instance_pre.clone();

                        tokio::task::spawn(async move {
                            let stdout = MemoryOutputPipe::new(4096);
                            let stderr = MemoryOutputPipe::new(4096);

                            // Create our wasi context.
                            let mut builder = WasiCtxBuilder::new();
                            builder.stdout(stdout.clone());
                            builder.stderr(stderr.clone());

                            let ctx = Ctx {
                                table: ResourceTable::new(),
                                wasi: builder.build(),
                                http: WasiHttpCtx {},
                                stderr,
                                stdout,
                                send_request: None,
                            };
                            let mut store = Store::new(&engine, ctx);
                            let (parts, body) = req.into_parts();
                            let req = Request::from_parts(
                                parts,
                                body.map_err(hyper_response_error).boxed(),
                            );
                            let req = store.data_mut().new_incoming_request(req)?;
                            let out = store.data_mut().new_response_outparam(sender)?;
                            // let (proxy, _) =
                            // Proxy::instantiate_pre(&mut store, &instance_pre).await?;
                            // proxy
                            // .wasi_http_incoming_handler()
                            // .call_handle(store, req, out)
                            // .await
                            Ok::<_, anyhow::Error>(())
                        });

                        match receiver.await {
                            Ok(Ok(res)) => Ok(res),
                            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
                            Err(e) => panic!("server exited without writing response: {}", e),
                        }
                    }),
                )
                .await;
            tracing::trace!("connection result {:?}", conn);
            conn?;
            Ok(())
        })
    }

    pub fn http1() -> Result<Self> {
        tracing::debug!("initializing http1 server");
        Self::new(|io| async move {
            let mut builder = hyper::server::conn::http1::Builder::new();
            let http = builder.keep_alive(false).pipeline_flush(true);

            tracing::debug!("preparing to bind connection to service");
            let conn = http.serve_connection(io, service_fn(test)).await;
            tracing::trace!("connection result {:?}", conn);
            conn?;
            Ok(())
        })
    }

    pub fn http2() -> Result<Self> {
        tracing::debug!("initializing http2 server");
        Self::new(|io| async move {
            let mut builder = hyper::server::conn::http2::Builder::new(TokioExecutor);
            let http = builder.max_concurrent_streams(20);

            tracing::debug!("preparing to bind connection to service");
            let conn = http.serve_connection(io, service_fn(test)).await;
            tracing::trace!("connection result {:?}", conn);
            if let Err(e) = &conn {
                let message = e.to_string();
                if message.contains("connection closed before reading preface")
                    || message.contains("unspecific protocol error detected")
                {
                    return Ok(());
                }
            }
            conn?;
            Ok(())
        })
    }

    pub fn addr(&self) -> String {
        format!("localhost:{}", self.addr.port())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        tracing::debug!("shutting down http1 server");
        // Force a connection to happen in case one hasn't happened already.
        let _ = TcpStream::connect(&self.addr);

        // If the worker fails with an error, report it here but don't panic.
        // Some tests don't make a connection so the error will be that the tcp
        // stream created above is closed immediately afterwards. Let the test
        // independently decide if it failed or not, and this should be in the
        // logs to assist with debugging if necessary.
        let worker = self.worker.take().unwrap();
        if let Err(e) = worker.join().unwrap() {
            eprintln!("worker failed with error {e:?}");
        }
    }
}

#[derive(Clone)]
/// An Executor that uses the tokio runtime.
struct TokioExecutor;

impl<F> hyper::rt::Executor<F> for TokioExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn(fut);
    }
}

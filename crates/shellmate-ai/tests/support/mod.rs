//! Tokio-clock HTTP body for deadline tests (rig's PacedStreamingClient uses wall time).
use futures::StreamExt;
use rig_core::http_client::{
    self, Bytes, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use std::{future::Future, time::Duration};
#[derive(Clone)]
pub struct TimedStream {
    pub chunks: Vec<String>,
    pub gap: Duration,
}
impl HttpClientExt for TimedStream {
    fn send<T: Into<Bytes> + Send, U: From<Bytes> + Send + 'static>(
        &self,
        _: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static {
        std::future::ready(Err(http_client::Error::StreamEnded))
    }
    fn send_multipart<U: From<Bytes> + Send + 'static>(
        &self,
        _: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static {
        std::future::ready(Err(http_client::Error::StreamEnded))
    }
    fn send_streaming<T: Into<Bytes> + Send>(
        &self,
        _: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + Send {
        let Self { chunks, gap } = self.clone();
        async move {
            let body = futures::stream::iter(chunks)
                .then(move |chunk| async move {
                    tokio::time::sleep(gap).await;
                    Ok(Bytes::from(chunk))
                })
                .chain(futures::stream::pending());
            let body: http_client::BoxedStream = Box::pin(body);
            Ok(Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(body)
                .unwrap())
        }
    }
}

// GPUI enables serde_json/preserve_order in combined app/AI test builds. Match SSE
// event semantics so the partial fixture cannot accidentally include completion.
pub fn before_completion(input: &str, anthropic: bool) -> String {
    input
        .split_inclusive("\n\n")
        .take_while(|event| {
            if anthropic {
                !event.starts_with("event: content_block_stop")
            } else {
                !event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
                    .any(|data| !data["choices"][0]["finish_reason"].is_null())
            }
        })
        .collect()
}

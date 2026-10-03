//! Shared affine body lifetime and HTTP header limits for native HTTP/1 RPC bindings.
use http::HeaderMap;
use http_body_util::{BodyExt, StreamBody};
use tonic::{body::BoxBody, Status};

pub fn hold_body<G: Send + 'static>(body: BoxBody, guard: G) -> BoxBody {
    StreamBody::new(futures::stream::try_unfold(
        (body, guard),
        |(mut body, guard)| async move {
            let frame = body.frame().await.transpose()?;
            Ok::<_, Status>(frame.map(|frame| (frame, (body, guard))))
        },
    ))
    .boxed_unsync()
}
pub fn bounded_headers(headers: &HeaderMap) -> bool {
    headers.len() <= 64
        && headers
            .iter()
            .try_fold(0usize, |total, (name, value)| {
                total
                    .checked_add(name.as_str().len())?
                    .checked_add(value.as_bytes().len())?
                    .checked_add(4)
            })
            .is_some_and(|total| total <= 32768)
}

#![doc = include_str!("../README.md")]

#[cfg(test)]
mod tests;

pub mod errors;
pub mod governor;
use crate::governor::{Governor, GovernorConfig};
use ::governor::clock::{Clock, DefaultClock};
use axum::body::Body;
pub use errors::GovernorError;
use http::response::Response;

use http::request::Request;
use http::HeaderMap;
use pin_project::pin_project;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin};
use tower::{Layer, Service};

/// The Layer type that implements tower::Layer and is passed into `.layer()`
pub struct GovernorLayer {
    pub config: Arc<GovernorConfig>,
}

impl<S> Layer<S> for GovernorLayer {
    type Service = Governor<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Governor::new(inner, &self.config)
    }
}

/// https://stegosaurusdormant.com/understanding-derive-clone/
impl Clone for GovernorLayer {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
        }
    }
}
// Implement tower::Service for Governor
impl<S, ReqBody> Service<Request<ReqBody>> for Governor<S>
where
    S: Service<Request<ReqBody>, Response = Response<Body>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        if let Some(configured_methods) = &self.methods {
            if !configured_methods.contains(req.method()) {
                // The request method is not configured, we're ignoring this one.
                let future = self.inner.call(req);
                return ResponseFuture {
                    inner: Kind::Passthrough { future },
                };
            }
        }
        match self.limiter.check() {
            Ok(_) => {
                let future = self.inner.call(req);
                ResponseFuture {
                    inner: Kind::Passthrough { future },
                }
            }

            Err(negative) => {
                let wait_time = negative
                    .wait_time_from(DefaultClock::default().now())
                    .as_secs();

                #[cfg(feature = "tracing")]
                {
                    tracing::info!("Rate limit exceeded, quota reset in {}s", &wait_time);
                }
                let mut headers = HeaderMap::new();
                headers.insert("x-ratelimit-after", wait_time.into());
                headers.insert("retry-after", wait_time.into());

                let error_response = self.error_handler()(GovernorError::TooManyRequests {
                    wait_time,
                    headers: Some(headers),
                });

                ResponseFuture {
                    inner: Kind::Error {
                        error_response: Some(error_response),
                    },
                }
            }
        }
    }
}

#[derive(Debug)]
#[pin_project]
/// Response future for [`Governor`].
pub struct ResponseFuture<F> {
    #[pin]
    inner: Kind<F>,
}

#[derive(Debug)]
#[pin_project(project = KindProj)]
enum Kind<F> {
    Passthrough {
        #[pin]
        future: F,
    },
    Error {
        error_response: Option<Response<Body>>,
    },
}

impl<F, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<Body>, E>>,
{
    type Output = Result<Response<Body>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().inner.project() {
            KindProj::Passthrough { future } => future.poll(cx),
            KindProj::Error { error_response } => Poll::Ready(Ok(error_response.take().expect("
                <Governor as Service<Request<_>>>::call must produce Response<String> when GovernorError occurs.
            "))),
        }
    }
}

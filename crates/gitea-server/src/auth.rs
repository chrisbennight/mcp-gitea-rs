use axum::{
    body::Body,
    http::{Request, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct BearerLayer {
    current: Vec<u8>,
    previous: Option<Vec<u8>>,
}

impl BearerLayer {
    pub fn new(current: String, previous: Option<String>) -> Self {
        Self {
            current: current.into_bytes(),
            previous: previous.map(String::into_bytes),
        }
    }
}

impl<S> Layer<S> for BearerLayer {
    type Service = BearerService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BearerService {
            inner,
            current: self.current.clone(),
            previous: self.previous.clone(),
        }
    }
}

#[derive(Clone)]
pub struct BearerService<S> {
    inner: S,
    current: Vec<u8>,
    previous: Option<Vec<u8>>,
}

impl<S> Service<Request<Body>> for BearerService<S>
where
    S: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future =
        futures_util::future::Either<S::Future, std::future::Ready<Result<Response, S::Error>>>;

    fn poll_ready(
        &mut self,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let valid = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|candidate| {
                matches_secret(candidate.as_bytes(), &self.current)
                    || self
                        .previous
                        .as_deref()
                        .is_some_and(|previous| matches_secret(candidate.as_bytes(), previous))
            });
        if valid {
            futures_util::future::Either::Left(self.inner.call(request))
        } else {
            let response = StatusCode::UNAUTHORIZED.into_response();
            futures_util::future::Either::Right(std::future::ready(Ok(response)))
        }
    }
}

fn matches_secret(candidate: &[u8], expected: &[u8]) -> bool {
    candidate.len() == expected.len() && candidate.ct_eq(expected).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_exact_secret() {
        assert!(matches_secret(b"correct", b"correct"));
        assert!(!matches_secret(b"incorrect", b"correct"));
        assert!(!matches_secret(b"correct-extra", b"correct"));
    }
}

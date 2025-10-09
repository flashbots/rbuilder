//! A layer responsible for implementing flashbots-style authentication
//! by signing the request body with a private key and adding the signature
//! to the request headers.
//! Based on https://github.com/paradigmxyz/mev-share-rs/tree/a75c5959e98a79031a89f8893c97528e8f726826 but upgraded to alloy

use std::{
    error::Error,
    task::{Context, Poll},
};

use alloy_primitives::{hex, keccak256};
use alloy_signer::Signer;
use futures_util::future::BoxFuture;

use http::{header::HeaderValue, HeaderName, Request};
use hyper::Body;

use tower::{Layer, Service};

static FLASHBOTS_HEADER: HeaderName = HeaderName::from_static("x-flashbots-signature");

/// Layer that applies [`FlashbotsSigner`] which adds a request header with a signed payload.
#[derive(Clone, Debug)]
pub struct FlashbotsSignerLayer<S> {
    signer: S,
}

impl<S> FlashbotsSignerLayer<S> {
    /// Creates a new [`FlashbotsSignerLayer`] with the given signer.
    pub fn new(signer: S) -> Self {
        FlashbotsSignerLayer { signer }
    }
}

impl<S: Clone, I> Layer<I> for FlashbotsSignerLayer<S> {
    type Service = FlashbotsSigner<S, I>;

    fn layer(&self, inner: I) -> Self::Service {
        FlashbotsSigner {
            signer: self.signer.clone(),
            inner,
        }
    }
}

/// Middleware that signs the request body and adds the signature to the x-flashbots-signature
/// header. For more info, see <https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#authentication>
#[derive(Clone, Debug)]
pub struct FlashbotsSigner<S, I> {
    signer: S,
    inner: I,
}

impl<S, I> Service<Request<Body>> for FlashbotsSigner<S, I>
where
    I: Service<Request<Body>> + Clone + Send + 'static,
    I::Future: Send,
    I::Error: Into<Box<dyn Error + Send + Sync>> + 'static,
    S: Signer + Clone + Send + Sync + 'static,
{
    type Response = I::Response;
    type Error = Box<dyn Error + Send + Sync>;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let clone = self.inner.clone();
        // wait for service to be ready
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let signer = self.signer.clone();

        let (mut parts, body) = request.into_parts();

        // if method is not POST, return an error.
        if parts.method != http::Method::POST {
            return Box::pin(async move {
                Err(format!("Invalid method: {}", parts.method.as_str()).into())
            });
        }

        // if content-type is not json, or signature already exists, just pass through the request
        let is_json = parts
            .headers
            .get(http::header::CONTENT_TYPE)
            .map(|v| v == HeaderValue::from_static("application/json"))
            .unwrap_or(false);
        let has_sig = parts.headers.contains_key(FLASHBOTS_HEADER.clone());

        if !is_json || has_sig {
            return Box::pin(async move {
                let request = Request::from_parts(parts, body);
                inner.call(request).await.map_err(Into::into)
            });
        }

        // otherwise, sign the request body and add the signature to the header
        Box::pin(async move {
            let body_bytes = hyper::body::to_bytes(body).await?;

            // sign request body and insert header
            let signature = signer
                .sign_message(format!("{:?}", keccak256(&body_bytes)).as_bytes())
                .await?;

            let header_val = HeaderValue::from_str(&format!(
                "{:?}:0x{}",
                signer.address(),
                hex::encode(signature.as_bytes())
            ))?;
            parts.headers.insert(FLASHBOTS_HEADER.clone(), header_val);

            let request = Request::from_parts(parts, Body::from(body_bytes.clone()));
            inner.call(request).await.map_err(Into::into)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_signer_local::PrivateKeySigner;
    use http::Response;
    use hyper::Body;
    use std::convert::Infallible;
    use tower::{service_fn, ServiceExt};

    #[tokio::test]
    async fn test_signature() {
        let fb_signer = PrivateKeySigner::random();

        // mock service that returns the request headers
        let svc = FlashbotsSigner {
            signer: fb_signer.clone(),
            inner: service_fn(|_req: Request<Body>| async {
                let (parts, _) = _req.into_parts();

                let mut res = Response::builder();
                for (k, v) in parts.headers.iter() {
                    res = res.header(k, v);
                }
                let res = res.body(Body::empty()).unwrap();
                Ok::<_, Infallible>(res)
            }),
        };

        // build request
        let bytes = vec![1u8; 32];
        let req = Request::builder()
            .method(http::Method::POST)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes.clone()))
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();

        let header = res.headers().get("x-flashbots-signature").unwrap();
        let header = header.to_str().unwrap();
        let header = header.split(":0x").collect::<Vec<_>>();
        let header_address = header[0];
        let header_signature = header[1];

        let signer_address = format!("{:?}", fb_signer.address());
        let expected_signature = fb_signer
            .sign_message(format!("{:?}", keccak256(bytes.clone())).as_bytes())
            .await
            .unwrap();
        let expected_signature = hex::encode(expected_signature.as_bytes());
        // verify that the header contains expected address and signature
        assert_eq!(header_address, signer_address);
        assert_eq!(header_signature, expected_signature);
    }

    #[tokio::test]
    async fn test_skips_non_json() {
        let fb_signer = PrivateKeySigner::random();

        // mock service that returns the request headers
        let svc = FlashbotsSigner {
            signer: fb_signer.clone(),
            inner: service_fn(|_req: Request<Body>| async {
                let (parts, _) = _req.into_parts();

                let mut res = Response::builder();
                for (k, v) in parts.headers.iter() {
                    res = res.header(k, v);
                }
                let res = res.body(Body::empty()).unwrap();
                Ok::<_, Infallible>(res)
            }),
        };

        // build plain text request
        let bytes = vec![1u8; 32];
        let req = Request::builder()
            .method(http::Method::POST)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body(Body::from(bytes.clone()))
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();

        // response should not contain a signature header
        let header = res.headers().get("x-flashbots-signature");
        assert!(header.is_none());
    }

    #[tokio::test]
    async fn test_returns_error_when_not_post() {
        let fb_signer = PrivateKeySigner::random();

        // mock service that returns the request headers
        let svc = FlashbotsSigner {
            signer: fb_signer.clone(),
            inner: service_fn(|_req: Request<Body>| async {
                let (parts, _) = _req.into_parts();

                let mut res = Response::builder();
                for (k, v) in parts.headers.iter() {
                    res = res.header(k, v);
                }
                let res = res.body(Body::empty()).unwrap();
                Ok::<_, Infallible>(res)
            }),
        };

        // build plain text request
        let bytes = vec![1u8; 32];
        let req = Request::builder()
            .method(http::Method::GET)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes.clone()))
            .unwrap();

        let res = svc.oneshot(req).await;

        // should be an error
        assert!(res.is_err());
    }

    /// Uses a static private key and compares the signature generated by this package to the signature
    /// generated by the `cast` CLI.
    /// Test copied from https://github.com/flashbots/go-utils/blob/main/signature/signature_test.go#L146 501d395be6a9802494ef1ef25a755acaa4448c17 (TestSignatureCreateCompareToCastAndEthers)
    #[tokio::test]
    async fn test_signature_cast() {
        let fb_signer: PrivateKeySigner =
            "fad9c8855b740a0b7ed4c221dbad0f33a83a49cad6b3fe8d5817ac83d38b6a19"
                .parse()
                .unwrap();
        // mock service that returns the request headers
        let svc = FlashbotsSigner {
            signer: fb_signer.clone(),
            inner: service_fn(|_req: Request<Body>| async {
                let (parts, _) = _req.into_parts();

                let mut res = Response::builder();
                for (k, v) in parts.headers.iter() {
                    res = res.header(k, v);
                }
                let res = res.body(Body::empty()).unwrap();
                Ok::<_, Infallible>(res)
            }),
        };

        // build request
        let bytes = "Hello".as_bytes();
        let req = Request::builder()
            .method(http::Method::POST)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();

        let header = res.headers().get("x-flashbots-signature").unwrap();
        let header = header.to_str().unwrap();
        let header = header.split(":0x").collect::<Vec<_>>();
        let header_address = header[0];
        let header_signature = header[1];
        // I generated the signature using the cast CLI:
        // 	cast wallet sign --private-key fad9c8855b740a0b7ed4c221dbad0f33a83a49cad6b3fe8d5817ac83d38b6a19  $(cast from-utf8 $(cast keccak Hello))
        let signer_address = "0x96216849c49358B10257cb55b28eA603c874b05E".to_lowercase();
        let expected_signature = "1446053488f02d460c012c84c4091cd5054d98c6cfca01b65f6c1a72773e80e60b8a4931aeee7ed18ce3adb45b2107e8c59e25556c1f871a8334e30e5bddbed21c";
        // verify that the header contains expected address and signature
        assert_eq!(header_address, signer_address);
        assert_eq!(header_signature, expected_signature);
    }
}

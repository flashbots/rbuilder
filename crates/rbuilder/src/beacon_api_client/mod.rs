use beacon_api_client::{mainnet::Client as bClient, Error, Topic};
use mev_share_sse::client::EventStream;
use reth::rpc::types::beacon::events::PayloadAttributesEvent;
use serde::Deserialize;
use std::{collections::HashMap, fmt::Debug};
use url::Url;

#[derive(Deserialize, Clone)]
#[serde(try_from = "String")]
pub struct Client {
    inner: bClient,
}

impl Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish()
    }
}

impl Client {
    pub fn new(endpoint: Url) -> Self {
        Self {
            inner: bClient::new(endpoint),
        }
    }

    pub async fn get_spec(&self) -> Result<HashMap<String, String>, Error> {
        self.inner.get_spec().await
    }

    pub async fn get_events<T: Topic>(&self) -> Result<EventStream<T::Data>, Error> {
        self.inner.get_events::<T>().await
    }
}

impl TryFrom<String> for Client {
    type Error = url::ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        let url = Url::parse(&s)?;
        Ok(Client::new(url))
    }
}

pub struct PayloadAttributesTopic;

impl Topic for PayloadAttributesTopic {
    const NAME: &'static str = "payload_attributes";

    type Data = PayloadAttributesEvent;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[ignore]
    #[tokio::test]
    async fn test_get_spec() {
        let client = Client::new(Url::parse("http://localhost:8000").unwrap());
        let spec = client.get_spec().await.unwrap();

        // validate that the spec contains the genesis fork version
        spec.get("GENESIS_FORK_VERSION").unwrap();
    }

    #[tokio::test]
    async fn test_get_events() {
        let client = Client::new(Url::parse("http://localhost:8000").unwrap());
        let mut stream = client.get_events::<PayloadAttributesTopic>().await.unwrap();

        // validate that the stream is not empty
        // TODO: add timeout
        let event = stream.next().await.unwrap().unwrap();
        print!("{:?}", event);
    }
}

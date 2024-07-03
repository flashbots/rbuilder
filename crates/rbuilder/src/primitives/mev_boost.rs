use crate::mev_boost::{RelayClient, SubmitBlockErr, SubmitBlockRequest};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use std::{sync::Arc, time::Duration};
use url::Url;

pub type MevBoostRelayID = String;

#[derive(Debug, Clone)]
pub struct MevBoostRelay {
    pub id: MevBoostRelayID,
    pub client: RelayClient,
    /// The lower priority -> more important
    pub priority: usize,
    /// true->ssz false->json
    pub use_ssz_for_submit: bool,
    pub use_gzip_for_submit: bool,
    pub optimistic: bool,
    pub submission_rate_limiter: Option<Arc<DefaultDirectRateLimiter>>,
}

impl MevBoostRelay {
    #[allow(clippy::too_many_arguments)]
    pub fn try_from_name_or_url(
        id: &str,
        name_or_url: &str,
        priority: usize,
        use_ssz_for_submit: bool,
        use_gzip_for_submit: bool,
        optimistic: bool,
        authorization_header: Option<String>,
        builder_id_header: Option<String>,
        api_token_header: Option<String>,
        interval_between_submissions: Option<Duration>,
    ) -> eyre::Result<Self> {
        let client = {
            let url: Url = name_or_url.parse()?;
            RelayClient::from_url(
                url,
                authorization_header,
                builder_id_header,
                api_token_header,
            )
        };

        let submission_rate_limiter = interval_between_submissions.map(|d| {
            Arc::new(RateLimiter::direct(
                Quota::with_period(d).expect("Rate limiter time period"),
            ))
        });

        Ok(MevBoostRelay {
            id: id.to_string(),
            client,
            priority,
            use_ssz_for_submit,
            use_gzip_for_submit,
            optimistic,
            submission_rate_limiter,
        })
    }

    pub async fn submit_block(&self, data: &SubmitBlockRequest) -> Result<(), SubmitBlockErr> {
        self.client
            .submit_block(data, self.use_ssz_for_submit, self.use_gzip_for_submit)
            .await
    }
}

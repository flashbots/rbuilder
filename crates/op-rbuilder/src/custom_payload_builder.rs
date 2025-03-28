use crate::tx_signer::Signer;
use url::Url;

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CustomOpPayloadBuilderBuilder {
    pub builder_signer: Option<Signer>,
    pub flashblocks_ws_url: Option<String>,
    pub chain_block_time: Option<u64>,
    pub flashblock_block_time: Option<u64>,
    pub supervisor_url: Option<Url>,
    pub supervisor_safety_level: Option<String>,
}

impl CustomOpPayloadBuilderBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builder_signer(mut self, signer: Option<Signer>) -> Self {
        self.builder_signer = signer;
        self
    }

    pub fn supervisor_url(mut self, url: Option<Url>) -> Self {
        self.supervisor_url = url;
        self
    }

    pub fn supervisor_safety_level(mut self, level: Option<String>) -> Self {
        self.supervisor_safety_level = level;
        self
    }

    pub fn flashblocks_ws_url(mut self, url: String) -> Self {
        self.flashblocks_ws_url = Some(url);
        self
    }

    pub fn chain_block_time(mut self, time: u64) -> Self {
        self.chain_block_time = Some(time);
        self
    }

    pub fn flashblock_block_time(mut self, time: u64) -> Self {
        self.flashblock_block_time = Some(time);
        self
    }

    pub fn build(self) -> CustomOpPayloadBuilder {
        CustomOpPayloadBuilder {
            builder_signer: self.builder_signer,
            #[cfg(feature = "flashblocks")]
            flashblocks_ws_url: self.flashblocks_ws_url.unwrap_or_default(),
            #[cfg(feature = "flashblocks")]
            chain_block_time: self.chain_block_time.unwrap_or_default(),
            #[cfg(feature = "flashblocks")]
            flashblock_block_time: self.flashblock_block_time.unwrap_or_default(),
            supervisor_url: self.supervisor_url,
            supervisor_safety_level: self.supervisor_safety_level,
        }
    }
}

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CustomOpPayloadBuilder {
    pub builder_signer: Option<Signer>,
    #[cfg(feature = "flashblocks")]
    pub flashblocks_ws_url: String,
    #[cfg(feature = "flashblocks")]
    pub chain_block_time: u64,
    #[cfg(feature = "flashblocks")]
    pub flashblock_block_time: u64,
    pub supervisor_url: Option<Url>,
    pub supervisor_safety_level: Option<String>,
}

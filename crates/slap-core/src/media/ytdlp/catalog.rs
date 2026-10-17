use url::Url;

use super::{YtDlpClient, dto};
use crate::media::{Import, MediaError, catalog::Catalog};

#[derive(Clone)]
pub struct YtDlpCatalog {
    client: YtDlpClient,
}

impl YtDlpCatalog {
    pub(super) fn new(client: YtDlpClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl Catalog for YtDlpCatalog {
    fn name(&self) -> &'static str {
        "yt-dlp"
    }

    /// yt-dlp supports far more sites than can be listed, so say yes to all and let ytdlp decide.
    fn handles(&self, _url: &Url) -> bool {
        true
    }

    async fn list(&self, url: &Url) -> Result<Import, MediaError> {
        let output = self.client.list(url).await?;
        // A single video comes back with its stream already attached.
        Ok(dto::parse_list(&output, url)?)
    }
}

use tracing::debug;
use url::Url;

use super::{Import, MediaError};

/// Something that can expand a link into a [`Import`].
#[async_trait::async_trait]
pub trait Catalog: Send + Sync {
    /// For logs.
    fn name(&self) -> &'static str;

    /// Whether this catalog understands links like `url`.
    fn handles(&self, url: &Url) -> bool;

    async fn list(&self, url: &Url) -> Result<Import, MediaError>;
}

/// Hands each link to the first catalog that understands it.
pub struct Importer {
    catalogs: Vec<Box<dyn Catalog>>,
}

impl Importer {
    /// Order matters: put specific catalogs before catch-all ones.
    pub fn new(catalogs: Vec<Box<dyn Catalog>>) -> Self {
        Self { catalogs }
    }

    pub async fn import(&self, url: &Url) -> Result<Import, MediaError> {
        let catalog = self
            .catalogs
            .iter()
            .find(|catalog| catalog.handles(url))
            .ok_or_else(|| MediaError::UnsupportedUrl(url.to_string()))?;

        debug!(
            catalog = catalog.name(),
            host = url.host_str(),
            "importing link"
        );
        catalog.list(url).await
    }
}

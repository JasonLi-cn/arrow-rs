// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! An object store implementation for Azure blob storage
//!
//! ## Streaming uploads
//!
//! [ObjectStore::put_multipart] will upload data in blocks and write a blob from those
//! blocks. Data is buffered internally to make blocks of at least 5MB and blocks
//! are uploaded concurrently.
//!
//! [ObjectStore::abort_multipart] is a no-op, since Azure Blob Store doesn't provide
//! a way to drop old blocks. Instead unused blocks are automatically cleaned up
//! after 7 days.
use crate::{
    multipart::{MultiPartStore, PartId, PutPart, WriteMultiPart},
    path::Path,
    signer::Signer,
    GetOptions, GetResult, ListResult, MultipartId, ObjectMeta, ObjectStore, PutOptions, PutResult,
    Result,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use reqwest::Method;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWrite;
use url::Url;

use crate::client::get::GetClientExt;
use crate::client::list::ListClientExt;
use crate::client::CredentialProvider;
pub use credential::{authority_hosts, AzureAccessKey, AzureAuthorizer};

mod builder;
mod client;
mod credential;

/// [`CredentialProvider`] for [`MicrosoftAzure`]
pub type AzureCredentialProvider = Arc<dyn CredentialProvider<Credential = AzureCredential>>;
pub use builder::{AzureConfigKey, MicrosoftAzureBuilder};
pub use credential::AzureCredential;

const STORE: &str = "MicrosoftAzure";

/// Interface for [Microsoft Azure Blob Storage](https://azure.microsoft.com/en-us/services/storage/blobs/).
#[derive(Debug)]
pub struct MicrosoftAzure {
    client: Arc<client::AzureClient>,
}

impl MicrosoftAzure {
    /// Returns the [`AzureCredentialProvider`] used by [`MicrosoftAzure`]
    pub fn credentials(&self) -> &AzureCredentialProvider {
        &self.client.config().credentials
    }

    /// Create a full URL to the resource specified by `path` with this instance's configuration.
    fn path_url(&self, path: &Path) -> url::Url {
        self.client.config().path_url(path)
    }
}

impl std::fmt::Display for MicrosoftAzure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MicrosoftAzure {{ account: {}, container: {} }}",
            self.client.config().account,
            self.client.config().container
        )
    }
}

#[async_trait]
impl ObjectStore for MicrosoftAzure {
    async fn put_opts(&self, location: &Path, bytes: Bytes, opts: PutOptions) -> Result<PutResult> {
        self.client.put_blob(location, bytes, opts).await
    }

    async fn put_multipart(
        &self,
        location: &Path,
    ) -> Result<(MultipartId, Box<dyn AsyncWrite + Unpin + Send>)> {
        let inner = AzureMultiPartUpload {
            client: Arc::clone(&self.client),
            location: location.to_owned(),
        };
        Ok((String::new(), Box::new(WriteMultiPart::new(inner, 8))))
    }

    async fn abort_multipart(&self, _location: &Path, _multipart_id: &MultipartId) -> Result<()> {
        // There is no way to drop blocks that have been uploaded. Instead, they simply
        // expire in 7 days.
        Ok(())
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        self.client.get_opts(location, options).await
    }

    async fn delete(&self, location: &Path) -> Result<()> {
        self.client.delete_request(location, &()).await
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'_, Result<ObjectMeta>> {
        self.client.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.client.list_with_delimiter(prefix).await
    }

    async fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        self.client.copy_request(from, to, true).await
    }

    async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> Result<()> {
        self.client.copy_request(from, to, false).await
    }
}

#[async_trait]
impl Signer for MicrosoftAzure {
    /// Create a URL containing the relevant [Service SAS] query parameters that authorize a request
    /// via `method` to the resource at `path` valid for the duration specified in `expires_in`.
    ///
    /// [Service SAS]: https://learn.microsoft.com/en-us/rest/api/storageservices/create-service-sas
    ///
    /// # Example
    ///
    /// This example returns a URL that will enable a user to upload a file to
    /// "some-folder/some-file.txt" in the next hour.
    ///
    /// ```
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # use object_store::{azure::MicrosoftAzureBuilder, path::Path, signer::Signer};
    /// # use reqwest::Method;
    /// # use std::time::Duration;
    /// #
    /// let azure = MicrosoftAzureBuilder::new()
    ///     .with_account("my-account")
    ///     .with_access_key("my-access-key")
    ///     .with_container_name("my-container")
    ///     .build()?;
    ///
    /// let url = azure.signed_url(
    ///     Method::PUT,
    ///     &Path::from("some-folder/some-file.txt"),
    ///     Duration::from_secs(60 * 60)
    /// ).await?;
    /// #     Ok(())
    /// # }
    /// ```
    async fn signed_url(&self, method: Method, path: &Path, expires_in: Duration) -> Result<Url> {
        let mut url = self.path_url(path);
        let signer = self.client.signer(expires_in).await?;
        signer.sign(&method, &mut url)?;
        Ok(url)
    }

    async fn signed_urls(
        &self,
        method: Method,
        paths: &[Path],
        expires_in: Duration,
    ) -> Result<Vec<Url>> {
        let mut urls = Vec::with_capacity(paths.len());
        let signer = self.client.signer(expires_in).await?;
        for path in paths {
            let mut url = self.path_url(path);
            signer.sign(&method, &mut url)?;
            urls.push(url);
        }
        Ok(urls)
    }
}

/// Relevant docs: <https://azure.github.io/Storage/docs/application-and-user-data/basics/azure-blob-storage-upload-apis/>
/// In Azure Blob Store, parts are "blocks"
/// put_multipart_part -> PUT block
/// complete -> PUT block list
/// abort -> No equivalent; blocks are simply dropped after 7 days
#[derive(Debug, Clone)]
struct AzureMultiPartUpload {
    client: Arc<client::AzureClient>,
    location: Path,
}

#[async_trait]
impl PutPart for AzureMultiPartUpload {
    async fn put_part(&self, buf: Vec<u8>, idx: usize) -> Result<PartId> {
        self.client.put_block(&self.location, idx, buf.into()).await
    }

    async fn complete(&self, parts: Vec<PartId>) -> Result<()> {
        self.client.put_block_list(&self.location, parts).await?;
        Ok(())
    }
}

#[async_trait]
impl MultiPartStore for MicrosoftAzure {
    async fn create_multipart(&self, _: &Path) -> Result<MultipartId> {
        Ok(String::new())
    }

    async fn put_part(
        &self,
        path: &Path,
        _: &MultipartId,
        part_idx: usize,
        data: Bytes,
    ) -> Result<PartId> {
        self.client.put_block(path, part_idx, data).await
    }

    async fn complete_multipart(
        &self,
        path: &Path,
        _: &MultipartId,
        parts: Vec<PartId>,
    ) -> Result<PutResult> {
        self.client.put_block_list(path, parts).await
    }

    async fn abort_multipart(&self, _: &Path, _: &MultipartId) -> Result<()> {
        // There is no way to drop blocks that have been uploaded. Instead, they simply
        // expire in 7 days.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;

    #[tokio::test]
    async fn azure_blob_test() {
        crate::test_util::maybe_skip_integration!();
        let integration = MicrosoftAzureBuilder::from_env().build().unwrap();

        put_get_delete_list_opts(&integration).await;
        get_opts(&integration).await;
        list_uses_directories_correctly(&integration).await;
        list_with_delimiter(&integration).await;
        rename_and_copy(&integration).await;
        copy_if_not_exists(&integration).await;
        stream_get(&integration).await;
        put_opts(&integration, true).await;
        multipart(&integration, &integration).await;
        signing(&integration).await;

        let validate = !integration.client.config().disable_tagging;
        tagging(&integration, validate, |p| {
            let client = Arc::clone(&integration.client);
            async move { client.get_blob_tagging(&p).await }
        })
        .await
    }

    #[ignore = "Used for manual testing against a real storage account."]
    #[tokio::test]
    async fn test_user_delegation_key() {
        let account = std::env::var("AZURE_ACCOUNT_NAME").unwrap();
        let container = std::env::var("AZURE_CONTAINER_NAME").unwrap();
        let client_id = std::env::var("AZURE_CLIENT_ID").unwrap();
        let client_secret = std::env::var("AZURE_CLIENT_SECRET").unwrap();
        let tenant_id = std::env::var("AZURE_TENANT_ID").unwrap();
        let integration = MicrosoftAzureBuilder::new()
            .with_account(account)
            .with_container_name(container)
            .with_client_id(client_id)
            .with_client_secret(client_secret)
            .with_tenant_id(&tenant_id)
            .build()
            .unwrap();

        let data = Bytes::from("hello world");
        let path = Path::from("file.txt");
        integration.put(&path, data.clone()).await.unwrap();

        let signed = integration
            .signed_url(Method::GET, &path, Duration::from_secs(60))
            .await
            .unwrap();

        let resp = reqwest::get(signed).await.unwrap();
        let loaded = resp.bytes().await.unwrap();

        assert_eq!(data, loaded);
    }

    #[test]
    fn azure_test_config_get_value() {
        let azure_client_id = "object_store:fake_access_key_id".to_string();
        let azure_storage_account_name = "object_store:fake_secret_key".to_string();
        let azure_storage_token = "object_store:fake_default_region".to_string();
        let builder = MicrosoftAzureBuilder::new()
            .with_config(AzureConfigKey::ClientId, &azure_client_id)
            .with_config(AzureConfigKey::AccountName, &azure_storage_account_name)
            .with_config(AzureConfigKey::Token, &azure_storage_token);

        assert_eq!(
            builder.get_config_value(&AzureConfigKey::ClientId).unwrap(),
            azure_client_id
        );
        assert_eq!(
            builder
                .get_config_value(&AzureConfigKey::AccountName)
                .unwrap(),
            azure_storage_account_name
        );
        assert_eq!(
            builder.get_config_value(&AzureConfigKey::Token).unwrap(),
            azure_storage_token
        );
    }
}

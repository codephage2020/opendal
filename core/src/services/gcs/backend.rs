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

use std::fmt::Debug;
use std::fmt::Formatter;
use std::sync::Arc;

use http::Response;
use http::StatusCode;
use log::debug;
use reqsign::GoogleCredentialLoader;
use reqsign::GoogleSigner;
use reqsign::GoogleTokenLoad;
use reqsign::GoogleTokenLoader;

use super::core::*;
use super::delete::GcsDeleter;
use super::error::parse_error;
use super::lister::GcsLister;
use super::writer::GcsWriter;
use super::writer::GcsWriters;
use super::DEFAULT_SCHEME;
use crate::raw::oio::BatchDeleter;
use crate::raw::*;
use crate::services::GcsConfig;
use crate::*;
const DEFAULT_GCS_ENDPOINT: &str = "https://storage.googleapis.com";
const DEFAULT_GCS_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

impl Configurator for GcsConfig {
    type Builder = GcsBuilder;

    #[allow(deprecated)]
    fn into_builder(self) -> Self::Builder {
        GcsBuilder {
            config: self,
            http_client: None,
            customized_token_loader: None,
        }
    }
}

/// [Google Cloud Storage](https://cloud.google.com/storage) services support.
#[doc = include_str!("docs.md")]
#[derive(Default)]
pub struct GcsBuilder {
    config: GcsConfig,

    #[deprecated(since = "0.53.0", note = "Use `Operator::update_http_client` instead")]
    http_client: Option<HttpClient>,
    customized_token_loader: Option<Box<dyn GoogleTokenLoad>>,
}

impl Debug for GcsBuilder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut ds = f.debug_struct("GcsBuilder");

        ds.field("config", &self.config);
        ds.finish_non_exhaustive()
    }
}

impl GcsBuilder {
    /// set the working directory root of backend
    pub fn root(mut self, root: &str) -> Self {
        self.config.root = if root.is_empty() {
            None
        } else {
            Some(root.to_string())
        };

        self
    }

    /// set the container's name
    pub fn bucket(mut self, bucket: &str) -> Self {
        self.config.bucket = bucket.to_string();
        self
    }

    /// set the GCS service scope
    ///
    /// If not set, we will use `https://www.googleapis.com/auth/devstorage.read_write`.
    ///
    /// # Valid scope examples
    ///
    /// - read-only: `https://www.googleapis.com/auth/devstorage.read_only`
    /// - read-write: `https://www.googleapis.com/auth/devstorage.read_write`
    /// - full-control: `https://www.googleapis.com/auth/devstorage.full_control`
    ///
    /// Reference: [Cloud Storage authentication](https://cloud.google.com/storage/docs/authentication)
    pub fn scope(mut self, scope: &str) -> Self {
        if !scope.is_empty() {
            self.config.scope = Some(scope.to_string())
        };
        self
    }

    /// Set the GCS service account.
    ///
    /// service account will be used for fetch token from vm metadata.
    /// If not set, we will try to fetch with `default` service account.
    pub fn service_account(mut self, service_account: &str) -> Self {
        if !service_account.is_empty() {
            self.config.service_account = Some(service_account.to_string())
        };
        self
    }

    /// set the endpoint GCS service uses
    pub fn endpoint(mut self, endpoint: &str) -> Self {
        if !endpoint.is_empty() {
            self.config.endpoint = Some(endpoint.to_string())
        };
        self
    }

    /// set the base64 hashed credentials string used for OAuth2 authentication.
    ///
    /// this method allows to specify the credentials directly as a base64 hashed string.
    /// alternatively, you can use `credential_path()` to provide the local path to a credentials file.
    /// we will use one of `credential` and `credential_path` to complete the OAuth2 authentication.
    ///
    /// Reference: [Google Cloud Storage Authentication](https://cloud.google.com/docs/authentication).
    pub fn credential(mut self, credential: &str) -> Self {
        if !credential.is_empty() {
            self.config.credential = Some(credential.to_string())
        };
        self
    }

    /// set the local path to credentials file which is used for OAuth2 authentication.
    ///
    /// credentials file contains the original credentials that have not been base64 hashed.
    /// we will use one of `credential` and `credential_path` to complete the OAuth2 authentication.
    ///
    /// Reference: [Google Cloud Storage Authentication](https://cloud.google.com/docs/authentication).
    pub fn credential_path(mut self, path: &str) -> Self {
        if !path.is_empty() {
            self.config.credential_path = Some(path.to_string())
        };
        self
    }

    /// Specify the http client that used by this service.
    ///
    /// # Notes
    ///
    /// This API is part of OpenDAL's Raw API. `HttpClient` could be changed
    /// during minor updates.
    #[deprecated(since = "0.53.0", note = "Use `Operator::update_http_client` instead")]
    #[allow(deprecated)]
    pub fn http_client(mut self, client: HttpClient) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Specify the customized token loader used by this service.
    pub fn customized_token_loader(mut self, token_load: Box<dyn GoogleTokenLoad>) -> Self {
        self.customized_token_loader = Some(token_load);
        self
    }

    /// Provide the OAuth2 token to use.
    pub fn token(mut self, token: String) -> Self {
        self.config.token = Some(token);
        self
    }

    /// Disable attempting to load credentials from the GCE metadata server.
    pub fn disable_vm_metadata(mut self) -> Self {
        self.config.disable_vm_metadata = true;
        self
    }

    /// Disable loading configuration from the environment.
    pub fn disable_config_load(mut self) -> Self {
        self.config.disable_config_load = true;
        self
    }

    /// Set the predefined acl for GCS.
    ///
    /// Available values are:
    /// - `authenticatedRead`
    /// - `bucketOwnerFullControl`
    /// - `bucketOwnerRead`
    /// - `private`
    /// - `projectPrivate`
    /// - `publicRead`
    pub fn predefined_acl(mut self, acl: &str) -> Self {
        if !acl.is_empty() {
            self.config.predefined_acl = Some(acl.to_string())
        };
        self
    }

    /// Set the default storage class for GCS.
    ///
    /// Available values are:
    /// - `STANDARD`
    /// - `NEARLINE`
    /// - `COLDLINE`
    /// - `ARCHIVE`
    pub fn default_storage_class(mut self, class: &str) -> Self {
        if !class.is_empty() {
            self.config.default_storage_class = Some(class.to_string())
        };
        self
    }

    /// Allow anonymous requests.
    ///
    /// This is typically used for buckets which are open to the public or GCS
    /// storage emulators.
    pub fn allow_anonymous(mut self) -> Self {
        self.config.allow_anonymous = true;
        self
    }
}

impl Builder for GcsBuilder {
    type Config = GcsConfig;

    fn build(self) -> Result<impl Access> {
        debug!("backend build started: {self:?}");

        let root = normalize_root(&self.config.root.unwrap_or_default());
        debug!("backend use root {root}");

        // Handle endpoint and bucket name
        let bucket = match self.config.bucket.is_empty() {
            false => Ok(&self.config.bucket),
            true => Err(
                Error::new(ErrorKind::ConfigInvalid, "The bucket is misconfigured")
                    .with_operation("Builder::build")
                    .with_context("service", Scheme::Gcs),
            ),
        }?;

        // TODO: server side encryption

        let endpoint = self
            .config
            .endpoint
            .clone()
            .unwrap_or_else(|| DEFAULT_GCS_ENDPOINT.to_string());
        debug!("backend use endpoint: {endpoint}");

        let mut cred_loader = GoogleCredentialLoader::default();
        if let Some(cred) = &self.config.credential {
            cred_loader = cred_loader.with_content(cred);
        }
        if let Some(cred) = &self.config.credential_path {
            cred_loader = cred_loader.with_path(cred);
        }
        #[cfg(target_arch = "wasm32")]
        {
            cred_loader = cred_loader.with_disable_env();
            cred_loader = cred_loader.with_disable_well_known_location();
        }

        if self.config.disable_config_load {
            cred_loader = cred_loader
                .with_disable_env()
                .with_disable_well_known_location();
        }

        let scope = if let Some(scope) = &self.config.scope {
            scope
        } else {
            DEFAULT_GCS_SCOPE
        };

        let mut token_loader = GoogleTokenLoader::new(scope, GLOBAL_REQWEST_CLIENT.clone());
        if let Some(account) = &self.config.service_account {
            token_loader = token_loader.with_service_account(account);
        }
        if let Ok(Some(cred)) = cred_loader.load() {
            token_loader = token_loader.with_credentials(cred)
        }
        if let Some(loader) = self.customized_token_loader {
            token_loader = token_loader.with_customized_token_loader(loader)
        }

        if self.config.disable_vm_metadata {
            token_loader = token_loader.with_disable_vm_metadata(true);
        }

        let signer = GoogleSigner::new("storage");

        let backend = GcsBackend {
            core: Arc::new(GcsCore {
                info: {
                    let am = AccessorInfo::default();
                    am.set_scheme(DEFAULT_SCHEME)
                        .set_root(&root)
                        .set_name(bucket)
                        .set_native_capability(Capability {
                            stat: true,
                            stat_with_if_match: true,
                            stat_with_if_none_match: true,

                            read: true,

                            read_with_if_match: true,
                            read_with_if_none_match: true,

                            write: true,
                            write_can_empty: true,
                            write_can_multi: true,
                            write_with_cache_control: true,
                            write_with_content_type: true,
                            write_with_content_encoding: true,
                            write_with_user_metadata: true,
                            write_with_if_not_exists: true,

                            // The min multipart size of Gcs is 5 MiB.
                            //
                            // ref: <https://cloud.google.com/storage/docs/xml-api/put-object-multipart>
                            write_multi_min_size: Some(5 * 1024 * 1024),
                            // The max multipart size of Gcs is 5 GiB.
                            //
                            // ref: <https://cloud.google.com/storage/docs/xml-api/put-object-multipart>
                            write_multi_max_size: if cfg!(target_pointer_width = "64") {
                                Some(5 * 1024 * 1024 * 1024)
                            } else {
                                Some(usize::MAX)
                            },

                            delete: true,
                            delete_max_size: Some(100),
                            copy: true,

                            list: true,
                            list_with_limit: true,
                            list_with_start_after: true,
                            list_with_recursive: true,

                            presign: true,
                            presign_stat: true,
                            presign_read: true,
                            presign_write: true,

                            shared: true,

                            ..Default::default()
                        });

                    // allow deprecated api here for compatibility
                    #[allow(deprecated)]
                    if let Some(client) = self.http_client {
                        am.update_http_client(|_| client);
                    }

                    am.into()
                },
                endpoint,
                bucket: bucket.to_string(),
                root,
                signer,
                token_loader,
                token: self.config.token,
                scope: scope.to_string(),
                credential_loader: cred_loader,
                predefined_acl: self.config.predefined_acl.clone(),
                default_storage_class: self.config.default_storage_class.clone(),
                allow_anonymous: self.config.allow_anonymous,
            }),
        };

        Ok(backend)
    }
}

/// GCS storage backend
#[derive(Clone, Debug)]
pub struct GcsBackend {
    core: Arc<GcsCore>,
}

impl Access for GcsBackend {
    type Reader = HttpBody;
    type Writer = GcsWriters;
    type Lister = oio::PageLister<GcsLister>;
    type Deleter = oio::BatchDeleter<GcsDeleter>;

    fn info(&self) -> Arc<AccessorInfo> {
        self.core.info.clone()
    }

    async fn stat(&self, path: &str, args: OpStat) -> Result<RpStat> {
        let resp = self.core.gcs_get_object_metadata(path, &args).await?;

        if !resp.status().is_success() {
            return Err(parse_error(resp));
        }

        let slc = resp.into_body();
        let m = GcsCore::build_metadata_from_object_response(path, slc)?;

        Ok(RpStat::new(m))
    }

    async fn read(&self, path: &str, args: OpRead) -> Result<(RpRead, Self::Reader)> {
        let resp = self.core.gcs_get_object(path, args.range(), &args).await?;

        let status = resp.status();

        match status {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                Ok((RpRead::default(), resp.into_body()))
            }
            _ => {
                let (part, mut body) = resp.into_parts();
                let buf = body.to_buffer().await?;
                Err(parse_error(Response::from_parts(part, buf)))
            }
        }
    }

    async fn write(&self, path: &str, args: OpWrite) -> Result<(RpWrite, Self::Writer)> {
        let concurrent = args.concurrent();
        let w = GcsWriter::new(self.core.clone(), path, args);
        let w = oio::MultipartWriter::new(self.core.info.clone(), w, concurrent);

        Ok((RpWrite::default(), w))
    }

    async fn delete(&self) -> Result<(RpDelete, Self::Deleter)> {
        Ok((
            RpDelete::default(),
            BatchDeleter::new(GcsDeleter::new(self.core.clone())),
        ))
    }

    async fn list(&self, path: &str, args: OpList) -> Result<(RpList, Self::Lister)> {
        let l = GcsLister::new(
            self.core.clone(),
            path,
            args.recursive(),
            args.limit(),
            args.start_after(),
        );

        Ok((RpList::default(), oio::PageLister::new(l)))
    }

    async fn copy(&self, from: &str, to: &str, _: OpCopy) -> Result<RpCopy> {
        let resp = self.core.gcs_copy_object(from, to).await?;

        if resp.status().is_success() {
            Ok(RpCopy::default())
        } else {
            Err(parse_error(resp))
        }
    }

    async fn presign(&self, path: &str, args: OpPresign) -> Result<RpPresign> {
        // We will not send this request out, just for signing.
        let req = match args.operation() {
            PresignOperation::Stat(v) => self.core.gcs_head_object_xml_request(path, v),
            PresignOperation::Read(v) => self.core.gcs_get_object_xml_request(path, v),
            PresignOperation::Write(v) => {
                self.core
                    .gcs_insert_object_xml_request(path, v, Buffer::new())
            }
            PresignOperation::Delete(_) => Err(Error::new(
                ErrorKind::Unsupported,
                "operation is not supported",
            )),
        };
        let mut req = req?;
        self.core.sign_query(&mut req, args.expire())?;

        // We don't need this request anymore, consume it directly.
        let (parts, _) = req.into_parts();

        Ok(RpPresign::new(PresignedRequest::new(
            parts.method,
            parts.uri,
            parts.headers,
        )))
    }
}

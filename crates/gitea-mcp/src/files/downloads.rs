//! Short-lived download grants issued only by the payload's authenticated owner.

use std::sync::Weak;

use super::*;
use crate::resources::{ResourceStore, StoredResource};

pub const AUTHORIZE_DOWNLOAD: &str = "files/authorizeDownload";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeDownloadParams {
    pub uri: String,
}

pub(super) struct DownloadTicket {
    credential_hash: [u8; 32],
    store: Weak<ResourceStore>,
    uri: String,
    pub(super) expires_at: Instant,
}

#[derive(Serialize)]
pub struct AuthorizeDownloadResult {
    pub file: FileValue,
    pub download: TransferDescriptor,
    pub sensitive: bool,
}

impl FilePlane {
    /// Authorize a helper to download one result owned by this identity.
    ///
    /// # Errors
    ///
    /// Refuses missing results, oversized metadata, or exhausted ticket capacity.
    pub fn authorize_download(
        &self,
        store: &Arc<ResourceStore>,
        params: AuthorizeDownloadParams,
    ) -> Result<AuthorizeDownloadResult, FileError> {
        if params.uri.len() > 512 {
            return Err(FileError::InvalidMetadata);
        }
        self.sweep();
        let stored = store.read(&params.uri).ok_or(FileError::MissingResult)?;
        let value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha256(&stored.body));
        let id = random_token()?;
        let credential = random_token()?;
        let mut tickets = lock(&self.downloads);
        if tickets.len() >= MAX_OUTSTANDING {
            return Err(FileError::TooManyOutstanding);
        }
        tickets.insert(
            id.clone(),
            DownloadTicket {
                credential_hash: sha256(credential.as_bytes()),
                store: Arc::downgrade(store),
                uri: params.uri.clone(),
                expires_at: Instant::now() + self.ttl,
            },
        );
        Ok(AuthorizeDownloadResult {
            file: FileValue {
                uri: params.uri,
                name: None,
                mime_type: Some(stored.content_type),
                size: Some(stored.body.len() as u64),
                digest: Some(FileDigest {
                    algorithm: "sha-256".into(),
                    value,
                }),
            },
            download: TransferDescriptor {
                transport: "http",
                method: "GET",
                url: format!("{}/files/download/{id}", self.public_origin),
                headers: HashMap::from([(TRANSFER_CREDENTIAL_HEADER.to_owned(), credential)]),
            },
            sensitive: stored.sensitive,
        })
    }

    /// Read an authorized, still-live payload without copying its body.
    ///
    /// # Errors
    ///
    /// Refuses missing or expired grants, invalid credentials, and expired payloads.
    pub fn read_download(&self, id: &str, credential: &str) -> Result<StoredResource, FileError> {
        if id.len() != ENCODED_TOKEN_CHARACTERS || credential.len() != ENCODED_TOKEN_CHARACTERS {
            return Err(FileError::Unauthorized);
        }
        self.sweep();
        let tickets = lock(&self.downloads);
        let ticket = tickets.get(id).ok_or(FileError::Unauthorized)?;
        let presented = sha256(credential.as_bytes());
        let valid: bool =
            subtle::ConstantTimeEq::ct_eq(&presented[..], &ticket.credential_hash[..]).into();
        if !valid {
            return Err(FileError::Unauthorized);
        }
        ticket
            .store
            .upgrade()
            .and_then(|store| store.read(&ticket.uri))
            .ok_or(FileError::MissingResult)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::ResourceLimits;

    fn store() -> Arc<ResourceStore> {
        ResourceStore::new(ResourceLimits {
            max_object_bytes: 16 * 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
            time_to_live: Duration::from_mins(1),
        })
    }

    #[test]
    fn only_the_owning_store_can_authorize_a_complete_sensitive_download() {
        let owner = store();
        let other = store();
        let resource = owner
            .insert("logs", "text/plain", true, vec![b'x'; 10 * 1024 * 1024])
            .unwrap();
        let plane = FilePlane::new("https://example.test", Duration::from_secs(30)).unwrap();
        assert!(matches!(
            plane.authorize_download(
                &other,
                AuthorizeDownloadParams {
                    uri: resource.uri.clone()
                }
            ),
            Err(FileError::MissingResult)
        ));
        let grant = plane
            .authorize_download(
                &owner,
                AuthorizeDownloadParams {
                    uri: resource.uri.clone(),
                },
            )
            .unwrap();
        let id = grant.download.url.rsplit('/').next().unwrap();
        let credential = &grant.download.headers[TRANSFER_CREDENTIAL_HEADER];
        assert!(matches!(
            plane.read_download(id, &"x".repeat(43)),
            Err(FileError::Unauthorized)
        ));
        let downloaded = plane.read_download(id, credential).unwrap();
        assert_eq!(downloaded.body.as_ptr(), resource.body.as_ptr());
        assert!(downloaded.sensitive);
        assert_eq!(grant.file.size, Some(resource.body.len() as u64));
        assert_eq!(
            grant.file.digest.unwrap().value,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha256(&downloaded.body))
        );
    }

    #[test]
    fn grants_do_not_extend_result_or_store_lifetimes() {
        for expire_resource in [false, true] {
            let owner = store();
            let resource = owner
                .insert("logs", "text/plain", false, b"fixture".to_vec())
                .unwrap();
            let plane = FilePlane::new("https://example.test", Duration::from_secs(30)).unwrap();
            let grant = plane
                .authorize_download(
                    &owner,
                    AuthorizeDownloadParams {
                        uri: resource.uri.clone(),
                    },
                )
                .unwrap();
            let id = grant.download.url.rsplit('/').next().unwrap();
            if expire_resource {
                assert!(
                    owner
                        .read_at(Instant::now() + Duration::from_secs(61), &resource.uri)
                        .is_none()
                );
            }
            drop(owner);
            let error = plane
                .read_download(id, &grant.download.headers[TRANSFER_CREDENTIAL_HEADER])
                .unwrap_err();
            assert!(matches!(error, FileError::MissingResult));
            assert!(error.to_string().contains("do not repeat"));
        }
    }

    #[test]
    fn grant_expiry_and_outstanding_capacity_are_enforced() {
        let owner = store();
        let resource = owner
            .insert("logs", "text/plain", false, b"fixture".to_vec())
            .unwrap();
        let plane = FilePlane::new("https://example.test", Duration::from_secs(30)).unwrap();
        let mut last = None;
        for _ in 0..MAX_OUTSTANDING {
            last = Some(
                plane
                    .authorize_download(
                        &owner,
                        AuthorizeDownloadParams {
                            uri: resource.uri.clone(),
                        },
                    )
                    .unwrap(),
            );
        }
        assert!(matches!(
            plane.authorize_download(
                &owner,
                AuthorizeDownloadParams {
                    uri: resource.uri.clone()
                }
            ),
            Err(FileError::TooManyOutstanding)
        ));
        let grant = last.unwrap();
        let id = grant.download.url.rsplit('/').next().unwrap();
        lock(&plane.downloads).get_mut(id).unwrap().expires_at =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            plane.read_download(id, &grant.download.headers[TRANSFER_CREDENTIAL_HEADER]),
            Err(FileError::Unauthorized)
        ));
        assert!(
            plane
                .authorize_download(&owner, AuthorizeDownloadParams { uri: resource.uri })
                .is_ok()
        );
    }
}

//! Bounded, single-use uploads for secret-bearing workflow inputs.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

mod downloads;
pub use downloads::{AUTHORIZE_DOWNLOAD, AuthorizeDownloadParams, AuthorizeDownloadResult};

pub const AUTHORIZE_UPLOAD: &str = "files/authorizeUpload";
pub const STAGED_PREFIX: &str = "gitea-secret://staged/";
pub const TRANSFER_CREDENTIAL_HEADER: &str = "Gitea-Transfer-Credential";
pub const MAX_SECRET_BYTES: usize = 64 * 1024;
const MAX_OUTSTANDING: usize = 16;
const TOKEN_BYTES: usize = 32;
const MAX_METADATA_CHARACTERS: usize = 255;
const ENCODED_TOKEN_CHARACTERS: usize = 43;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileDigest {
    pub algorithm: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizeUploadParams {
    pub name: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    pub digest: Option<FileDigest>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileValue {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<FileDigest>,
}

#[derive(Debug, Serialize)]
pub struct TransferDescriptor {
    pub transport: &'static str,
    pub method: &'static str,
    pub url: String,
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct AuthorizeUploadResult {
    pub file: FileValue,
    pub upload: TransferDescriptor,
}

struct Ticket {
    credential_hash: [u8; 32],
    staged_id: String,
    size: Option<u64>,
    ceiling: usize,
    digest: Option<[u8; 32]>,
    expires_at: Instant,
}

struct Staged {
    bytes: Vec<u8>,
    expires_at: Instant,
}

impl Drop for Staged {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[derive(Debug, Error)]
pub enum FileError {
    #[error("no file transfer authorization is available")]
    Unauthorized,
    #[error("the file transfer window has closed")]
    Expired,
    #[error("too many file transfers are outstanding")]
    TooManyOutstanding,
    #[error("the declared secret exceeds the upload limit")]
    TooLarge,
    #[error("the uploaded size does not match the declaration")]
    SizeMismatch,
    #[error("the uploaded digest does not match the declaration")]
    DigestMismatch,
    #[error("only SHA-256 file digests are supported")]
    UnsupportedDigest,
    #[error("file metadata exceeds its limit")]
    InvalidMetadata,
    #[error("secure randomness is unavailable")]
    EntropyUnavailable,
    #[error("the staged secret is unavailable or already consumed")]
    Unavailable,
    #[error("the staged secret must be non-empty UTF-8 text")]
    InvalidSecret,
    #[error(
        "the retained payload is unavailable; do not repeat the original operation to recover it"
    )]
    MissingResult,
}

impl FileError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized | Self::Expired => "gitea_file_unauthorized",
            Self::TooManyOutstanding => "gitea_file_too_many_outstanding",
            Self::TooLarge => "gitea_file_too_large",
            Self::SizeMismatch => "gitea_file_size_mismatch",
            Self::DigestMismatch => "gitea_file_digest_mismatch",
            Self::UnsupportedDigest => "gitea_file_unsupported_digest",
            Self::InvalidMetadata => "gitea_file_invalid_metadata",
            Self::EntropyUnavailable => "gitea_file_entropy_unavailable",
            Self::Unavailable => "gitea_file_unavailable",
            Self::InvalidSecret => "gitea_file_invalid_secret",
            Self::MissingResult => "gitea_result_unavailable",
        }
    }
}

pub struct FilePlane {
    public_origin: String,
    ttl: Duration,
    tickets: Mutex<HashMap<String, Ticket>>,
    staged: Mutex<HashMap<String, Staged>>,
    downloads: Mutex<HashMap<String, downloads::DownloadTicket>>,
}

impl FilePlane {
    /// Create a process-local, memory-only upload plane.
    ///
    /// # Errors
    ///
    /// Returns an error unless `public_origin` is a bare HTTP(S) origin.
    pub fn new(public_origin: &str, ttl: Duration) -> Result<Arc<Self>, String> {
        let plane = Arc::new(Self {
            public_origin: validate_public_origin(public_origin)?,
            ttl,
            tickets: Mutex::new(HashMap::new()),
            staged: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&plane);
        thread::Builder::new()
            .name("gitea-file-expiry".to_owned())
            .spawn(move || {
                loop {
                    let Some(plane) = weak.upgrade() else {
                        break;
                    };
                    let wait = plane.sweep_and_next_wait();
                    drop(plane);
                    thread::sleep(wait);
                }
            })
            .map_err(|error| format!("failed to start file expiry worker: {error}"))?;
        Ok(plane)
    }

    /// Authorize one bounded upload.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid integrity metadata or exhausted capacity.
    pub fn authorize_upload(
        &self,
        params: AuthorizeUploadParams,
    ) -> Result<AuthorizeUploadResult, FileError> {
        self.sweep();
        if params
            .name
            .as_ref()
            .is_some_and(|value| value.chars().count() > MAX_METADATA_CHARACTERS)
            || params
                .mime_type
                .as_ref()
                .is_some_and(|value| value.chars().count() > MAX_METADATA_CHARACTERS)
        {
            return Err(FileError::InvalidMetadata);
        }
        let declared_size = params
            .size
            .map(usize::try_from)
            .transpose()
            .map_err(|_| FileError::TooLarge)?;
        if declared_size.is_some_and(|size| size > MAX_SECRET_BYTES) {
            return Err(FileError::TooLarge);
        }
        let digest = params.digest.as_ref().map(decode_digest).transpose()?;
        let mut tickets = lock(&self.tickets);
        let staged = lock(&self.staged);
        if tickets.len() + staged.len() >= MAX_OUTSTANDING {
            return Err(FileError::TooManyOutstanding);
        }
        let upload_id = random_token()?;
        let staged_id = random_token()?;
        let credential = random_token()?;
        tickets.insert(
            upload_id.clone(),
            Ticket {
                credential_hash: sha256(credential.as_bytes()),
                staged_id: staged_id.clone(),
                size: params.size,
                ceiling: declared_size.unwrap_or(MAX_SECRET_BYTES),
                digest,
                expires_at: Instant::now() + self.ttl,
            },
        );
        drop(staged);
        drop(tickets);

        Ok(AuthorizeUploadResult {
            file: FileValue {
                uri: format!("{STAGED_PREFIX}{staged_id}"),
                name: params.name,
                mime_type: params.mime_type,
                size: params.size,
                digest: params.digest,
            },
            upload: TransferDescriptor {
                transport: if self.public_origin.starts_with("https://") {
                    "https"
                } else {
                    "http"
                },
                method: "PUT",
                url: format!("{}/files/upload/{upload_id}", self.public_origin),
                headers: HashMap::from([(TRANSFER_CREDENTIAL_HEADER.to_owned(), credential)]),
            },
        })
    }

    /// Consume one upload ticket and stage its verified bytes in memory.
    ///
    /// # Errors
    ///
    /// Returns a bounded refusal for invalid authority, size, or digest.
    pub fn receive(&self, id: &str, credential: &str, bytes: &[u8]) -> Result<(), FileError> {
        let mut tickets = lock(&self.tickets);
        let ticket = tickets.get(id).ok_or(FileError::Unauthorized)?;
        let presented = sha256(credential.as_bytes());
        let matches: bool =
            subtle::ConstantTimeEq::ct_eq(&presented[..], &ticket.credential_hash[..]).into();
        if !matches {
            return Err(FileError::Unauthorized);
        }
        if Instant::now() > ticket.expires_at {
            tickets.remove(id);
            return Err(FileError::Expired);
        }
        let ticket = tickets.remove(id).ok_or(FileError::Unauthorized)?;
        if bytes.len() > ticket.ceiling || bytes.len() > MAX_SECRET_BYTES {
            return Err(FileError::TooLarge);
        }
        if ticket.size.is_some_and(|size| size != bytes.len() as u64) {
            return Err(FileError::SizeMismatch);
        }
        if ticket.digest.is_some_and(|digest| digest != sha256(bytes)) {
            return Err(FileError::DigestMismatch);
        }
        lock(&self.staged).insert(
            ticket.staged_id,
            Staged {
                bytes: bytes.to_vec(),
                expires_at: Instant::now() + self.ttl,
            },
        );
        Ok(())
    }

    /// Verify an upload credential without reading a request body or consuming
    /// the ticket. `receive` repeats this check atomically before consuming it.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown, mismatched, or expired ticket.
    pub fn verify_upload(&self, id: &str, credential: &str) -> Result<(), FileError> {
        let mut tickets = lock(&self.tickets);
        let ticket = tickets.get(id).ok_or(FileError::Unauthorized)?;
        let presented = sha256(credential.as_bytes());
        let matches: bool =
            subtle::ConstantTimeEq::ct_eq(&presented[..], &ticket.credential_hash[..]).into();
        if !matches {
            return Err(FileError::Unauthorized);
        }
        if Instant::now() > ticket.expires_at {
            tickets.remove(id);
            return Err(FileError::Expired);
        }
        Ok(())
    }

    /// Take a staged secret exactly once.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown, expired, empty, or non-UTF-8 value.
    pub fn take_secret(&self, uri: &str) -> Result<Zeroizing<String>, FileError> {
        let id = uri
            .strip_prefix(STAGED_PREFIX)
            .ok_or(FileError::Unavailable)?;
        if id.len() != ENCODED_TOKEN_CHARACTERS
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(FileError::Unavailable);
        }
        let mut staged = lock(&self.staged)
            .remove(id)
            .ok_or(FileError::Unavailable)?;
        if Instant::now() > staged.expires_at {
            return Err(FileError::Unavailable);
        }
        let bytes = std::mem::take(&mut staged.bytes);
        let value = match String::from_utf8(bytes) {
            Ok(value) => Zeroizing::new(value),
            Err(error) => {
                let mut bytes = error.into_bytes();
                bytes.zeroize();
                return Err(FileError::InvalidSecret);
            }
        };
        if value.is_empty() {
            return Err(FileError::InvalidSecret);
        }
        Ok(value)
    }

    fn sweep(&self) {
        let now = Instant::now();
        lock(&self.tickets).retain(|_, ticket| now <= ticket.expires_at);
        lock(&self.staged).retain(|_, staged| now <= staged.expires_at);
        lock(&self.downloads).retain(|_, ticket| now <= ticket.expires_at);
    }

    fn sweep_and_next_wait(&self) -> Duration {
        self.sweep();
        let now = Instant::now();
        let next_ticket = lock(&self.tickets)
            .values()
            .map(|ticket| ticket.expires_at)
            .min();
        let next_staged = lock(&self.staged)
            .values()
            .map(|staged| staged.expires_at)
            .min();
        let next_download = lock(&self.downloads)
            .values()
            .map(|ticket| ticket.expires_at)
            .min();
        next_ticket
            .into_iter()
            .chain(next_staged)
            .chain(next_download)
            .min()
            .map_or(self.ttl, |expires_at| {
                expires_at.saturating_duration_since(now)
            })
            .max(Duration::from_millis(1))
    }
}

/// Normalize and validate the origin advertised in upload descriptors.
///
/// # Errors
///
/// Returns an error unless `origin` is a bare HTTP(S) origin without userinfo.
pub fn validate_public_origin(origin: &str) -> Result<String, String> {
    let parsed = url::Url::parse(origin).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none_or(str::is_empty)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("must be a bare HTTP(S) origin without userinfo".to_owned());
    }
    Ok(parsed.origin().ascii_serialization())
}

fn random_token() -> Result<String, FileError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| FileError::EntropyUnavailable)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn decode_digest(digest: &FileDigest) -> Result<[u8; 32], FileError> {
    if !digest.algorithm.eq_ignore_ascii_case("sha-256") {
        return Err(FileError::UnsupportedDigest);
    }
    if digest.value.len() != ENCODED_TOKEN_CHARACTERS {
        return Err(FileError::DigestMismatch);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&digest.value)
        .map_err(|_| FileError::DigestMismatch)?
        .try_into()
        .map_err(|_| FileError::DigestMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_is_integrity_checked_and_consumed_once() {
        let plane = FilePlane::new("http://gitea:8000", Duration::from_mins(1)).unwrap();
        let value = b"one-time-secret";
        let authorized = plane
            .authorize_upload(AuthorizeUploadParams {
                size: Some(value.len() as u64),
                digest: Some(FileDigest {
                    algorithm: "sha-256".to_owned(),
                    value: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha256(value)),
                }),
                ..AuthorizeUploadParams::default()
            })
            .unwrap();
        let id = authorized.upload.url.rsplit('/').next().unwrap();
        let credential = &authorized.upload.headers[TRANSFER_CREDENTIAL_HEADER];
        plane.receive(id, credential, value).unwrap();
        let secret = plane.take_secret(&authorized.file.uri).unwrap();
        assert_eq!(secret.as_str(), "one-time-secret");
        assert!(matches!(
            plane.take_secret(&authorized.file.uri),
            Err(FileError::Unavailable)
        ));
    }

    #[test]
    fn wrong_credential_does_not_burn_the_ticket() {
        let plane = FilePlane::new("https://gitea.example", Duration::from_mins(1)).unwrap();
        let authorized = plane
            .authorize_upload(AuthorizeUploadParams::default())
            .unwrap();
        let id = authorized.upload.url.rsplit('/').next().unwrap();
        assert!(matches!(
            plane.receive(id, "wrong", b"secret"),
            Err(FileError::Unauthorized)
        ));
        let credential = &authorized.upload.headers[TRANSFER_CREDENTIAL_HEADER];
        plane.receive(id, credential, b"secret").unwrap();
    }

    #[test]
    fn invalid_integrity_metadata_is_bounded_before_decoding() {
        let plane = FilePlane::new("https://gitea.example", Duration::from_mins(1)).unwrap();
        let error = plane
            .authorize_upload(AuthorizeUploadParams {
                digest: Some(FileDigest {
                    algorithm: "sha-256".to_owned(),
                    value: "x".repeat(MAX_SECRET_BYTES),
                }),
                ..AuthorizeUploadParams::default()
            })
            .expect_err("an oversized digest is not decoded");
        assert!(matches!(error, FileError::DigestMismatch));
    }

    #[test]
    fn failed_integrity_check_does_not_stage_bytes() {
        let plane = FilePlane::new("https://gitea.example", Duration::from_mins(1)).unwrap();
        let authorized = plane
            .authorize_upload(AuthorizeUploadParams {
                size: Some(6),
                ..AuthorizeUploadParams::default()
            })
            .unwrap();
        let id = authorized.upload.url.rsplit('/').next().unwrap();
        let credential = &authorized.upload.headers[TRANSFER_CREDENTIAL_HEADER];
        assert!(matches!(
            plane.receive(id, credential, b"wrong"),
            Err(FileError::SizeMismatch)
        ));
        assert!(matches!(
            plane.take_secret(&authorized.file.uri),
            Err(FileError::Unavailable)
        ));
    }

    #[test]
    fn upload_metadata_and_outstanding_state_are_bounded() {
        let plane = FilePlane::new("https://gitea.example", Duration::from_mins(1)).unwrap();
        assert!(matches!(
            plane.authorize_upload(AuthorizeUploadParams {
                name: Some("x".repeat(MAX_METADATA_CHARACTERS + 1)),
                ..AuthorizeUploadParams::default()
            }),
            Err(FileError::InvalidMetadata)
        ));
        for _ in 0..MAX_OUTSTANDING {
            plane
                .authorize_upload(AuthorizeUploadParams::default())
                .expect("capacity remains");
        }
        assert!(matches!(
            plane.authorize_upload(AuthorizeUploadParams::default()),
            Err(FileError::TooManyOutstanding)
        ));
    }

    #[test]
    fn expiry_worker_evicts_staged_secret_without_later_file_activity() {
        let plane = FilePlane::new("https://gitea.example", Duration::from_millis(5)).unwrap();
        let authorized = plane
            .authorize_upload(AuthorizeUploadParams::default())
            .unwrap();
        let id = authorized.upload.url.rsplit('/').next().unwrap();
        let credential = &authorized.upload.headers[TRANSFER_CREDENTIAL_HEADER];
        plane
            .receive(id, credential, b"short-lived-secret")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while !lock(&plane.staged).is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }

        assert!(lock(&plane.staged).is_empty());
    }

    #[test]
    fn public_origin_refuses_paths_and_credentials() {
        assert_eq!(
            validate_public_origin("http://gitea:8000/").unwrap(),
            "http://gitea:8000"
        );
        assert_eq!(
            validate_public_origin("http://[::1]:8000/").unwrap(),
            "http://[::1]:8000"
        );
        for invalid in [
            "file:///tmp/upload",
            "https://gitea.example/path",
            "https://user@gitea.example",
        ] {
            assert!(
                validate_public_origin(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }
}

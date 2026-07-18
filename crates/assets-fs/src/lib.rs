//! Content-addressed storage for passive, first-party image assets.
//!
//! The caller supplies bytes, a display filename, and an optional claimed media
//! type. The store trusts none of them: it detects an allowlisted image format
//! from magic bytes, hashes the bytes, derives the storage path from the exact
//! lowercase SHA-256 digest, and installs both the blob and metadata without
//! overwriting an existing file.

#![forbid(unsafe_code)]

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_ASSET_BYTES: usize = 10 * 1024 * 1024;
pub const ASSET_RECORD_VERSION: &str = "1.0";

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Durable metadata stored beside a content-addressed asset.
///
/// `digest` is exactly 64 lowercase hexadecimal SHA-256 characters. The blob
/// path is `sha256/<first two digest characters>/<digest>` relative to the
/// store root. `original_filename` is a display-only, sanitized basename and
/// is never used to construct a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssetRecord {
    pub schema_version: String,
    pub digest: String,
    pub media_type: String,
    pub size: u64,
    pub original_filename: String,
    pub created_at: DateTime<Utc>,
}

/// An integrity-checked asset returned by [`AssetStore::get`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAsset {
    pub record: AssetRecord,
    pub bytes: Vec<u8>,
}

/// A filesystem-backed, content-addressed asset store.
///
/// There is intentionally no deletion API. Lifecycle and garbage collection
/// require a separate, explicitly authorized design.
#[derive(Debug, Clone)]
pub struct AssetStore {
    root: PathBuf,
}

impl AssetStore {
    /// Opens a store and creates its `sha256` namespace if necessary.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, AssetError> {
        let root = root.into();
        fs::create_dir_all(root.join("sha256"))?;
        Ok(Self { root })
    }

    /// Stores one verified image and returns the winning immutable metadata.
    ///
    /// Re-uploading identical bytes is a deduplicating operation. The first
    /// successfully installed sidecar remains authoritative, including its
    /// sanitized filename and timestamp. Existing blobs or sidecars are never
    /// overwritten. A corrupt existing object causes an integrity error.
    pub fn put(
        &self,
        bytes: &[u8],
        original_filename: &str,
        claimed_media_type: Option<&str>,
    ) -> Result<AssetRecord, AssetError> {
        if bytes.len() > MAX_ASSET_BYTES {
            return Err(AssetError::TooLarge {
                size: bytes.len(),
                maximum: MAX_ASSET_BYTES,
            });
        }

        let format = detect_format(bytes)?;
        if let Some(claimed) = claimed_media_type {
            let claimed = claimed.trim();
            if !claimed.eq_ignore_ascii_case(format.media_type()) {
                return Err(AssetError::ClaimedMediaTypeMismatch {
                    claimed: claimed.to_owned(),
                    detected: format.media_type(),
                });
            }
        }

        let digest = sha256_hex(bytes);
        let paths = self.paths(&digest)?;
        fs::create_dir_all(&paths.directory)?;

        match atomic_install_noclobber(&paths.blob, bytes)? {
            InstallOutcome::Created => {}
            InstallOutcome::Existing => self.verify_blob(&digest, bytes, format)?,
        }

        let candidate = AssetRecord {
            schema_version: ASSET_RECORD_VERSION.into(),
            digest: digest.clone(),
            media_type: format.media_type().into(),
            size: bytes.len() as u64,
            original_filename: sanitize_filename(original_filename),
            created_at: Utc::now(),
        };
        let mut metadata = serde_json::to_vec_pretty(&candidate)?;
        metadata.push(b'\n');

        match atomic_install_noclobber(&paths.metadata, &metadata)? {
            InstallOutcome::Created => Ok(candidate),
            InstallOutcome::Existing => self.read_and_validate_record(&digest, bytes, format),
        }
    }

    /// Reads an asset only when `digest` is an exact lowercase SHA-256 digest.
    ///
    /// The blob is rehashed and its magic-byte media type is compared with the
    /// sidecar on every read. Neither a digest prefix nor a `sha256:` URI is
    /// accepted, so caller input cannot influence path components.
    pub fn get(&self, digest: &str) -> Result<StoredAsset, AssetError> {
        let paths = self.paths(digest)?;
        let bytes = match fs::read(&paths.blob) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(AssetError::NotFound {
                    digest: digest.into(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let format = detect_format(&bytes).map_err(|_| AssetError::IntegrityMismatch {
            digest: digest.into(),
        })?;
        let record = self.read_and_validate_record(digest, &bytes, format)?;
        Ok(StoredAsset { record, bytes })
    }

    /// Returns the store root without granting any mutation operation.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn verify_blob(
        &self,
        digest: &str,
        expected_bytes: &[u8],
        expected_format: AssetFormat,
    ) -> Result<(), AssetError> {
        let paths = self.paths(digest)?;
        let existing = fs::read(paths.blob)?;
        let existing_format =
            detect_format(&existing).map_err(|_| AssetError::IntegrityMismatch {
                digest: digest.into(),
            })?;
        if existing != expected_bytes
            || existing_format != expected_format
            || sha256_hex(&existing) != digest
        {
            return Err(AssetError::IntegrityMismatch {
                digest: digest.into(),
            });
        }
        Ok(())
    }

    fn read_and_validate_record(
        &self,
        digest: &str,
        bytes: &[u8],
        format: AssetFormat,
    ) -> Result<AssetRecord, AssetError> {
        let paths = self.paths(digest)?;
        let metadata = match fs::read(paths.metadata) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(AssetError::MetadataMissing {
                    digest: digest.into(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let record: AssetRecord = serde_json::from_slice(&metadata)?;
        let valid = record.schema_version == ASSET_RECORD_VERSION
            && record.digest == digest
            && record.media_type == format.media_type()
            && record.size == bytes.len() as u64
            && record.original_filename == sanitize_filename(&record.original_filename)
            && sha256_hex(bytes) == digest;
        if !valid {
            return Err(AssetError::IntegrityMismatch {
                digest: digest.into(),
            });
        }
        Ok(record)
    }

    fn paths(&self, digest: &str) -> Result<AssetPaths, AssetError> {
        validate_digest(digest)?;
        let directory = self.root.join("sha256").join(&digest[..2]);
        Ok(AssetPaths {
            blob: directory.join(digest),
            metadata: directory.join(format!("{digest}.json")),
            directory,
        })
    }
}

#[derive(Debug)]
struct AssetPaths {
    directory: PathBuf,
    blob: PathBuf,
    metadata: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
    Avif,
}

impl AssetFormat {
    const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
            Self::Avif => "image/avif",
        }
    }
}

fn detect_format(bytes: &[u8]) -> Result<AssetFormat, AssetError> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(AssetFormat::Png);
    }
    if bytes.len() >= 3 && bytes[..3] == [0xff, 0xd8, 0xff] {
        return Ok(AssetFormat::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok(AssetFormat::Gif);
    }
    if bytes.len() >= 16
        && &bytes[..4] == b"RIFF"
        && &bytes[8..12] == b"WEBP"
        && matches!(&bytes[12..16], b"VP8 " | b"VP8L" | b"VP8X")
    {
        return Ok(AssetFormat::WebP);
    }
    if is_avif(bytes) {
        return Ok(AssetFormat::Avif);
    }

    if let Some(kind) = detect_unsafe_text_format(bytes) {
        return Err(AssetError::UnsafeFormat { kind });
    }
    Err(AssetError::UnsupportedFormat)
}

fn is_avif(bytes: &[u8]) -> bool {
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let box_size = u32::from_be_bytes(bytes[..4].try_into().expect("four-byte slice")) as usize;
    if !(16..=bytes.len()).contains(&box_size) {
        return false;
    }
    if matches!(&bytes[8..12], b"avif" | b"avis") {
        return true;
    }
    bytes[16..box_size]
        .chunks_exact(4)
        .any(|brand| matches!(brand, b"avif" | b"avis"))
}

fn detect_unsafe_text_format(bytes: &[u8]) -> Option<UnsafeAssetKind> {
    let prefix = &bytes[..bytes.len().min(4096)];
    let text = String::from_utf8_lossy(prefix);
    let trimmed = text
        .trim_start_matches('\u{feff}')
        .trim_start_matches(char::is_whitespace);
    let lowercase = trimmed.to_ascii_lowercase();
    if lowercase.contains("<svg") || lowercase.contains("<!doctype svg") {
        Some(UnsafeAssetKind::Svg)
    } else if lowercase.starts_with('<') {
        Some(UnsafeAssetKind::Html)
    } else {
        None
    }
}

fn validate_digest(digest: &str) -> Result<(), AssetError> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(AssetError::InvalidDigest)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sanitize_filename(value: &str) -> String {
    let basename = value.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut sanitized = String::with_capacity(basename.len().min(180));
    let mut previous_was_replacement = false;
    let mut previous_was_dot = false;

    for character in basename.chars().take(180) {
        let allowed = character.is_alphanumeric() || matches!(character, '.' | '-' | '_');
        if allowed {
            if character == '.' && previous_was_dot {
                continue;
            }
            sanitized.push(character);
            previous_was_replacement = false;
            previous_was_dot = character == '.';
        } else if !previous_was_replacement {
            sanitized.push('_');
            previous_was_replacement = true;
            previous_was_dot = false;
        }
    }

    let sanitized = sanitized.trim_matches(['.', '_']).to_owned();
    if sanitized.is_empty() {
        "asset".into()
    } else {
        sanitized
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallOutcome {
    Created,
    Existing,
}

/// Installs `bytes` as a completed file without ever replacing `destination`.
///
/// A fully written and synced temporary file is hard-linked into its final
/// name. Creating that directory entry is atomic and fails when the final name
/// already exists. Both paths are always inside the same directory/filesystem.
fn atomic_install_noclobber(
    destination: &Path,
    bytes: &[u8],
) -> Result<InstallOutcome, AssetError> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("asset destination has no parent"))?;
    let filename = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::other("asset destination is not valid UTF-8"))?;

    for _ in 0..64 {
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{filename}.tmp-{}-{counter}", std::process::id()));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };

        let write_result = (|| -> io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }

        let outcome = match fs::hard_link(&temporary, destination) {
            Ok(()) => InstallOutcome::Created,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => InstallOutcome::Existing,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        };
        fs::remove_file(temporary)?;
        return Ok(outcome);
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary asset path",
    )
    .into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeAssetKind {
    Svg,
    Html,
}

impl std::fmt::Display for UnsafeAssetKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Svg => formatter.write_str("SVG"),
            Self::Html => formatter.write_str("HTML or XML markup"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AssetError {
    #[error("asset is {size} bytes; maximum accepted size is {maximum} bytes")]
    TooLarge { size: usize, maximum: usize },
    #[error("active content format is forbidden: {kind}")]
    UnsafeFormat { kind: UnsafeAssetKind },
    #[error("unknown or unsupported asset format; expected PNG, JPEG, GIF, WebP, or AVIF")]
    UnsupportedFormat,
    #[error("claimed media type {claimed:?} does not match detected type {detected}")]
    ClaimedMediaTypeMismatch {
        claimed: String,
        detected: &'static str,
    },
    #[error("digest must be exactly 64 lowercase hexadecimal SHA-256 characters")]
    InvalidDigest,
    #[error("asset {digest} was not found")]
    NotFound { digest: String },
    #[error("asset {digest} exists without its metadata sidecar")]
    MetadataMissing { digest: String },
    #[error("stored asset {digest} failed its digest, type, size, or metadata integrity check")]
    IntegrityMismatch { digest: String },
    #[error("asset filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("asset metadata is invalid: {0}")]
    Metadata(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRtest-payload";

    #[test]
    fn roundtrip_uses_the_content_addressed_path_and_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let store = AssetStore::open(directory.path()).unwrap();

        let record = store.put(PNG, "my image.png", Some("image/png")).unwrap();
        assert_eq!(record.digest.len(), 64);
        assert!(
            record
                .digest
                .bytes()
                .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) })
        );
        assert_eq!(record.media_type, "image/png");
        assert_eq!(record.size, PNG.len() as u64);
        assert_eq!(record.original_filename, "my_image.png");

        let expected = directory
            .path()
            .join("sha256")
            .join(&record.digest[..2])
            .join(&record.digest);
        assert_eq!(fs::read(&expected).unwrap(), PNG);
        assert!(
            expected
                .with_file_name(format!("{}.json", record.digest))
                .is_file()
        );

        let loaded = store.get(&record.digest).unwrap();
        assert_eq!(loaded.record, record);
        assert_eq!(loaded.bytes, PNG);
    }

    #[test]
    fn duplicate_bytes_preserve_the_first_metadata_without_clobbering() {
        let directory = tempfile::tempdir().unwrap();
        let store = AssetStore::open(directory.path()).unwrap();

        let first = store.put(PNG, "first.png", None).unwrap();
        let second = store.put(PNG, "second.png", None).unwrap();

        assert_eq!(second, first);
        assert_eq!(second.original_filename, "first.png");
        assert_eq!(store.get(&first.digest).unwrap().bytes, PNG);
    }

    #[test]
    fn concurrent_duplicates_install_one_complete_record() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles = ["first.png", "second.png"].map(|filename| {
            let root = directory.path().to_owned();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let store = AssetStore::open(root).unwrap();
                barrier.wait();
                store.put(PNG, filename, Some("image/png")).unwrap()
            })
        });
        let [first, second] = handles.map(|handle| handle.join().unwrap());

        assert_eq!(first, second);
        assert!(matches!(
            first.original_filename.as_str(),
            "first.png" | "second.png"
        ));
        let store = AssetStore::open(directory.path()).unwrap();
        assert_eq!(store.get(&first.digest).unwrap().bytes, PNG);
    }

    #[test]
    fn every_allowlisted_magic_byte_format_is_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let store = AssetStore::open(directory.path()).unwrap();
        let formats: [(&[u8], &str, &str); 5] = [
            (PNG, "image/png", "asset.png"),
            (b"\xff\xd8\xff\xe0jpeg", "image/jpeg", "asset.jpg"),
            (b"GIF89apayload", "image/gif", "asset.gif"),
            (
                b"RIFF\x08\x00\x00\x00WEBPVP8 payload",
                "image/webp",
                "asset.webp",
            ),
            (
                b"\x00\x00\x00\x14ftypavif\x00\x00\x00\x00data",
                "image/avif",
                "asset.avif",
            ),
        ];

        for (bytes, media_type, filename) in formats {
            let record = store.put(bytes, filename, Some(media_type)).unwrap();
            assert_eq!(record.media_type, media_type);
            assert_eq!(store.get(&record.digest).unwrap().bytes, bytes);
        }
    }

    #[test]
    fn path_traversal_is_never_used_as_a_storage_path() {
        let directory = tempfile::tempdir().unwrap();
        let store = AssetStore::open(directory.path()).unwrap();
        let record = store
            .put(PNG, "../../outside/evil.png", Some("image/png"))
            .unwrap();

        assert_eq!(record.original_filename, "evil.png");
        assert!(!directory.path().join("outside").exists());
        assert!(matches!(
            store.get("../etc/passwd"),
            Err(AssetError::InvalidDigest)
        ));
        assert!(matches!(
            store.get(&record.digest.to_ascii_uppercase()),
            Err(AssetError::InvalidDigest)
        ));
        assert!(matches!(
            store.get(&format!("sha256:{}", record.digest)),
            Err(AssetError::InvalidDigest)
        ));
    }

    #[test]
    fn claimed_media_type_must_match_magic_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let store = AssetStore::open(directory.path()).unwrap();
        let error = store.put(PNG, "image.jpg", Some("image/jpeg")).unwrap_err();
        assert!(matches!(
            error,
            AssetError::ClaimedMediaTypeMismatch {
                detected: "image/png",
                ..
            }
        ));
    }

    #[test]
    fn svg_is_rejected_even_when_claimed_as_an_image() {
        let directory = tempfile::tempdir().unwrap();
        let store = AssetStore::open(directory.path()).unwrap();
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;
        let error = store
            .put(svg, "active.svg", Some("image/svg+xml"))
            .unwrap_err();
        assert!(matches!(
            error,
            AssetError::UnsafeFormat {
                kind: UnsafeAssetKind::Svg
            }
        ));
    }

    #[test]
    fn html_is_rejected_as_active_content() {
        let directory = tempfile::tempdir().unwrap();
        let store = AssetStore::open(directory.path()).unwrap();
        let error = store
            .put(
                b"<!doctype html><html><script>alert(1)</script></html>",
                "pixel.html",
                Some("text/html"),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            AssetError::UnsafeFormat {
                kind: UnsafeAssetKind::Html
            }
        ));
    }

    #[test]
    fn oversized_and_unknown_inputs_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = AssetStore::open(directory.path()).unwrap();
        let oversized = vec![0_u8; MAX_ASSET_BYTES + 1];
        assert!(matches!(
            store.put(&oversized, "large.png", Some("image/png")),
            Err(AssetError::TooLarge { .. })
        ));
        assert!(matches!(
            store.put(b"not an image", "notes.txt", None),
            Err(AssetError::UnsupportedFormat)
        ));
    }
}

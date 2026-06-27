//! The `.pkpkg` container format.
//!
//! A container frames a project for distribution. It starts with a fixed
//! 32-byte little-endian header, followed by the verbatim `peko.toml`
//! manifest, the source payload, and an optional detached signature trailer.
//!
//! The header holds the magic bytes `PEKO`, a container version, a compression
//! tag, a flags byte whose low bit marks a signed container, the manifest
//! length, the payload length, and 12 reserved bytes.

use thiserror::Error;

/// The four magic bytes at the start of every container.
pub const CONTAINER_MAGIC: [u8; 4] = *b"PEKO";

/// The byte length of the fixed container header.
pub const CONTAINER_HEADER_LEN: usize = 32;

/// The container format version this module reads and writes.
pub const CONTAINER_VERSION: u16 = 1;

/// The compression applied to a container payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// The payload is stored uncompressed.
    None,
    /// The payload is compressed with zstd.
    Zstd,
}

impl Compression {
    /// The on-disk tag for this compression.
    pub fn as_u8(self) -> u8 {
        match self {
            Compression::None => 0,
            Compression::Zstd => 1,
        }
    }

    /// The compression named by an on-disk tag.
    pub fn from_u8(tag: u8) -> Option<Compression> {
        match tag {
            0 => Some(Compression::None),
            1 => Some(Compression::Zstd),
            _ => None,
        }
    }
}

/// The fixed header at the start of a `.pkpkg` container.
#[derive(Debug, Clone, Copy)]
pub struct ContainerHeader {
    /// The container format version.
    pub container_version: u16,
    /// The compression applied to the payload.
    pub compression: Compression,
    /// Whether a detached signature trailer follows the payload.
    pub signed: bool,
    /// The byte length of the embedded manifest text.
    pub meta_len: u32,
    /// The byte length of the payload.
    pub payload_len: u64,
}

impl ContainerHeader {
    /// Serialize the header to its fixed 32-byte form.
    pub fn encode(&self) -> [u8; CONTAINER_HEADER_LEN] {
        let mut bytes = [0u8; CONTAINER_HEADER_LEN];
        bytes[0..4].copy_from_slice(&CONTAINER_MAGIC);
        bytes[4..6].copy_from_slice(&self.container_version.to_le_bytes());
        bytes[6] = self.compression.as_u8();
        bytes[7] = u8::from(self.signed);
        bytes[8..12].copy_from_slice(&self.meta_len.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.payload_len.to_le_bytes());
        bytes
    }

    /// Parse a header from the leading bytes of a container.
    pub fn decode(bytes: &[u8]) -> Result<ContainerHeader, ContainerError> {
        let header: &[u8; CONTAINER_HEADER_LEN] = bytes
            .get(..CONTAINER_HEADER_LEN)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(ContainerError::Truncated)?;

        if header[0..4] != CONTAINER_MAGIC {
            return Err(ContainerError::BadMagic);
        }

        let compression =
            Compression::from_u8(header[6]).ok_or(ContainerError::UnknownCompression(header[6]))?;

        Ok(ContainerHeader {
            container_version: u16::from_le_bytes([header[4], header[5]]),
            compression,
            signed: header[7] & 1 != 0,
            meta_len: u32::from_le_bytes(header[8..12].try_into().unwrap()),
            payload_len: u64::from_le_bytes(header[12..20].try_into().unwrap()),
        })
    }
}

/// A container split into its header and sections, borrowing the source bytes.
#[derive(Debug, Clone, Copy)]
pub struct Container<'a> {
    /// The parsed header.
    pub header: ContainerHeader,
    /// The embedded `peko.toml` text.
    pub manifest: &'a str,
    /// The payload bytes.
    pub payload: &'a [u8],
    /// The detached signature trailer, present when the header is signed.
    pub signature: Option<&'a [u8]>,
}

/// Frame a manifest and payload into a `.pkpkg` container.
///
/// The manifest is embedded verbatim. Passing a signature sets the signed flag
/// and appends the bytes as the trailer.
pub fn encode_container(
    manifest: &str,
    compression: Compression,
    payload: &[u8],
    signature: Option<&[u8]>,
) -> Vec<u8> {
    let header = ContainerHeader {
        container_version: CONTAINER_VERSION,
        compression,
        signed: signature.is_some(),
        meta_len: manifest.len() as u32,
        payload_len: payload.len() as u64,
    };
    let signature = signature.unwrap_or_default();

    let mut bytes = Vec::with_capacity(
        CONTAINER_HEADER_LEN + manifest.len() + payload.len() + signature.len(),
    );
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(manifest.as_bytes());
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(signature);
    bytes
}

/// Parse a `.pkpkg` container into its header and borrowed sections.
pub fn decode_container(bytes: &[u8]) -> Result<Container<'_>, ContainerError> {
    let header = ContainerHeader::decode(bytes)?;

    let meta_end = CONTAINER_HEADER_LEN
        .checked_add(header.meta_len as usize)
        .ok_or(ContainerError::Truncated)?;
    let payload_end = meta_end
        .checked_add(header.payload_len as usize)
        .ok_or(ContainerError::Truncated)?;
    if bytes.len() < payload_end {
        return Err(ContainerError::Truncated);
    }

    let manifest = std::str::from_utf8(&bytes[CONTAINER_HEADER_LEN..meta_end])
        .map_err(|_| ContainerError::ManifestNotUtf8)?;
    let signature = header.signed.then(|| &bytes[payload_end..]);

    Ok(Container {
        header,
        manifest,
        payload: &bytes[meta_end..payload_end],
        signature,
    })
}

/// One failure mode for reading a `.pkpkg` container.
#[derive(Debug, Error)]
pub enum ContainerError {
    /// The buffer was shorter than the header or its declared sections.
    #[error("container is truncated")]
    Truncated,

    /// The leading magic bytes were not `PEKO`.
    #[error("container does not start with the PEKO magic")]
    BadMagic,

    /// The compression tag did not name a known compression.
    #[error("container uses unknown compression tag {0}")]
    UnknownCompression(u8),

    /// The embedded manifest section was not valid UTF-8.
    #[error("container manifest is not valid UTF-8")]
    ManifestNotUtf8,
}

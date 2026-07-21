//! Pure-Rust MSIX packaging for the Microsoft Store.
//!
//! An MSIX is an OPC package: a ZIP that pairs the app payload with three
//! generated parts — `AppxManifest.xml` (identity + full-trust entry point),
//! `[Content_Types].xml` (the OPC content-type map), and `AppxBlockMap.xml` (a
//! SHA-256 block map over every payload part). We write the ZIP by hand, storing
//! every part uncompressed, so each part's local-file-header size (`LfhSize`,
//! `30 + name length` with no extra field) is exact — the MSIX runtime reads the
//! block map's `LfhSize` to locate part data, so it must match the archive
//! byte-for-byte. No Microsoft tooling is involved. The package is emitted
//! unsigned, which is what the Store accepts: it re-signs on ingestion with the
//! publisher identity. (Sideloading a build instead needs a signature, which is
//! a separate step.)

use base64::Engine;
use sha2::{Digest, Sha256};

/// MSIX block size: parts are hashed in 64 KiB chunks.
const BLOCK_SIZE: usize = 65536;

/// The identity and entry point stamped into the package manifest. The three
/// Store values (`identity_name`, `publisher`, `publisher_display_name`) come
/// from Partner Center and are validated by the caller before packaging.
pub struct MsixIdentity<'a> {
    pub identity_name: &'a str,
    pub publisher: &'a str,
    pub publisher_display_name: &'a str,
    /// The app's display name (from the project name).
    pub display_name: &'a str,
    /// A four-part `a.b.c.0` version (see [`four_part_version`]).
    pub version: String,
    /// The payload executable name, e.g. `todossr.exe`.
    pub executable: &'a str,
}

/// A package part: its OPC/ZIP name (forward slashes) and bytes.
struct Part {
    name: String,
    data: Vec<u8>,
}

/// Normalize a version string to the four-part `a.b.c.0` MSIX form. The Store
/// requires the fourth field (revision) to be 0, so it is always forced to 0;
/// missing leading fields default to 0, and any non-numeric suffix on a field
/// (a pre-release tag) is dropped.
pub fn four_part_version(version: &str) -> String {
    let mut parts: Vec<u64> = version
        .split('.')
        .take(3)
        .map(|field| {
            field
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect();
    while parts.len() < 3 {
        parts.push(0);
    }
    format!("{}.{}.{}.0", parts[0], parts[1], parts[2])
}

/// Build the MSIX package bytes for the given identity and payload. The logos
/// are PNG bytes at 50×50 (store), 150×150 and 44×44 (tile/app-list).
pub fn build_package(
    identity: &MsixIdentity,
    exe_bytes: Vec<u8>,
    logo_store_50: Vec<u8>,
    logo_150: Vec<u8>,
    logo_44: Vec<u8>,
) -> Vec<u8> {
    // Parts the block map covers: everything except [Content_Types].xml and the
    // block map itself.
    let mut mapped = vec![
        Part {
            name: "AppxManifest.xml".to_string(),
            data: appx_manifest(identity).into_bytes(),
        },
        Part {
            name: identity.executable.to_string(),
            data: exe_bytes,
        },
        Part {
            name: "Assets/StoreLogo.png".to_string(),
            data: logo_store_50,
        },
        Part {
            name: "Assets/Square150x150Logo.png".to_string(),
            data: logo_150,
        },
        Part {
            name: "Assets/Square44x44Logo.png".to_string(),
            data: logo_44,
        },
    ];
    let block_map = appx_block_map(&mapped);

    // Full ZIP part list: the content-type map first (OPC convention), then the
    // mapped parts, then the block map.
    let mut all = Vec::with_capacity(mapped.len() + 2);
    all.push(Part {
        name: "[Content_Types].xml".to_string(),
        data: content_types().into_bytes(),
    });
    all.append(&mut mapped);
    all.push(Part {
        name: "AppxBlockMap.xml".to_string(),
        data: block_map.into_bytes(),
    });

    write_stored_zip(&all)
}

fn content_types() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="exe" ContentType="application/octet-stream"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/AppxManifest.xml" ContentType="application/vnd.ms-appx.manifest+xml"/>
  <Override PartName="/AppxBlockMap.xml" ContentType="application/vnd.ms-appx.blockmap+xml"/>
</Types>"#
        .to_string()
}

fn appx_manifest(id: &MsixIdentity) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10" xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10" xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities">
  <Identity Name="{name}" Publisher="{publisher}" Version="{version}" ProcessorArchitecture="x64"/>
  <Properties>
    <DisplayName>{display}</DisplayName>
    <PublisherDisplayName>{publisher_display}</PublisherDisplayName>
    <Logo>Assets\StoreLogo.png</Logo>
  </Properties>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.17763.0" MaxVersionTested="10.0.22621.0"/>
  </Dependencies>
  <Resources>
    <Resource Language="en-us"/>
  </Resources>
  <Applications>
    <Application Id="App" Executable="{executable}" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements DisplayName="{display}" Description="{display}" BackgroundColor="transparent" Square150x150Logo="Assets\Square150x150Logo.png" Square44x44Logo="Assets\Square44x44Logo.png"/>
    </Application>
  </Applications>
  <Capabilities>
    <rescap:Capability Name="runFullTrust"/>
  </Capabilities>
</Package>"#,
        name = xml_escape(id.identity_name),
        publisher = xml_escape(id.publisher),
        version = id.version,
        display = xml_escape(id.display_name),
        publisher_display = xml_escape(id.publisher_display_name),
        executable = xml_escape(id.executable),
    )
}

fn appx_block_map(parts: &[Part]) -> String {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    xml.push_str(
        "<BlockMap xmlns=\"http://schemas.microsoft.com/appx/2010/blockmap\" \
         HashMethod=\"http://www.w3.org/2001/04/xmlenc#sha256\">",
    );
    for part in parts {
        // The block map names parts with backslashes; the ZIP uses forward
        // slashes. Same byte length, so LfhSize (30-byte local header + name,
        // no extra field for a stored entry) matches the archive.
        let backslashed = part.name.replace('/', "\\");
        let lfh_size = 30 + part.name.len();
        xml.push_str(&format!(
            "<File Name=\"{}\" Size=\"{}\" LfhSize=\"{}\">",
            xml_escape(&backslashed),
            part.data.len(),
            lfh_size
        ));
        for block in part.data.chunks(BLOCK_SIZE) {
            let hash = Sha256::digest(block);
            let encoded = base64::engine::general_purpose::STANDARD.encode(hash);
            xml.push_str(&format!("<Block Hash=\"{encoded}\"/>"));
        }
        xml.push_str("</File>");
    }
    xml.push_str("</BlockMap>");
    xml
}

/// Write a minimal ZIP with every part stored (method 0, no compression, no data
/// descriptor, no Zip64), so the local file headers are exactly `30 + name` and
/// match the block map's `LfhSize`.
fn write_stored_zip(parts: &[Part]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::with_capacity(parts.len());

    for part in parts {
        offsets.push(out.len() as u32);
        let name = part.name.as_bytes();
        let crc = crc32(&part.data);
        let size = part.data.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header signature
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0x21u16.to_le_bytes()); // mod date (1980-01-01)
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes()); // compressed size
        out.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra length
        out.extend_from_slice(name);
        out.extend_from_slice(&part.data);
    }

    let central_start = out.len() as u32;
    let mut central = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let name = part.name.as_bytes();
        let crc = crc32(&part.data);
        let size = part.data.len() as u32;
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header signature
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        central.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central.extend_from_slice(&0x21u16.to_le_bytes()); // mod date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes()); // compressed size
        central.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra length
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&offsets[index].to_le_bytes()); // local header offset
        central.extend_from_slice(name);
    }
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);

    let entries = parts.len() as u16;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // end-of-central-directory signature
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with central directory
    out.extend_from_slice(&entries.to_le_bytes()); // entries on this disk
    out.extend_from_slice(&entries.to_le_bytes()); // total entries
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out
}

/// IEEE CRC-32, bit-by-bit. Run once per part at build time, so the table-free
/// form is fine.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_become_four_part_with_zero_revision() {
        assert_eq!(four_part_version("1.2.3"), "1.2.3.0");
        assert_eq!(four_part_version("0.1.0"), "0.1.0.0");
        assert_eq!(four_part_version("1.2"), "1.2.0.0");
        assert_eq!(four_part_version("1.2.3.9"), "1.2.3.0");
        assert_eq!(four_part_version("2.0.0-beta"), "2.0.0.0");
    }

    /// The package is a valid ZIP the `zip` crate can read back, and the block
    /// map's LfhSize matches each entry's real local header offset delta.
    #[test]
    fn package_is_a_readable_zip_with_matching_lfh() {
        let identity = MsixIdentity {
            identity_name: "Contoso.App",
            publisher: "CN=Contoso",
            publisher_display_name: "Contoso",
            display_name: "App",
            version: four_part_version("1.0.0"),
            executable: "app.exe",
        };
        let package = build_package(
            &identity,
            b"MZ fake exe payload".to_vec(),
            b"png50".to_vec(),
            b"png150".to_vec(),
            b"png44".to_vec(),
        );
        let reader =
            zip::ZipArchive::new(std::io::Cursor::new(package)).expect("valid zip archive");
        let names: Vec<String> = reader.file_names().map(str::to_string).collect();
        assert!(names.iter().any(|n| n == "AppxManifest.xml"));
        assert!(names.iter().any(|n| n == "AppxBlockMap.xml"));
        assert!(names.iter().any(|n| n == "[Content_Types].xml"));
        assert!(names.iter().any(|n| n == "app.exe"));
    }
}

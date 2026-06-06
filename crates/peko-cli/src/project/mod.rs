//! Peko project metadata: parsing, serialization, and on-disk discovery.
//!
//! A "Peko project" is a directory tree containing a `.peko/project/config.pkbin`
//! file describing the project's name, optional UI metadata (bundle id, app
//! version, target platforms, app icon), plus an `incremental/` directory that
//! caches per-file build artifacts between runs. UI projects also carry an
//! `assets/` directory at the project root whose files are the asset set.
//!
//! The configuration binary uses a simple custom format: a magic header,
//! followed by typed blocks (`PROJECTCONFIG`, `TARGETPLATFORMS`, `APPICON`),
//! each terminated by 8 null bytes. [`PekoBinaryReader`] provides the streaming
//! parse primitives; [`PekoProject::from_binary_file`] drives them.

use std::collections::BTreeMap;
use std::env::current_dir;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use derive_new::new;
use image::{DynamicImage, EncodableLayout};
use thiserror::Error;

use crate::bundler::cartool::{carinfo, carwriter};
use crate::execution::incremental::ProjectIncrementalMap;

/// One failure mode for project loading and parsing.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// No project config was found in the searched directory or any parent
    /// within the search-depth limit.
    #[error("couldn't find project in the current directory or its parents")]
    NotFound,

    /// The project root was located but `main.peko` is missing.
    #[error("couldn't find project entrypoint main.peko in {0}")]
    MissingEntrypoint(PathBuf),

    /// The config binary exists but couldn't be read.
    #[error("couldn't read Peko binary at {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The config binary path didn't have enough parent directories to be a
    /// valid project layout (we expect `<root>/.peko/project/config.pkbin`).
    #[error("Peko binary at {0} is not inside a valid project directory layout")]
    InvalidLayout(PathBuf),

    /// The config binary parsed inconsistently with what the format requires.
    #[error("Peko binary ({path}) is corrupt: {detail}")]
    Corrupt { path: PathBuf, detail: String },
}

/// App icon stored as raw pixel data plus the originating image width.
#[derive(Debug, Clone, new)]
pub struct ProjectIcon {
    pub icon_pixel_data: Vec<u8>,
    pub width: u32,
}

impl ProjectIcon {
    /// Returns this image's bytes resized to the desired dimensions.
    pub fn resize(&self, width: u32, height: u32) -> ProjectIcon {
        let rgba_image = DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(
                self.width,
                (self.icon_pixel_data.len() as u32 / 4) / self.width,
                self.icon_pixel_data.clone(),
            )
            .unwrap(),
        );

        ProjectIcon::new(
            rgba_image
                .resize_exact(width, height, image::imageops::FilterType::Lanczos3)
                .to_rgba8()
                .as_bytes()
                .to_vec(),
            width,
        )
    }

    /// Returns the icon's pixel data converted from the project's stored
    /// BGRA ordering to RGBA.
    pub fn get_rgba_pixels(&self) -> Vec<u8> {
        let mut buffer = self.icon_pixel_data.clone();
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        buffer
    }

    /// Converts the icon to Apple's CAR binary format.
    pub fn to_car<W: Write + Seek>(&self, writer: &mut W) {
        let iconx1024 = self.resize(1024, 1024);

        let carbinary = carinfo::CarBinary::new(
            carinfo::CarHeader {
                coreui_version: 973,
                storage_version: 17,
                main_version_string: "@(#)PROGRAM:CoreUI  PROJECT:CoreUI-973.1".to_owned(),
                asset_storage_version_string:
                    "Xcode 26.3 (17C529) via AssetCatalogSimulatorAgent".to_owned(),
            },
            carinfo::CarMetadata {
                deployment_platform_version: "26.2".to_owned(),
                deployment_platform: "ios".to_owned(),
                authoring_tool:
                    "@(#)PROGRAM:CoreThemeDefinition  PROJECT:CoreThemeDefinition-653.3  [IIO-2784.3.4]"
                        .to_owned(),
            },
            vec![
                carinfo::KeyAttributeType::Appearance,   // 0
                carinfo::KeyAttributeType::Localization, // 0
                carinfo::KeyAttributeType::Scale,        // 1
                carinfo::KeyAttributeType::Idiom,        // 1 for iphone, 2 for ipad
                carinfo::KeyAttributeType::Subtype,      // 0
                carinfo::KeyAttributeType::Dimension2,   // 0 in icon image, 1 in multisized image
                carinfo::KeyAttributeType::Identifier,   // 6849
                carinfo::KeyAttributeType::Element,      // 85
                carinfo::KeyAttributeType::Part, // 218 for main icon, 220 for sub icons
            ],
            carinfo::BomTree {
                block_name: Some("RENDITIONS".to_owned()),
                keys: vec![
                    // iphone icon rendition keys
                    carinfo::ValueBlock::RenditionKey(vec![0, 0, 1, 1, 0, 0, 6849, 85, 218]),
                    carinfo::ValueBlock::RenditionKey(vec![0, 0, 1, 1, 0, 1, 6849, 85, 220]),
                    // ipad icon rendition keys
                    carinfo::ValueBlock::RenditionKey(vec![0, 0, 1, 2, 0, 0, 6849, 85, 218]),
                    carinfo::ValueBlock::RenditionKey(vec![0, 0, 1, 2, 0, 1, 6849, 85, 220]),
                ],
                values: vec![
                    carinfo::ValueBlock::CSIData(carinfo::CSIData {
                        width: 0,
                        height: 0,
                        scale: 0,
                        layout: 1010,
                        asset_name: "AppIcon".to_owned(),
                        tlv_entries: vec![
                            carinfo::TLVEntry {
                                tlv_type: 1004,
                                data: vec![0; 8],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1006,
                                data: vec![1, 0, 0, 0],
                            },
                        ],
                        asset_data: Box::new(carinfo::ValueBlock::MultisizedImageSetData(
                            carinfo::MSISData {
                                idiom: 1,
                                scale: 1,
                                width: 1024,
                                height: 1024,
                                reference_index: 1,
                            },
                        )),
                    }),
                    carinfo::ValueBlock::CSIData(carinfo::CSIData {
                        width: 1024,
                        height: 1024,
                        scale: 100,
                        layout: 12,
                        asset_name: "icon.png".to_owned(),
                        tlv_entries: vec![
                            carinfo::TLVEntry {
                                tlv_type: 1001,
                                data: vec![
                                    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 4, 0, 0,
                                ],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1003,
                                data: vec![
                                    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                                    0, 4, 0, 0, 0, 4, 0, 0,
                                ],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1004,
                                data: vec![0, 0, 0, 0, 0, 0, 128, 63],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1004,
                                data: vec![0, 0, 0, 0, 0, 0, 0, 0],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1006,
                                data: vec![1, 0, 0, 0],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1007,
                                data: vec![0, 16, 0, 0],
                            },
                        ],
                        asset_data: Box::new(carinfo::ValueBlock::CELMImageData(
                            carinfo::CELMImageData::new(
                                iconx1024.icon_pixel_data.clone(),
                                false,
                                1024,
                            ),
                        )),
                    }),
                    carinfo::ValueBlock::CSIData(carinfo::CSIData {
                        width: 0,
                        height: 0,
                        scale: 0,
                        layout: 1010,
                        asset_name: "AppIcon".to_owned(),
                        tlv_entries: vec![
                            carinfo::TLVEntry {
                                tlv_type: 1004,
                                data: vec![0; 8],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1006,
                                data: vec![1, 0, 0, 0],
                            },
                        ],
                        asset_data: Box::new(carinfo::ValueBlock::MultisizedImageSetData(
                            carinfo::MSISData {
                                idiom: 1,
                                scale: 1,
                                width: 1024,
                                height: 1024,
                                reference_index: 1,
                            },
                        )),
                    }),
                    carinfo::ValueBlock::CSIData(carinfo::CSIData {
                        width: 1024,
                        height: 1024,
                        scale: 100,
                        layout: 12,
                        asset_name: "icon.png".to_owned(),
                        tlv_entries: vec![
                            carinfo::TLVEntry {
                                tlv_type: 1001,
                                data: vec![
                                    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 4, 0, 0,
                                ],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1003,
                                data: vec![
                                    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                                    0, 4, 0, 0, 0, 4, 0, 0,
                                ],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1004,
                                data: vec![0, 0, 0, 0, 0, 0, 128, 63],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1006,
                                data: vec![1, 0, 0, 0],
                            },
                            carinfo::TLVEntry {
                                tlv_type: 1007,
                                data: vec![0, 16, 0, 0],
                            },
                        ],
                        asset_data: Box::new(carinfo::ValueBlock::CELMImageData(
                            carinfo::CELMImageData::new(
                                iconx1024.icon_pixel_data.clone(),
                                false,
                                1024,
                            ),
                        )),
                    }),
                ],
                block_size: 4096,
            },
            carinfo::BomTree {
                block_name: Some("FACETKEYS".to_owned()),
                keys: vec![carinfo::ValueBlock::String("AppIcon".to_owned())],
                values: vec![carinfo::ValueBlock::FacetKeyToken(carinfo::FacetKeyToken {
                    attributes: {
                        let mut attrs = BTreeMap::new();
                        attrs.insert(1, 85);
                        attrs.insert(2, 220);
                        attrs.insert(17, 6849);
                        attrs
                    },
                })],
                block_size: 4096,
            },
            carinfo::BomTree {
                block_name: Some("APPEARANCEKEYS".to_owned()),
                keys: vec![carinfo::ValueBlock::String("UIAppearanceAny".to_owned())],
                values: vec![carinfo::ValueBlock::Int(0)],
                block_size: 4096,
            },
        );

        writer
            .write(
                carwriter::CarWriter::new(carbinary)
                    .create_binary()
                    .as_slice(),
            )
            .unwrap();
    }

    /// Writes this icon to a PNG.
    pub fn to_png<W: Write + Seek>(&self, writer: &mut W) {
        let rgba_image = DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(
                self.width,
                (self.icon_pixel_data.len() as u32 / 4) / self.width,
                self.get_rgba_pixels(),
            )
            .unwrap(),
        );

        rgba_image
            .write_to(writer, image::ImageFormat::Png)
            .unwrap();
    }

    /// Resizes the icon to 256x256 and writes it as an `.icns` for Apple
    /// bundles.
    pub fn to_icns<W: Write + Seek>(&self, writer: &mut W) {
        let imagex256 = self.resize(256, 256);
        let mut icns_family = icns::IconFamily::new();
        icns_family
            .add_icon(
                &icns::Image::from_data(
                    icns::PixelFormat::RGBA,
                    256,
                    256,
                    imagex256.get_rgba_pixels(),
                )
                .unwrap(),
            )
            .unwrap();

        icns_family.write(writer).unwrap();
    }

    /// Resizes the icon to 256x256 and writes it as an `.ico` for Windows
    /// apps.
    pub fn to_ico<W: Write + Seek>(&self, writer: &mut W) {
        let imagex256 = self.resize(256, 256);

        let mut icon_directory = ico::IconDir::new(ico::ResourceType::Icon);
        icon_directory.add_entry(
            ico::IconDirEntry::encode(&ico::IconImage::from_rgba_data(
                256,
                256,
                imagex256.get_rgba_pixels(),
            ))
            .unwrap(),
        );

        icon_directory.write(writer).unwrap();
    }
}

/// Metadata specific to UI projects: bundle id, app version, target
/// platforms, and app icon.
#[derive(Debug, Clone, new)]
pub struct UIProjectInfo {
    pub bundle_id: String,
    pub version: String,
    pub platforms: Vec<peko_core::target::OperatingSystem>,
    pub icon: ProjectIcon,
}

/// A discovered or constructed Peko project.
#[derive(Debug, Clone)]
pub struct PekoProject {
    root: PathBuf,
    entry_file: PathBuf,
    pub incremental_info: Option<ProjectIncrementalMap>,

    pub name: String,
    pub ui_project_info: Option<UIProjectInfo>,
}

/// Streaming reader over a Peko project binary's bytes.
///
/// Each `parse_*` method reads and advances. Tag and nullspace parses return
/// `bool` (matched / not matched), while typed reads return `Option` and
/// fail to `None` when the cursor would run past the end of the buffer.
pub struct PekoBinaryReader {
    index: usize,
    bytes: Vec<u8>,
}

impl PekoBinaryReader {
    /// Creates a reader over the given bytes, starting at index 0.
    pub fn new(bytes: Vec<u8>) -> PekoBinaryReader {
        PekoBinaryReader { index: 0, bytes }
    }

    /// `true` if the cursor still points at a readable byte.
    pub fn inbounds(&self) -> bool {
        self.index < self.bytes.len()
    }

    /// `true` if the cursor has not yet moved past one-past-the-end. The
    /// inclusive form is used after an advance to confirm the read stayed
    /// within the buffer.
    pub fn inbounds_inclusive(&self) -> bool {
        self.index <= self.bytes.len()
    }

    /// Skip past a magic tag. Returns `false` if the tag bytes don't match
    /// at the cursor.
    pub fn parse_magic(&mut self, magic: impl AsRef<str>) -> bool {
        if !self.inbounds() {
            return false;
        }

        for byte in magic.as_ref().bytes() {
            if self.index >= self.bytes.len() || self.bytes[self.index] != byte {
                return false;
            }
            self.index += 1;
        }

        self.inbounds_inclusive()
    }

    /// Skip past 8 null bytes (the block terminator).
    pub fn parse_nullspace(&mut self) -> bool {
        if !self.inbounds() {
            return false;
        }

        for _ in 0..8 {
            if self.index >= self.bytes.len() || self.bytes[self.index] != 0 {
                return false;
            }
            self.index += 1;
        }

        self.inbounds_inclusive()
    }

    /// Parse one byte.
    pub fn parse_u8(&mut self) -> Option<u8> {
        if !self.inbounds() {
            return None;
        }
        let byte_index = self.index;
        self.index += 1;
        if !self.inbounds_inclusive() {
            return None;
        }
        Some(self.bytes[byte_index])
    }

    /// Parse a big-endian `u32`.
    pub fn parse_u32(&mut self) -> Option<u32> {
        let mut bytes_u32 = [0u8; 4];
        for slot in &mut bytes_u32 {
            if !self.inbounds() {
                return None;
            }
            *slot = self.bytes[self.index];
            self.index += 1;
        }

        if !self.inbounds_inclusive() {
            return None;
        }
        Some(u32::from_be_bytes(bytes_u32))
    }

    /// Parse a byte-per-char string of the provided length.
    ///
    /// Each byte becomes one `char` (Latin-1 style), preserving the
    /// project binary's original encoding. Use [`parse_bytes`] when the
    /// payload is binary data rather than text.
    ///
    /// [`parse_bytes`]: PekoBinaryReader::parse_bytes
    pub fn parse_string(&mut self, string_length: u32) -> Option<String> {
        let mut string = String::new();
        for _ in 0..string_length {
            if !self.inbounds() {
                return None;
            }
            string.push(self.bytes[self.index] as char);
            self.index += 1;
        }

        if !self.inbounds_inclusive() {
            return None;
        }
        Some(string)
    }

    /// Read `length` bytes out of the buffer and advance the cursor past
    /// them. Returns `None` if fewer than `length` bytes remain.
    pub fn parse_bytes(&mut self, length: u32) -> Option<Vec<u8>> {
        let length = length as usize;
        let end = self.index.checked_add(length)?;
        if end > self.bytes.len() {
            return None;
        }
        let slice = self.bytes[self.index..end].to_vec();
        self.index = end;
        Some(slice)
    }

    /// The current cursor index into the buffer.
    pub fn get_index(&self) -> usize {
        self.index
    }

    /// The reader's underlying byte buffer.
    pub fn get_raw_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Manually advance the cursor by `count` bytes (without bounds
    /// checking, callers must ensure the advance stays within the
    /// buffer).
    pub fn increase_index_by(&mut self, count: usize) {
        self.index += count;
    }
}

impl PekoProject {
    /// Construct a project from already-resolved fields.
    pub fn new(
        name: String,
        root: PathBuf,
        incremental_info: Option<ProjectIncrementalMap>,
        entry_file: PathBuf,
        ui_project_info: Option<UIProjectInfo>,
    ) -> PekoProject {
        PekoProject {
            root,
            entry_file,
            incremental_info,
            name,
            ui_project_info,
        }
    }

    /// The project's entrypoint source file (`<root>/main.peko`).
    pub fn get_entrypoint(&self) -> &Path {
        &self.entry_file
    }

    /// The project's root directory.
    pub fn get_root(&self) -> &Path {
        &self.root
    }

    /// The project's asset directory (`<root>/assets`).
    pub fn assets_dir(&self) -> PathBuf {
        self.root.join("assets")
    }

    /// Build the asset set from the on-disk asset directory.
    ///
    /// Walks `<root>/assets` recursively and maps each file to its absolute
    /// path. Keys are the file's path relative to the asset directory, with
    /// forward slashes as separators (for example "icons/home.png"). The
    /// directory itself is the index; no manifest is read. Returns an empty
    /// map when the asset directory is absent.
    pub fn asset_index(&self) -> BTreeMap<String, PathBuf> {
        let mut index = BTreeMap::new();
        let assets_root = self.assets_dir();
        collect_assets(&assets_root, &assets_root, &mut index);
        index
    }

    /// Locate and load the project that owns the current working directory.
    pub fn from_current_directory() -> Result<PekoProject, ProjectError> {
        let cwd = current_dir().map_err(|_| ProjectError::NotFound)?;
        Self::from_directory(cwd)
    }

    /// Locate and load the project that owns `directory` or any of its
    /// nearest ancestor directories (up to a small fixed search depth).
    pub fn from_directory<P: AsRef<Path>>(directory: P) -> Result<PekoProject, ProjectError> {
        let mut project_folder = directory.as_ref().to_path_buf();
        let mut search_limit = 5;

        while search_limit > 0
            && project_folder.parent().is_some()
            && !project_folder.join(".peko/project/config.pkbin").exists()
        {
            project_folder = project_folder.parent().unwrap().to_path_buf();
            search_limit -= 1;
        }

        if search_limit == 0 || !project_folder.join(".peko/project/config.pkbin").exists() {
            return Err(ProjectError::NotFound);
        }

        Self::from_binary_file(project_folder.join(".peko/project/config.pkbin"))
    }

    /// Parse a project's metadata out of an on-disk config binary.
    pub fn from_binary_file<P: AsRef<Path>>(binary_path: P) -> Result<PekoProject, ProjectError> {
        let binary_path = binary_path.as_ref();

        if !binary_path.exists() {
            return Err(ProjectError::NotFound);
        }

        let path_cannon =
            binary_path
                .canonicalize()
                .map_err(|source| ProjectError::Unreadable {
                    path: binary_path.to_path_buf(),
                    source,
                })?;

        // From `<root>/.peko/project/config.pkbin`, walk up three levels for
        // the project root. If the layout is wrong, surface a typed error
        // rather than panicking.
        let project_root = path_cannon
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or_else(|| ProjectError::InvalidLayout(path_cannon.clone()))?
            .to_path_buf();

        let incremental_dir = project_root.join(".peko/incremental");
        let incremental_info =
            ProjectIncrementalMap::from_incremental_directory(&incremental_dir, true);
        if incremental_info.is_none() && incremental_dir.exists() {
            let _ = std::fs::remove_dir_all(&incremental_dir);
        }

        if !project_root.join("main.peko").exists() {
            return Err(ProjectError::MissingEntrypoint(project_root.clone()));
        }

        let raw = std::fs::read(binary_path).map_err(|source| ProjectError::Unreadable {
            path: binary_path.to_path_buf(),
            source,
        })?;
        let mut binary_reader = PekoBinaryReader::new(raw);

        let corrupt = |detail: &str| ProjectError::Corrupt {
            path: path_cannon.clone(),
            detail: detail.to_owned(),
        };

        if !binary_reader.parse_magic("PEKOPROJECT") || !binary_reader.parse_nullspace() {
            return Err(corrupt("couldn't find PEKOPROJECT tag"));
        }

        if !binary_reader.parse_magic("PROJECTCONFIG") {
            return Err(corrupt("couldn't find PROJECTCONFIG block"));
        }

        let project_type = binary_reader
            .parse_u8()
            .ok_or_else(|| corrupt("truncated PROJECTCONFIG block"))?;

        let name_length = binary_reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated project name length"))?;
        let project_name = binary_reader
            .parse_string(name_length)
            .ok_or_else(|| corrupt("truncated project name"))?;

        if project_type == 1 {
            return Ok(PekoProject {
                root: project_root.clone(),
                entry_file: project_root.join("main.peko"),
                incremental_info,
                name: project_name,
                ui_project_info: None,
            });
        }

        let id_length = binary_reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated project id length"))?;
        let project_id = binary_reader
            .parse_string(id_length)
            .ok_or_else(|| corrupt("truncated project id"))?;

        let version_length = binary_reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated project version length"))?;
        let project_version = binary_reader
            .parse_string(version_length)
            .ok_or_else(|| corrupt("truncated project version"))?;

        if !binary_reader.parse_nullspace() {
            return Err(corrupt("couldn't find PROJECTCONFIG block terminator"));
        }

        // ----- Target platforms ----------------------------------------
        if !binary_reader.parse_magic("TARGETPLATFORMS") {
            return Err(corrupt("couldn't find TARGETPLATFORMS block"));
        }

        let mut target_platforms = Vec::new();
        let target_platform_count = binary_reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated TARGETPLATFORMS count"))?;
        for _ in 0..target_platform_count {
            let raw = binary_reader
                .parse_u8()
                .ok_or_else(|| corrupt("truncated TARGETPLATFORMS entry"))?;
            target_platforms.push(
                decode_platform(raw)
                    .ok_or_else(|| corrupt("found unknown target in TARGETPLATFORMS block"))?,
            );
        }

        if !binary_reader.parse_nullspace() {
            return Err(corrupt("couldn't find TARGETPLATFORMS block terminator"));
        }

        // ----- App icon -------------------------------------------------
        if !binary_reader.parse_magic("APPICON") {
            return Err(corrupt("couldn't find APPICON block"));
        }

        let image_width = binary_reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated APPICON width"))?;
        let byte_length = binary_reader
            .parse_u32()
            .ok_or_else(|| corrupt("truncated APPICON byte length"))?;
        let image_bytes = binary_reader
            .parse_bytes(byte_length)
            .ok_or_else(|| corrupt("app icon bytes in APPICON block corrupt"))?;

        if !binary_reader.parse_nullspace() {
            return Err(corrupt("couldn't find APPICON block terminator"));
        }

        Ok(PekoProject {
            root: project_root.clone(),
            entry_file: project_root.join("main.peko"),
            name: project_name,
            incremental_info,
            ui_project_info: Some(UIProjectInfo::new(
                project_id,
                project_version,
                target_platforms,
                ProjectIcon::new(image_bytes, image_width),
            )),
        })
    }

    /// Serialize this project to its binary configuration format.
    pub fn to_binary(&self) -> Vec<u8> {
        let mut final_project_binary = Vec::new();

        // ----- Magic header --------------------------------------------
        final_project_binary.extend("PEKOPROJECT".bytes());
        final_project_binary.extend([0; 8]);

        // ----- Project config ------------------------------------------
        final_project_binary.extend("PROJECTCONFIG".bytes());
        final_project_binary.push(if self.ui_project_info.is_some() { 0 } else { 1 });

        final_project_binary.extend((self.name.len() as u32).to_be_bytes());
        final_project_binary.extend(self.name.bytes());

        let Some(ui_project_info) = self.ui_project_info.as_ref() else {
            return final_project_binary;
        };

        final_project_binary.extend((ui_project_info.bundle_id.len() as u32).to_be_bytes());
        final_project_binary.extend(ui_project_info.bundle_id.bytes());

        final_project_binary.extend((ui_project_info.version.len() as u32).to_be_bytes());
        final_project_binary.extend(ui_project_info.version.bytes());

        final_project_binary.extend([0; 8]);

        // ----- Target platforms ----------------------------------------
        final_project_binary.extend("TARGETPLATFORMS".bytes());
        final_project_binary.extend((ui_project_info.platforms.len() as u32).to_be_bytes());

        for platform in &ui_project_info.platforms {
            final_project_binary.push(encode_platform(platform));
        }

        final_project_binary.extend([0; 8]);

        // ----- App icon ------------------------------------------------
        final_project_binary.extend("APPICON".bytes());
        final_project_binary.extend(ui_project_info.icon.width.to_be_bytes());
        final_project_binary
            .extend((ui_project_info.icon.icon_pixel_data.len() as u32).to_be_bytes());
        final_project_binary.extend(&ui_project_info.icon.icon_pixel_data);

        final_project_binary.extend([0; 8]);

        final_project_binary
    }
}

/// Decode the on-disk numeric platform tag.
fn decode_platform(raw: u8) -> Option<peko_core::target::OperatingSystem> {
    use peko_core::target::OperatingSystem;
    match raw {
        0 => Some(OperatingSystem::Android),
        1 => Some(OperatingSystem::IOS),
        2 => Some(OperatingSystem::MacOS),
        3 => Some(OperatingSystem::Linux),
        4 => Some(OperatingSystem::Windows),
        _ => None,
    }
}

/// Encode an [`OperatingSystem`] into its on-disk numeric tag.
///
/// [`OperatingSystem`]: peko_core::target::OperatingSystem
fn encode_platform(platform: &peko_core::target::OperatingSystem) -> u8 {
    use peko_core::target::OperatingSystem;
    match platform {
        OperatingSystem::Android => 0,
        OperatingSystem::IOS => 1,
        OperatingSystem::MacOS => 2,
        OperatingSystem::Linux => 3,
        OperatingSystem::Windows => 4,
        OperatingSystem::Unknown => panic!("Shouldn't be here"),
    }
}

/// Recursively collect asset files under `dir` into `index`.
///
/// `root` is the asset directory the names are made relative to. Each regular
/// file is keyed by its path relative to `root` with forward-slash separators
/// and mapped to its absolute path. Subdirectories are walked in turn. Entries
/// that cannot be read are skipped.
fn collect_assets(root: &Path, dir: &Path, index: &mut BTreeMap<String, PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_assets(root, &path, index);
        } else if path.is_file() {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let name = relative
                .components()
                .filter_map(|component| component.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/");
            if !name.is_empty() {
                index.insert(name, path);
            }
        }
    }
}

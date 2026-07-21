//! Peko project metadata: discovery, the runtime project model, and app icon
//! rendering.
//!
//! A "Peko project" is a directory tree containing a `peko.toml` manifest that
//! names the project and, for UI applications, carries the bundle id, app
//! version, target platforms, and a square PNG app icon. Discovery loads the
//! manifest through [`peko_core::config::Manifest`] and builds a
//! [`PekoProject`] around it. The project root also holds a `.peko/incremental`
//! directory that caches per-file build artifacts between runs, and UI projects
//! carry an `assets/` directory whose files are the asset set.

use std::collections::BTreeMap;
use std::env::current_dir;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use derive_new::new;
use image::{DynamicImage, EncodableLayout};
use peko_core::config::{ConfigError, Icon, LoadedManifest, Manifest, Project, Ui, Windows};
use peko_core::target::OperatingSystem;
use thiserror::Error;

use crate::bundler::cartool::{carinfo, carwriter};
use crate::execution::incremental::ProjectIncrementalMap;

/// One failure mode for project loading.
#[derive(Debug, Error)]
pub enum ProjectError {
    /// No `peko.toml` was found in the searched directory or any parent within
    /// the search-depth limit.
    #[error("couldn't find a peko.toml in the current directory or its parents")]
    NotFound,

    /// The manifest was located but its entry source file is missing.
    #[error("couldn't find project entrypoint {0}")]
    MissingEntrypoint(PathBuf),

    /// The app icon could not be decoded, or is not square.
    #[error("couldn't load app icon at {path}: {detail}")]
    Icon { path: PathBuf, detail: String },

    /// The manifest failed to load or parse.
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// The toolchain-relative path of the default app icon blob.
const DEFAULT_ICON_BIN: &str = "Compiler/bundling/defaulticon.bin";

/// The pixel width and height of the default app icon.
const DEFAULT_ICON_SIZE: u32 = 1024;

/// The mask shape an app icon is trimmed to for a platform.
#[derive(Debug, Clone, Copy)]
pub enum IconShape {
    /// Full square, no mask.
    Square,
    /// A rounded rectangle.
    RoundedSquare,
    /// A full circle.
    Circle,
    /// An Apple-style squircle (superellipse).
    Squircle,
}

/// Whether a point (in content-box coordinates, each axis in [-1, 1]) is inside
/// `shape`.
fn shape_contains(shape: IconShape, x: f32, y: f32) -> bool {
    match shape {
        IconShape::Square => x.abs() <= 1.0 && y.abs() <= 1.0,
        IconShape::Circle => x * x + y * y <= 1.0,
        // The Apple icon shape is a rounded rectangle with continuous (squircle)
        // corners: flat sides that meet the canvas edges, joined by superellipse
        // corners. A pure superellipse pinches the sides inward and reads as too
        // round. `radius` is the corner size as a fraction of the half-side and
        // matches Apple's icon-grid corner (about 0.2237 of the full side). The
        // corner uses exponent 5 for the continuous curvature.
        IconShape::Squircle => {
            // Apple's icon corner radius is 0.2237 of the full side, which is
            // 0.4474 of the half-side used for these normalized coordinates.
            let radius = 0.4474f32;
            let ax = x.abs();
            let ay = y.abs();
            if ax > 1.0 || ay > 1.0 {
                false
            } else if ax <= 1.0 - radius || ay <= 1.0 - radius {
                true
            } else {
                let dx = (ax - (1.0 - radius)) / radius;
                let dy = (ay - (1.0 - radius)) / radius;
                dx.powf(5.0) + dy.powf(5.0) <= 1.0
            }
        }
        IconShape::RoundedSquare => {
            let radius = 0.45f32;
            let ax = x.abs();
            let ay = y.abs();
            if ax <= 1.0 - radius || ay <= 1.0 - radius {
                ax <= 1.0 && ay <= 1.0
            } else {
                let dx = ax - (1.0 - radius);
                let dy = ay - (1.0 - radius);
                dx * dx + dy * dy <= radius * radius
            }
        }
    }
}

/// App icon stored as RGBA pixel data plus the originating image width.
///
/// The image is square, so the width also gives the height.
#[derive(Debug, Clone, new)]
pub struct ProjectIcon {
    pub icon_pixel_data: Vec<u8>,
    pub width: u32,
}

impl ProjectIcon {
    /// Decode a square PNG app icon from disk into RGBA pixels.
    ///
    /// A non-square image is rejected. Any image format the `image` crate
    /// recognizes is accepted and converted to RGBA.
    pub fn from_png<P: AsRef<Path>>(path: P) -> Result<ProjectIcon, ProjectError> {
        let path = path.as_ref();
        let image = image::open(path).map_err(|source| ProjectError::Icon {
            path: path.to_path_buf(),
            detail: source.to_string(),
        })?;

        if image.width() != image.height() {
            return Err(ProjectError::Icon {
                path: path.to_path_buf(),
                detail: format!(
                    "app icon must be square, got {}x{}",
                    image.width(),
                    image.height()
                ),
            });
        }

        let width = image.width();
        Ok(ProjectIcon::new(image.into_rgba8().into_raw(), width))
    }

    /// Load the toolchain's default app icon.
    ///
    /// The default is a square block of BGRA pixels at
    /// `Compiler/bundling/defaulticon.bin` under the Peko root named by
    /// `PEKO_ROOT_PATH`. Its pixels are converted to RGBA, the same form a
    /// decoded PNG yields.
    pub fn default_icon() -> Result<ProjectIcon, ProjectError> {
        let Some(peko_root) = std::env::var_os("PEKO_ROOT_PATH") else {
            return Err(ProjectError::Icon {
                path: PathBuf::from(DEFAULT_ICON_BIN),
                detail: "PEKO_ROOT_PATH is not set, so the default app icon could not be found"
                    .to_owned(),
            });
        };

        ProjectIcon::from_bgra_bin(
            Path::new(&peko_root).join(DEFAULT_ICON_BIN),
            DEFAULT_ICON_SIZE,
        )
    }

    /// Load a square app icon from a raw BGRA pixel blob.
    ///
    /// The file holds `width * width * 4` bytes in BGRA order. The pixels are
    /// converted to RGBA.
    pub fn from_bgra_bin<P: AsRef<Path>>(path: P, width: u32) -> Result<ProjectIcon, ProjectError> {
        let path = path.as_ref();
        let mut pixels = std::fs::read(path).map_err(|source| ProjectError::Icon {
            path: path.to_path_buf(),
            detail: source.to_string(),
        })?;

        let expected = (width as usize) * (width as usize) * 4;
        if pixels.len() != expected {
            return Err(ProjectError::Icon {
                path: path.to_path_buf(),
                detail: format!(
                    "expected {expected} bytes for a {width}x{width} BGRA icon, found {}",
                    pixels.len()
                ),
            });
        }

        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Ok(ProjectIcon::new(pixels, width))
    }

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

    /// Returns the icon's pixel data in RGBA order.
    pub fn get_rgba_pixels(&self) -> Vec<u8> {
        self.icon_pixel_data.clone()
    }

    /// A copy shaped to `shape`, the way a platform displays its app icon. The
    /// content is inset by `inset` (a fraction of the side) on a transparent
    /// canvas, and everything outside the shape is made transparent with
    /// anti-aliased edges (2x2 supersampled). A square with no inset is a no-op.
    pub fn shaped(&self, shape: IconShape, inset: f32) -> ProjectIcon {
        if matches!(shape, IconShape::Square) && inset <= 0.0 {
            return self.clone();
        }
        let size = self.width;
        let content = (((size as f32) * (1.0 - 2.0 * inset)).round() as u32).max(1);
        let offset = ((size - content) / 2) as usize;
        let n = size as usize;
        let c = content as usize;

        // Draw the (optionally inset) source onto a transparent canvas.
        let source = if content == size {
            self.clone()
        } else {
            self.resize(content, content)
        };
        let mut canvas = vec![0u8; n * n * 4];
        for row in 0..c {
            let src = row * c * 4;
            let dst = ((row + offset) * n + offset) * 4;
            canvas[dst..dst + c * 4].copy_from_slice(&source.icon_pixel_data[src..src + c * 4]);
        }

        // Multiply each alpha by the shape's coverage over the content box.
        let half = content as f32 / 2.0;
        let center = size as f32 / 2.0;
        for y in 0..size {
            for x in 0..size {
                let mut inside = 0u32;
                for (sx, sy) in [(0.25f32, 0.25f32), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                    let px = (x as f32 + sx - center) / half;
                    let py = (y as f32 + sy - center) / half;
                    if shape_contains(shape, px, py) {
                        inside += 1;
                    }
                }
                if inside < 4 {
                    let alpha_index = (y as usize * n + x as usize) * 4 + 3;
                    canvas[alpha_index] = (canvas[alpha_index] as u32 * inside / 4) as u8;
                }
            }
        }
        ProjectIcon::new(canvas, size)
    }

    /// A copy shaped the way `os` displays its app icon: a squircle for Apple
    /// platforms (macOS with padding), a circle for Android, and square for the
    /// rest.
    pub fn shaped_for(&self, os: OperatingSystem) -> ProjectIcon {
        match os {
            // macOS shows the app icon as provided. macOS 26 no longer masks a
            // flat bitmap into the squircle, so the shape is baked in. The icon
            // is a full-bleed squircle: the flat sides meet the canvas edges and
            // only the corners are trimmed, matching Apple's icon shape.
            OperatingSystem::MacOS => self.shaped(IconShape::Squircle, 0.0),
            OperatingSystem::IOS => self.shaped(IconShape::Squircle, 0.0),
            OperatingSystem::Android => self.shaped(IconShape::Circle, 0.0),
            _ => self.clone(),
        }
    }

    /// Returns the icon's pixel data in BGRA order, the channel order the CAR
    /// renderer stores.
    fn bgra_pixels(&self) -> Vec<u8> {
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
                                iconx1024.bgra_pixels(),
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
                                iconx1024.bgra_pixels(),
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
            .write_all(
                carwriter::CarWriter::new(carbinary)
                    .create_binary()
                    .as_slice(),
            )
            .unwrap();
    }

    /// Writes this icon to Apple's CAR binary format for macOS.
    ///
    /// macOS renders the Dock and Finder app icon from a compiled asset
    /// catalog named by CFBundleIconName. The catalog carries one Icon Image
    /// rendition per size and scale (16 through 1024 pixels), so the system
    /// selects a full-resolution image that fills the tile at every size.
    /// A MultiSized Image rendition lists the five logical point sizes that
    /// tie the per-size renditions to the AppIcon facet.
    ///
    /// The key format has nine attributes in this order: appearance,
    /// localization, element, part, size, identifier, dimension2, layer,
    /// scale. Every rendition key is a nine value vector in that same order.
    /// Element 85 and identifier 6849 name the app icon; part 220 marks a
    /// per-size image and part 218 marks the multi-sized descriptor. The
    /// dimension2 value groups the two scales of one logical size (16pt is
    /// group 1, 32pt group 2, 128pt group 3, 256pt group 4, 512pt group 5).
    pub fn to_car_macos<W: Write + Seek>(&self, writer: &mut W) {
        // Little-endian byte vector from a run of u32 values, used to build
        // the fixed-shape TLV payloads.
        fn u32_bytes(values: &[u32]) -> Vec<u8> {
            let mut bytes = Vec::with_capacity(values.len() * 4);
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            bytes
        }

        // Build one standalone Icon Image rendition at the given pixel size
        // and scale, embedding its own lzfse pixel data. The dimension2 group
        // and scale place it in the rendition tree; the TLV payloads mirror
        // the layout the CAR renderer expects for a lone image bitmap.
        let image_rendition = |pixel_size: u32, scale: u32, dimension2: u16| {
            let resized = self.resize(pixel_size, pixel_size);
            let key = vec![0u16, 0, 85, 220, 0, 6849, dimension2, 0, scale as u16];
            let value = carinfo::ValueBlock::CSIData(carinfo::CSIData {
                width: pixel_size,
                height: pixel_size,
                scale: scale * 100,
                layout: 12,
                asset_name: "icon.png".to_owned(),
                tlv_entries: vec![
                    carinfo::TLVEntry {
                        tlv_type: 1001,
                        data: u32_bytes(&[1, 0, 0, pixel_size, pixel_size]),
                    },
                    carinfo::TLVEntry {
                        tlv_type: 1003,
                        data: u32_bytes(&[1, 0, 0, 0, 0, pixel_size, pixel_size]),
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
                        data: (pixel_size * 4).to_le_bytes().to_vec(),
                    },
                ],
                asset_data: Box::new(carinfo::ValueBlock::CELMImageData(
                    carinfo::CELMImageData::new(
                        resized.get_rgba_pixels(),
                        true,
                        pixel_size as usize,
                    ),
                )),
            });
            (key, value)
        };

        // The ten per-size, per-scale image renditions, plus the multi-sized
        // descriptor. Each logical point size supplies a 1x and a 2x image.
        let mut renditions = vec![
            image_rendition(16, 1, 1),
            image_rendition(32, 2, 1),
            image_rendition(32, 1, 2),
            image_rendition(64, 2, 2),
            image_rendition(128, 1, 3),
            image_rendition(256, 2, 3),
            image_rendition(256, 1, 4),
            image_rendition(512, 2, 4),
            image_rendition(512, 1, 5),
            image_rendition(1024, 2, 5),
        ];

        // The multi-sized descriptor lists the five logical point sizes and
        // the dimension2 group each maps to. Part 218 marks it as the icon's
        // main multi-sized entry.
        renditions.push((
            vec![0u16, 0, 85, 218, 0, 6849, 0, 0, 1],
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
                asset_data: Box::new(carinfo::ValueBlock::MultisizedImageList(vec![
                    carinfo::MSISSizeEntry {
                        width: 16,
                        height: 16,
                        index: 1,
                    },
                    carinfo::MSISSizeEntry {
                        width: 32,
                        height: 32,
                        index: 2,
                    },
                    carinfo::MSISSizeEntry {
                        width: 128,
                        height: 128,
                        index: 3,
                    },
                    carinfo::MSISSizeEntry {
                        width: 256,
                        height: 256,
                        index: 4,
                    },
                    carinfo::MSISSizeEntry {
                        width: 512,
                        height: 512,
                        index: 5,
                    },
                ])),
            }),
        ));

        // Order the tree entries by their rendition key. The renderer walks
        // the leaf in key order, so the multi-sized entry (part 218) precedes
        // the per-size images (part 220).
        renditions.sort_by(|left, right| left.0.cmp(&right.0));
        let (keys, values): (Vec<_>, Vec<_>) = renditions
            .into_iter()
            .map(|(key, value)| (carinfo::ValueBlock::RenditionKey(key), value))
            .unzip();

        let carbinary = carinfo::CarBinary::new(
            carinfo::CarHeader {
                coreui_version: 972,
                storage_version: 17,
                main_version_string: "@(#)PROGRAM:CoreUI  PROJECT:CoreUI-972.1".to_owned(),
                asset_storage_version_string:
                    "Xcode 26.3 (17C529) via AssetCatalogAgent-AssetRuntime".to_owned(),
            },
            carinfo::CarMetadata {
                deployment_platform_version: "11.0".to_owned(),
                deployment_platform: "macosx".to_owned(),
                authoring_tool:
                    "@(#)PROGRAM:CoreThemeDefinition  PROJECT:CoreThemeDefinition-653.3  [IIO-2784.3.4]"
                        .to_owned(),
            },
            vec![
                carinfo::KeyAttributeType::Appearance,
                carinfo::KeyAttributeType::Localization,
                carinfo::KeyAttributeType::Element,
                carinfo::KeyAttributeType::Part,
                carinfo::KeyAttributeType::Size,
                carinfo::KeyAttributeType::Identifier,
                carinfo::KeyAttributeType::Dimension2,
                carinfo::KeyAttributeType::Layer,
                carinfo::KeyAttributeType::Scale,
            ],
            carinfo::BomTree {
                block_name: Some("RENDITIONS".to_owned()),
                keys,
                values,
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
            .write_all(
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

    /// Writes this icon to a PNG with no alpha channel. Each pixel is
    /// composited over an opaque white background and the result is
    /// encoded as RGB. The App Store app icon rejects an alpha channel on
    /// the large app icon.
    pub fn to_png_opaque<W: Write + Seek>(&self, writer: &mut W) {
        let height = (self.icon_pixel_data.len() as u32 / 4) / self.width;
        let rgba = image::RgbaImage::from_raw(self.width, height, self.get_rgba_pixels()).unwrap();

        let mut rgb = image::RgbImage::new(self.width, height);
        for (x, y, pixel) in rgba.enumerate_pixels() {
            let [r, g, b, a] = pixel.0;
            let alpha = a as u32;
            let background = 255 - alpha;
            let blend =
                |channel: u8| -> u8 { ((channel as u32 * alpha + 255 * background) / 255) as u8 };
            rgb.put_pixel(x, y, image::Rgb([blend(r), blend(g), blend(b)]));
        }

        DynamicImage::ImageRgb8(rgb)
            .write_to(writer, image::ImageFormat::Png)
            .unwrap();
    }
    pub fn to_icns<W: Write + Seek>(&self, writer: &mut W) {
        let mut icns_family = icns::IconFamily::new();

        // Emit every icon size macOS expects. With only a single 256x256 image,
        // the Dock and Finder (which ask for 512 and 1024 on a Retina display)
        // fall back to drawing that small image inside a larger tile, which
        // reads as a shrunken icon on a blank backdrop. Providing each size lets
        // the system pick a full-resolution image that fills the tile.
        for size in [16u32, 32, 64, 128, 256, 512, 1024] {
            let resized = self.resize(size, size);
            let Ok(image) = icns::Image::from_data(
                icns::PixelFormat::RGBA,
                size,
                size,
                resized.get_rgba_pixels(),
            ) else {
                continue;
            };
            // add_icon maps the pixel size to the matching icns type; ignore a
            // size this icns build does not support rather than failing the
            // whole icon.
            let _ = icns_family.add_icon(&image);
        }

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
    /// The platform-assigned app id, absent until `peko link` writes one.
    pub app_id: Option<String>,
    /// The UI framework identifier: `native`, `static`, or `server`.
    pub framework: String,
    /// For a server app, which SSR framework it uses (`next`, `nuxt`, ...);
    /// `None` for native and static apps.
    pub server_framework: Option<String>,
    pub platforms: Vec<peko_core::target::OperatingSystem>,
    /// The base app icon, reworked for platforms without an explicit override.
    pub icon: ProjectIcon,
    /// Per-platform icon overrides from `[icon].<platform>`, replacing the base
    /// icon for that platform.
    pub icon_overrides: BTreeMap<peko_core::target::OperatingSystem, ProjectIcon>,
    /// The custom URL scheme the app registers for deep links, absent when it
    /// registers none.
    pub scheme: Option<String>,
    /// The initial window width in pixels from the manifest, absent for the
    /// default.
    pub width: Option<f64>,
    /// The initial window height in pixels from the manifest, absent for the
    /// default.
    pub height: Option<f64>,
    /// The Android adaptive-icon foreground layer, present when the icon builder
    /// saved a foreground/background split. Absent icons fall back to the flat
    /// masked icon on Android.
    pub android_foreground: Option<ProjectIcon>,
    /// The Android adaptive-icon background layer.
    pub android_background: Option<ProjectIcon>,
    /// The Windows Store identity name (`[windows].identity_name`), required to
    /// emit the release MSIX. Absent until the project declares it.
    pub windows_identity_name: Option<String>,
    /// The Windows Store publisher id (`[windows].publisher`, e.g. `CN=...`).
    pub windows_publisher: Option<String>,
    /// The Windows Store publisher display name (`[windows].publisher_display_name`).
    pub windows_publisher_display_name: Option<String>,
}

impl UIProjectInfo {
    /// The icon to bundle for a platform: its explicit override, else the base.
    pub fn icon_for(&self, os: peko_core::target::OperatingSystem) -> &ProjectIcon {
        self.icon_overrides.get(&os).unwrap_or(&self.icon)
    }

    /// The Android adaptive-icon foreground and background layers, present only
    /// when both were provided.
    pub fn android_adaptive(&self) -> Option<(&ProjectIcon, &ProjectIcon)> {
        match (&self.android_foreground, &self.android_background) {
            (Some(foreground), Some(background)) => Some((foreground, background)),
            _ => None,
        }
    }
}

/// A discovered or constructed Peko project.
#[derive(Debug, Clone)]
pub struct PekoProject {
    root: PathBuf,
    entry_file: PathBuf,
    pub incremental_info: Option<ProjectIncrementalMap>,

    pub name: String,
    /// The reverse-DNS bundle identifier from `[project].bundle`. Empty when
    /// unset or for a package manifest.
    pub identifier: String,
    /// The project version string from the manifest.
    pub version: String,
    /// The platform-assigned app id from `[project].app_id`, absent until
    /// `peko link` writes one.
    pub app_id: Option<String>,
    /// The platform serving host from `[project].host`
    /// (`<slug>.serve.pekoui.com`), absent until the app is deployed.
    pub host: Option<String>,
    pub ui_project_info: Option<UIProjectInfo>,
}

impl PekoProject {
    /// Construct a project from already-resolved fields.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        identifier: String,
        version: String,
        app_id: Option<String>,
        host: Option<String>,
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
            identifier,
            version,
            app_id,
            host,
            ui_project_info,
        }
    }

    /// The project's entrypoint source file.
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
    /// nearest ancestor directories.
    ///
    /// Discovery walks upward for a `peko.toml` through
    /// [`Manifest::discover`](peko_core::config::Manifest::discover).
    pub fn from_directory<P: AsRef<Path>>(directory: P) -> Result<PekoProject, ProjectError> {
        let loaded = match Manifest::discover(directory.as_ref()) {
            Ok(loaded) => loaded,
            Err(ConfigError::NotFound) => return Err(ProjectError::NotFound),
            Err(other) => return Err(ProjectError::Config(other)),
        };
        Self::from_loaded_manifest(loaded)
    }

    /// Build a project from a discovered manifest and its root.
    fn from_loaded_manifest(loaded: LoadedManifest) -> Result<PekoProject, ProjectError> {
        let root = loaded.root.clone();
        let entry_file = loaded.entry();
        // A server (SSR) app has no Peko entry file — it is a web framework app
        // the platform hosts — so it is exempt from the entrypoint requirement.
        let is_server = match &loaded.manifest {
            Manifest::Application(app) => app
                .ui
                .as_ref()
                .is_some_and(|ui| matches!(ui.framework, peko_core::config::Framework::Server)),
            Manifest::Package(_) => false,
        };
        if !is_server && !entry_file.exists() {
            return Err(ProjectError::MissingEntrypoint(entry_file));
        }

        let incremental_dir = root.join(".peko/incremental");
        let incremental_info =
            ProjectIncrementalMap::from_incremental_directory(&incremental_dir, true);
        if incremental_info.is_none() && incremental_dir.exists() {
            let _ = std::fs::remove_dir_all(&incremental_dir);
        }

        let ui_project_info = match &loaded.manifest {
            Manifest::Application(app) => match &app.ui {
                Some(ui) => Some(build_ui_info(
                    &root,
                    &app.project,
                    ui,
                    app.icon.as_ref(),
                    app.windows.as_deref(),
                )?),
                None => None,
            },
            Manifest::Package(_) => None,
        };

        // The bundle identifier and app id come from `[project]` and are
        // absent for a package manifest.
        let (identifier, app_id, host) = match &loaded.manifest {
            Manifest::Application(app) => (
                app.project.bundle.clone().unwrap_or_default(),
                app.project.app_id.clone(),
                app.project.host.clone(),
            ),
            Manifest::Package(_) => (String::new(), None, None),
        };

        Ok(PekoProject {
            name: loaded.manifest.name().to_owned(),
            identifier,
            version: loaded.manifest.version().to_string(),
            app_id,
            host,
            root,
            entry_file,
            incremental_info,
            ui_project_info,
        })
    }
}

/// Assemble UI metadata from an application manifest's `[project]` and `[ui]`
/// tables.
///
/// The app icon is loaded from the square PNG the `[icon].source` names, or
/// `[ui].icon` when there is no `[icon]` table, resolved relative to the project
/// root. A UI manifest without an icon falls back to the toolchain's default.
fn build_ui_info(
    root: &Path,
    project: &Project,
    ui: &Ui,
    icon_config: Option<&Icon>,
    windows: Option<&Windows>,
) -> Result<UIProjectInfo, ProjectError> {
    // A configured icon path that no longer exists falls back rather than
    // failing the whole project load: the icon is regenerated on the next save,
    // and a stale path should not brick opening or building the project. Only a
    // present-but-invalid PNG is a hard error.
    let load = |path: Option<&PathBuf>| -> Result<Option<ProjectIcon>, ProjectError> {
        match path {
            Some(path) => {
                let full = root.join(path);
                if full.exists() {
                    Ok(Some(ProjectIcon::from_png(full)?))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    };

    let source = icon_config
        .and_then(|config| config.source.as_ref())
        .or(ui.icon.as_ref());
    let icon = match load(source)? {
        Some(icon) => icon,
        None => ProjectIcon::default_icon()?,
    };

    // Per-platform overrides: each present PNG replaces the base for one OS.
    let mut icon_overrides = BTreeMap::new();
    if let Some(config) = icon_config {
        for (os, path) in &config.overrides {
            if let Some(icon) = load(Some(path))? {
                icon_overrides.insert(*os, icon);
            }
        }
    }

    // Android adaptive layers, present only when the icon builder saved a
    // foreground/background split.
    let android_foreground =
        load(icon_config.and_then(|config| config.android_foreground.as_ref()))?;
    let android_background =
        load(icon_config.and_then(|config| config.android_background.as_ref()))?;

    Ok(UIProjectInfo::new(
        project.bundle.clone().unwrap_or_default(),
        project.version.to_string(),
        project.app_id.clone(),
        ui.framework.as_str().to_owned(),
        ui.server_framework.map(|sf| sf.as_str().to_owned()),
        project.target_platforms.clone(),
        icon,
        icon_overrides,
        ui.scheme.clone(),
        ui.width,
        ui.height,
        android_foreground,
        android_background,
        windows.and_then(|w| w.identity_name.clone()),
        windows.and_then(|w| w.publisher.clone()),
        windows.and_then(|w| w.publisher_display_name.clone()),
    ))
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

#[cfg(test)]
mod car_tests {
    use super::*;
    use std::io::Cursor;

    /// The macOS CAR writer emits a well-formed asset catalog across every
    /// icon size. It exercises the small sizes (16 through 128) whose row
    /// count is below one compression chunk, guarding the chunker against a
    /// short-image regression.
    #[test]
    fn writes_macos_car() {
        let size = 1024u32;
        let mut pixels = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                pixels.extend_from_slice(&[
                    (x * 255 / size) as u8,
                    (y * 255 / size) as u8,
                    128,
                    255,
                ]);
            }
        }
        let icon = ProjectIcon::new(pixels, size);
        let mut buffer = Cursor::new(Vec::new());
        icon.to_car_macos(&mut buffer);

        let bytes = buffer.into_inner();
        // The BOM container magic marks a valid CAR, and the ten embedded
        // renditions make the catalog far larger than an empty header.
        assert_eq!(&bytes[..8], b"BOMStore");
        assert!(
            bytes.len() > 100_000,
            "car unexpectedly small: {}",
            bytes.len()
        );
    }
}

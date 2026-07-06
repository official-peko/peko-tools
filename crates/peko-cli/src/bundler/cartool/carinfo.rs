//! Data shapes for the CAR binary format.
//!
//! The types here mirror the on-disk structures used by Apple's compiled
//! asset catalog: a top-level [`CarBinary`] holding [`CarHeader`] /
//! [`CarMetadata`], plus three [`BomTree`]s ([`renditions`], [`facet_keys`],
//! [`appearance_keys`]) whose nodes carry [`ValueBlock`]s.
//!
//! Only the subset needed for embedding app icons is modelled, namely
//! [`CSIData`] entries pointing at [`CELMImageData`] (PNG-style raw image
//! data, lzfse-compressed in chunks) or [`MSISData`] (multi-sized image
//! set references).
//!
//! [`renditions`]: CarBinary::renditions
//! [`facet_keys`]: CarBinary::facet_keys
//! [`appearance_keys`]: CarBinary::appearance_keys

use std::collections::BTreeMap;

/// Token mapping attribute IDs to their values for a facet key node in
/// the FACETKEYS BomTree.
#[derive(Clone)]
pub struct FacetKeyToken {
    pub attributes: BTreeMap<u16, u16>,
}

/// Type-length-value entry attached to a CSI tag for extra metadata.
#[derive(Clone)]
pub struct TLVEntry {
    pub tlv_type: u32,
    pub data: Vec<u8>,
}

/// CSI ("CoreUI Storage Item"): the per-rendition asset data block.
#[derive(Clone)]
pub struct CSIData {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub layout: u16,
    pub asset_name: String,
    pub tlv_entries: Vec<TLVEntry>,
    pub asset_data: Box<ValueBlock>,
}

/// One BGRA8 pixel-data chunk inside a CELMImageData. Chunks are
/// individually lzfse-compressed.
#[derive(Clone)]
pub struct KCBCChunk {
    pub pixel_height: u32,
    pub compressed_pixels: Vec<u8>,
}

/// Raw image data stored as a sequence of lzfse-compressed pixel chunks
/// (the format CAR uses internally to represent "PNG" image data).
#[derive(Clone)]
pub struct CELMImageData {
    pub kcbc_chunks: Vec<KCBCChunk>,
}

/// Number of full-height slices a rendition is split into before the
/// remainder. CoreUI's icon bitmap expander tiles a rendition into this
/// many slices and rejects a slice taller than the tile height it computes,
/// so the chunker matches that tiling.
const CHUNK_SLICE_COUNT: usize = 3;

/// Bytes per pixel in the CAR's BGRA8 storage format.
const PIXEL_BYTE_WIDTH: usize = 4;

/// Swap each pixel's red and blue channels in place. Used to convert
/// RGBA8 to BGRA8 (or vice versa) since CAR stores pixels in BGRA8 but
/// PNG decoders typically produce RGBA8.
fn swap_red_and_blue(mut buffer: Vec<u8>) -> Vec<u8> {
    for pixel in buffer.chunks_exact_mut(PIXEL_BYTE_WIDTH) {
        pixel.swap(0, 2);
    }
    buffer
}

impl CELMImageData {
    /// Build a CELMImageData from raw image bytes.
    ///
    /// When `bytes_rgba` is true, the input is interpreted as RGBA8 and
    /// converted to BGRA8 in-place before chunking. When false, the input
    /// is assumed to already be BGRA8.
    pub fn new(image_bytes: Vec<u8>, bytes_rgba: bool, image_width: usize) -> CELMImageData {
        let mut bgra_image_bytes = if bytes_rgba {
            swap_red_and_blue(image_bytes)
        } else {
            image_bytes
        };

        // Slice the image into strips of `height / CHUNK_SLICE_COUNT` rows
        // each, matching the tile height CoreUI computes for the rendition.
        // Greedy filling leaves a shorter final strip for the remainder rows.
        let mut chunk_data: Vec<Vec<u8>> = Vec::new();
        let mut chunk_heights: Vec<usize> = Vec::new();
        let bytes_per_row = image_width * PIXEL_BYTE_WIDTH;
        let total_rows = bgra_image_bytes.len() / bytes_per_row;
        let slice_rows = (total_rows / CHUNK_SLICE_COUNT).max(1);

        while !bgra_image_bytes.is_empty() {
            let remaining_rows = bgra_image_bytes.len() / bytes_per_row;
            let chunk_height = remaining_rows.min(slice_rows);
            let current_chunk_byte_count = chunk_height * bytes_per_row;

            // Split off the current chunk from the front of the buffer.
            let image_split = bgra_image_bytes.split_off(current_chunk_byte_count);
            chunk_heights.push(chunk_height);
            chunk_data.push(bgra_image_bytes);
            bgra_image_bytes = image_split;
        }

        // Encode each chunk with lzfse and trim trailing zero padding.
        let mut kcbc_chunks = Vec::new();
        for (height, chunk) in chunk_heights.iter().zip(chunk_data) {
            let mut encoded_data = vec![0; chunk.len() + 12];
            let _encoded_size = lzfse::encode_buffer(chunk.as_slice(), &mut encoded_data).unwrap();

            // Strip the trailing zeroes lzfse left in the over-sized buffer.
            while encoded_data.last() == Some(&0) {
                encoded_data.pop();
            }

            kcbc_chunks.push(KCBCChunk {
                pixel_height: *height as u32,
                compressed_pixels: encoded_data,
            });
        }

        CELMImageData { kcbc_chunks }
    }
}

/// Multi-sized image set data: a reference to a sibling image rendition
/// rather than embedded pixel data.
#[derive(Clone)]
pub struct MSISData {
    pub idiom: u32,
    pub scale: u32,
    pub width: u32,
    pub height: u32,
    pub reference_index: u32,
}

/// One logical size entry in a multi-sized image list. `width` and `height`
/// are point sizes; `index` is the dimension2 group the matching per-size
/// renditions carry.
#[derive(Clone)]
pub struct MSISSizeEntry {
    pub width: u32,
    pub height: u32,
    pub index: u32,
}

/// One value-block entry in the CAR's value table. The variant determines
/// both the on-disk encoding and the byte length [`get_byte_length`]
/// reports.
///
/// [`get_byte_length`]: ValueBlock::get_byte_length
#[derive(Clone)]
pub enum ValueBlock {
    /// Rendition key into the renditions BomTree.
    RenditionKey(Vec<u16>),
    /// Asset-data block referenced from a rendition key.
    CSIData(CSIData),
    /// Plain string (used by FACETKEYS and APPEARANCEKEYS trees).
    String(String),
    /// 16-bit integer (used by appearance values).
    Int(u16),
    /// Facet-key token (used by FACETKEYS tree values).
    FacetKeyToken(FacetKeyToken),
    /// Raw lzfse-compressed BGRA8 pixel data.
    CELMImageData(CELMImageData),
    /// Multi-sized image set reference.
    MultisizedImageSetData(MSISData),
    /// Multi-sized image list: an inline table of logical sizes, each
    /// naming the dimension2 group its per-size renditions belong to.
    MultisizedImageList(Vec<MSISSizeEntry>),
}

impl ValueBlock {
    /// On-disk byte length of this block once serialized.
    pub fn get_byte_length(&self) -> u32 {
        let len = match self {
            Self::RenditionKey(key_data) => key_data.len() * 2,
            Self::CSIData(csidata) => {
                184 + csidata.tlv_entries.len() * 8
                    + csidata
                        .tlv_entries
                        .iter()
                        .map(|entry| entry.data.len())
                        .sum::<usize>()
                    + csidata.asset_data.get_byte_length() as usize
            }
            Self::String(string) => string.len(),
            Self::Int(_) => 2,
            Self::FacetKeyToken(facet_token) => 6 + 4 * facet_token.attributes.len(),
            Self::CELMImageData(celm_data) => {
                16 + 20 * celm_data.kcbc_chunks.len()
                    + celm_data
                        .kcbc_chunks
                        .iter()
                        .map(|chunk| chunk.compressed_pixels.len())
                        .sum::<usize>()
            }
            Self::MultisizedImageSetData(_) => 28,
            // Magic + version + count, then 12 bytes per size entry.
            Self::MultisizedImageList(entries) => 12 + 12 * entries.len(),
        };
        len as u32
    }
}

/// A BOM (Bill Of Materials) tree, the CAR's key/value indexing
/// structure. Each CAR file contains three: RENDITIONS, FACETKEYS, and
/// APPEARANCEKEYS.
#[derive(Clone)]
pub struct BomTree {
    pub block_name: Option<String>,
    pub keys: Vec<ValueBlock>,
    pub values: Vec<ValueBlock>,
    pub block_size: u32,
}

// ---------------------------------------------------------------------------
// CarBinary structures
// ---------------------------------------------------------------------------

/// CAR file header: version info and tool identification strings.
pub struct CarHeader {
    pub coreui_version: u32,
    pub storage_version: u32,
    pub main_version_string: String,
    pub asset_storage_version_string: String,
}

/// CAR file metadata: deployment target and authoring-tool strings.
pub struct CarMetadata {
    pub deployment_platform_version: String,
    pub deployment_platform: String,
    pub authoring_tool: String,
}

/// Attribute IDs used when constructing rendition key values.
///
/// The numeric discriminants mirror the on-disk attribute IDs; do not
/// renumber.
#[derive(Clone, Copy)]
pub enum KeyAttributeType {
    Element = 1,
    Part = 2,
    Size = 3,
    Direction = 4,
    Placeholder = 5,
    Value = 6,
    Appearance = 7,
    Dimension1 = 8,
    Dimension2 = 9,
    State = 10,
    Layer = 11,
    Scale = 12,
    Localization = 13,
    PresentationState = 14,
    Idiom = 15,
    Subtype = 16,
    Identifier = 17,
    PreviousValue = 18,
    PreviousState = 19,
    SizeClassHorizontal = 20,
    SizeClassVertical = 21,
    MemoryClass = 22,
    GraphicsClass = 23,
    DisplayGamut = 24,
    DeploymentTarget = 25,
    GlyphWeight = 26,
    GlyphSize = 27,
}

/// High-level shape of a complete CAR file, ready for serialization by
/// [`crate::bundler::cartool::carwriter::CarWriter`].
pub struct CarBinary {
    pub header: CarHeader,
    pub metadata: CarMetadata,
    pub key_format: Vec<KeyAttributeType>,

    pub renditions: BomTree,
    pub facet_keys: BomTree,
    pub appearance_keys: BomTree,
}

impl CarBinary {
    pub fn new(
        header: CarHeader,
        metadata: CarMetadata,
        key_format: Vec<KeyAttributeType>,
        renditions: BomTree,
        facet_keys: BomTree,
        appearance_keys: BomTree,
    ) -> CarBinary {
        CarBinary {
            header,
            metadata,
            key_format,
            renditions,
            facet_keys,
            appearance_keys,
        }
    }
}

#[cfg(test)]
mod celm_tests {
    use super::*;

    /// The CELM chunker slices every image into `height / 3` row strips.
    /// CoreUI's icon bitmap expander tiles a rendition into thirds and aborts
    /// on a slice taller than that tile, so no chunk may exceed the slice
    /// height, and the chunk heights must sum to the full image.
    #[test]
    fn chunks_match_the_third_height_tiling() {
        for size in [16usize, 32, 64, 128, 256, 512, 1024] {
            let pixels = vec![0u8; size * size * PIXEL_BYTE_WIDTH];
            let celm = CELMImageData::new(pixels, false, size);

            let slice_rows = (size / CHUNK_SLICE_COUNT).max(1);
            let total: u32 = celm.kcbc_chunks.iter().map(|chunk| chunk.pixel_height).sum();
            assert_eq!(total as usize, size, "chunk heights must cover {size} rows");
            for chunk in &celm.kcbc_chunks {
                assert!(
                    chunk.pixel_height as usize <= slice_rows,
                    "size {size}: chunk height {} exceeds slice height {slice_rows}",
                    chunk.pixel_height
                );
            }
        }
    }
}

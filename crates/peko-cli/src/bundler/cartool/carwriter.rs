//! Serializer that turns a [`carinfo::CarBinary`] into its on-disk byte
//! representation.
//!
//! The CAR file format starts with a 512-byte BOM (Bill Of Materials)
//! header pointing at a block-offsets table and a variable-names table.
//! Each named block (`CARHEADER`, `KEYFORMAT`, `EXTENDED_METADATA`,
//! `RENDITIONS`, `FACETKEYS`, `APPEARANCEKEYS`) sits in the file body and
//! is registered both in the block-offsets table (by index) and the
//! variable-names table (by name).
//!
//! [`CarWriter::create_binary`] orchestrates the whole serialization, then
//! goes back to patch the offset/length values it couldn't know in
//! advance.

use indexmap::IndexMap;

use crate::bundler::cartool::carinfo;

/// Reference to one block in the file's body, recorded in the block
/// offsets table.
#[derive(Clone)]
struct BomBlock {
    pub name: Option<String>,
    pub byte_offset: u32,
    pub byte_count: u32,
}

/// Serialize a CAR data structure into the writer's byte buffer.
/// Returns the block-table index of the created root block (or 0 for
/// kinds that don't produce a block, e.g. image data).
pub trait WriteToCar {
    fn write_to_car(&self, writer: &mut CarWriter) -> u32;
}

/// State accumulator + helpers for building the CAR binary byte-by-byte.
pub struct CarWriter {
    car_binary: carinfo::CarBinary,
    blocks: Vec<BomBlock>,
    named_block_indicies: IndexMap<String, u32>,

    binary: Vec<u8>,
}

impl CarWriter {
    pub fn new(binary: carinfo::CarBinary) -> CarWriter {
        CarWriter {
            car_binary: binary,
            // Block index 0 is always a zero placeholder.
            blocks: vec![BomBlock {
                name: None,
                byte_offset: 0,
                byte_count: 0,
            }],
            binary: Vec::new(),
            named_block_indicies: IndexMap::new(),
        }
    }

    /// Build and return the full CAR binary byte stream.
    pub fn create_binary(&mut self) -> Vec<u8> {
        // 1. BOM header: magic, version, then placeholder slots for the
        //    block count, block-offsets table location, and variable-names
        //    table location. These are patched in step 6 once the actual
        //    locations are known.
        self.write_bytes(b"BOMStore");
        self.write_bytes(&1u32.to_be_bytes());

        let blocks_count_index = self.current_offset();
        self.write_bytes(&[0; 4]);

        let block_var_list_offsets_index = self.current_offset();
        self.write_bytes(&[0; 16]);

        // BOMHeader must be exactly 512 bytes, padded with zeroes.
        self.write_bytes(&[0; 480]);

        // 2. The four named body blocks, in order.
        self.write_car_header();

        let renditions = self.car_binary.renditions.clone();
        renditions.write_to_car(self);

        let facetkeys = self.car_binary.facet_keys.clone();
        facetkeys.write_to_car(self);

        let appearancekeys = self.car_binary.appearance_keys.clone();
        appearancekeys.write_to_car(self);

        self.write_key_format();
        self.write_extended_metadata();

        // 3. Variable-names table: maps each named block's name to its
        //    index in the block-offsets table.
        let variable_table_index = self.current_offset();
        let variable_table_length = self.write_variable_table();

        // 4. Block-offsets table: one (offset, length) pair per block in
        //    the file body.
        let block_table_index = self.current_offset();
        let block_table_length = self.write_block_offsets_table();

        // 5. Patch the BOM header's placeholder slots with the real values.
        self.set_bytes(
            blocks_count_index as usize,
            &(self.blocks.len() as u32).to_be_bytes(),
        );
        self.set_bytes(
            block_var_list_offsets_index as usize,
            &block_table_index.to_be_bytes(),
        );
        self.set_bytes(
            block_var_list_offsets_index as usize + 4,
            &block_table_length.to_be_bytes(),
        );
        self.set_bytes(
            block_var_list_offsets_index as usize + 8,
            &variable_table_index.to_be_bytes(),
        );
        self.set_bytes(
            block_var_list_offsets_index as usize + 12,
            &variable_table_length.to_be_bytes(),
        );

        self.binary.clone()
    }

    /// Append one byte to the binary buffer.
    pub fn write_byte(&mut self, byte: u8) {
        self.binary.push(byte);
    }

    /// Append a slice of bytes to the binary buffer.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.binary.extend_from_slice(bytes);
    }

    /// Overwrite one byte at a previously-recorded offset.
    pub fn set_byte(&mut self, byte_index: usize, byte: u8) {
        self.binary[byte_index] = byte;
    }

    /// Overwrite a sequence of bytes starting at `byte_index`. Used to
    /// patch placeholder values once their real values are known.
    pub fn set_bytes(&mut self, byte_index: usize, bytes: &[u8]) {
        self.binary[byte_index..byte_index + bytes.len()].copy_from_slice(bytes);
    }

    /// Current write cursor position in the binary.
    pub fn current_offset(&self) -> u32 {
        self.binary.len() as u32
    }

    fn add_block(&mut self, block: &BomBlock) {
        if let Some(name) = &block.name {
            self.named_block_indicies
                .insert(name.clone(), self.blocks.len() as u32);
        }
        self.blocks.push(block.clone());
    }

    pub fn get_block_count(&self) -> u32 {
        self.blocks.len() as u32
    }

    // -- Body-block writers ------------------------------------------------

    /// CAR header (little endian):
    /// magic ("CTAR" reversed as "RATC"), coreui version, storage version,
    /// timestamp (0), rendition count, main version string (128 bytes,
    /// zero-padded), asset storage version string (256 bytes, zero-padded),
    /// uuid (0; u128), checksum (0; u32), schema version (2), color space
    /// id (1), key semantics (2).
    fn write_car_header(&mut self) {
        let carheader_start = self.current_offset();

        self.write_bytes(b"RATC");
        self.write_bytes(&self.car_binary.header.coreui_version.to_le_bytes());
        self.write_bytes(&self.car_binary.header.storage_version.to_le_bytes());

        // Storage timestamp, always zero.
        self.write_bytes(&[0; 4]);

        self.write_bytes(&(self.car_binary.renditions.keys.len() as u32).to_le_bytes());

        self.write_padded_string(&self.car_binary.header.main_version_string.clone(), 128);
        self.write_padded_string(
            &self.car_binary.header.asset_storage_version_string.clone(),
            256,
        );

        self.write_bytes(&0u128.to_le_bytes());
        self.write_bytes(&0u32.to_le_bytes());

        self.write_bytes(&2u32.to_le_bytes());
        self.write_bytes(&1u32.to_le_bytes());
        self.write_bytes(&2u32.to_le_bytes());

        self.add_block(&BomBlock {
            name: Some("CARHEADER".to_owned()),
            byte_offset: carheader_start,
            byte_count: self.current_offset() - carheader_start,
        });
    }

    /// Key-format block (little endian):
    /// magic ("kfmt" reversed as "tmfk"), version (0), token count, tokens.
    fn write_key_format(&mut self) {
        let keyformat_start = self.current_offset();
        self.write_bytes(b"tmfk");
        self.write_bytes(&[0; 4]);

        self.write_bytes(&(self.car_binary.key_format.len() as u32).to_le_bytes());

        let key_format = self.car_binary.key_format.clone();
        for key_token in &key_format {
            self.write_bytes(&(*key_token as u32).to_le_bytes());
        }

        self.add_block(&BomBlock {
            name: Some("KEYFORMAT".to_owned()),
            byte_offset: keyformat_start,
            byte_count: self.current_offset() - keyformat_start,
        });
    }

    /// Extended metadata block (big endian):
    /// magic ("META"), 256 bytes of thinning arguments (zeroes), deployment
    /// platform version (256 bytes, zero-padded), deployment platform
    /// (256 bytes, zero-padded), authoring tool (256 bytes, zero-padded).
    fn write_extended_metadata(&mut self) {
        let metadata_start = self.current_offset();
        self.write_bytes(b"META");
        self.write_bytes(&[0; 256]);

        self.write_padded_string(
            &self.car_binary.metadata.deployment_platform_version.clone(),
            256,
        );
        self.write_padded_string(&self.car_binary.metadata.deployment_platform.clone(), 256);
        self.write_padded_string(&self.car_binary.metadata.authoring_tool.clone(), 256);

        self.add_block(&BomBlock {
            name: Some("EXTENDED_METADATA".to_owned()),
            byte_offset: metadata_start,
            byte_count: self.current_offset() - metadata_start,
        });
    }

    /// Variable-names table (big endian):
    /// count of named blocks, then `{ block_index: u32, name_len: u8,
    /// name: u8[name_len] }`. Returns the table's length in bytes.
    fn write_variable_table(&mut self) -> u32 {
        let start = self.current_offset();
        self.write_bytes(&(self.named_block_indicies.len() as u32).to_be_bytes());

        let entries = self.named_block_indicies.clone();
        for (block_name, block_idx) in entries {
            self.write_bytes(&block_idx.to_be_bytes());
            self.write_byte(block_name.len() as u8);
            self.write_bytes(block_name.as_bytes());
        }
        self.current_offset() - start
    }

    /// Block offsets table (big endian):
    /// block count, then `{ offset: u32, size: u32 }` per block, then a
    /// free-list count. Returns the table's length in bytes.
    fn write_block_offsets_table(&mut self) -> u32 {
        let start = self.current_offset();
        self.write_bytes(&(self.blocks.len() as u32).to_be_bytes());

        let blocks = self.blocks.clone();
        for block in blocks {
            self.write_bytes(&block.byte_offset.to_be_bytes());
            self.write_bytes(&block.byte_count.to_be_bytes());
        }

        // Block-index free list. The index ends with a free-list count and
        // that many free entries. The count is zero.
        self.write_bytes(&0u32.to_be_bytes());

        self.current_offset() - start
    }

    /// Write `text` as bytes followed by zero-padding so the total written
    /// is exactly `field_width` bytes. The text is assumed to fit.
    fn write_padded_string(&mut self, text: &str, field_width: usize) {
        self.write_bytes(text.as_bytes());
        for _ in text.len()..field_width {
            self.write_byte(0);
        }
    }
}

// ---------------------------------------------------------------------------
// WriteToCar implementations
// ---------------------------------------------------------------------------

impl WriteToCar for carinfo::ValueBlock {
    fn write_to_car(&self, writer: &mut CarWriter) -> u32 {
        let value_start = writer.current_offset();

        match self {
            carinfo::ValueBlock::RenditionKey(key_values) => {
                // Keys are stored little-endian.
                for key in key_values {
                    writer.write_bytes(&key.to_le_bytes());
                }
            }

            carinfo::ValueBlock::CSIData(csidata) => {
                // Magic "ISTC" (= "CTSI" in CAR-endian).
                writer.write_bytes(b"ISTC");

                // CSI version (1 as u32, little endian).
                writer.write_bytes(&1u32.to_le_bytes());

                // Rendition flags (0).
                writer.write_bytes(&[0; 4]);

                // Width, height, scale.
                writer.write_bytes(&csidata.width.to_le_bytes());
                writer.write_bytes(&csidata.height.to_le_bytes());
                writer.write_bytes(&csidata.scale.to_le_bytes());

                // Pixel format "ARGB" stored little-endian (effectively BGRA).
                writer.write_bytes(b"BGRA");

                // Color space (1, little endian).
                writer.write_bytes(&1u32.to_le_bytes());

                // Modification timestamp - always 0.
                writer.write_bytes(&[0; 4]);

                // Layout (u16) + 2 trailing zero bytes.
                writer.write_bytes(&csidata.layout.to_le_bytes());
                writer.write_bytes(&[0; 2]);

                // Asset filename - 128-byte field, zero-padded.
                writer.write_bytes(csidata.asset_name.as_bytes());
                for _ in csidata.asset_name.len()..128 {
                    writer.write_byte(0);
                }

                // TLV length: 8 bytes header per entry + sum of payload lengths.
                let tlv_length: usize = 8 * csidata.tlv_entries.len()
                    + csidata
                        .tlv_entries
                        .iter()
                        .map(|entry| entry.data.len())
                        .sum::<usize>();
                writer.write_bytes(&(tlv_length as u32).to_le_bytes());

                // Bitmap count - always 1.
                writer.write_bytes(&1u32.to_le_bytes());

                // Reserved - always 0.
                writer.write_bytes(&0u32.to_le_bytes());

                // Rendition payload length.
                writer.write_bytes(&csidata.asset_data.get_byte_length().to_le_bytes());

                // TLV entries.
                for entry in &csidata.tlv_entries {
                    writer.write_bytes(&entry.tlv_type.to_le_bytes());
                    writer.write_bytes(&(entry.data.len() as u32).to_le_bytes());
                    writer.write_bytes(&entry.data);
                }

                // Recurse into the embedded asset_data block.
                csidata.asset_data.as_ref().write_to_car(writer);
            }

            carinfo::ValueBlock::String(key_name) => {
                writer.write_bytes(key_name.as_bytes());
            }

            carinfo::ValueBlock::Int(key_value) => {
                writer.write_bytes(&key_value.to_le_bytes());
            }

            carinfo::ValueBlock::FacetKeyToken(key_token) => {
                // Cursor hotspot data (4 zero bytes).
                writer.write_bytes(&[0; 4]);

                // Attribute count.
                writer.write_bytes(&(key_token.attributes.len() as u16).to_le_bytes());

                // Attribute (id, value) pairs.
                for (attribute_id, attribute_value) in &key_token.attributes {
                    writer.write_bytes(&attribute_id.to_le_bytes());
                    writer.write_bytes(&attribute_value.to_le_bytes());
                }
            }

            carinfo::ValueBlock::CELMImageData(celm_data) => {
                // Magic "MLEC" (= "CELM" in CAR-endian).
                writer.write_bytes(b"MLEC");
                // Flags = 3.
                writer.write_bytes(&3u32.to_le_bytes());
                // Compression type = 4 (lzfse).
                writer.write_bytes(&4u32.to_le_bytes());
                // "Length" of data = 4.
                writer.write_bytes(&4u32.to_le_bytes());

                for chunk in &celm_data.kcbc_chunks {
                    // Magic "KCBC".
                    writer.write_bytes(b"KCBC");
                    // Reserved (8 zero bytes).
                    writer.write_bytes(&[0; 8]);
                    // Pixel height.
                    writer.write_bytes(&chunk.pixel_height.to_le_bytes());
                    // Chunk payload length.
                    writer.write_bytes(&(chunk.compressed_pixels.len() as u32).to_le_bytes());
                    // Payload.
                    writer.write_bytes(&chunk.compressed_pixels);
                }
            }

            carinfo::ValueBlock::MultisizedImageSetData(msis_data) => {
                // Magic "SISM" (= "MSIS" in CAR-endian).
                writer.write_bytes(b"SISM");

                writer.write_bytes(&msis_data.idiom.to_le_bytes());
                writer.write_bytes(&msis_data.scale.to_le_bytes());
                writer.write_bytes(&msis_data.width.to_le_bytes());
                writer.write_bytes(&msis_data.height.to_le_bytes());
                writer.write_bytes(&msis_data.reference_index.to_le_bytes());

                // 4 padding bytes.
                writer.write_bytes(&[0; 4]);
            }

            carinfo::ValueBlock::MultisizedImageList(entries) => {
                // Magic "SISM" (= "MSIS" in CAR-endian).
                writer.write_bytes(b"SISM");

                // Version (1) and size-entry count.
                writer.write_bytes(&1u32.to_le_bytes());
                writer.write_bytes(&(entries.len() as u32).to_le_bytes());

                // Each entry: point width, point height, dimension2 group.
                for entry in entries {
                    writer.write_bytes(&entry.width.to_le_bytes());
                    writer.write_bytes(&entry.height.to_le_bytes());
                    writer.write_bytes(&entry.index.to_le_bytes());
                }
            }
        }

        // Image-data blocks are embedded inline within their parent CSI's
        // block, so they don't get their own block-table entry.
        match self {
            Self::CELMImageData(_)
            | Self::MultisizedImageSetData(_)
            | Self::MultisizedImageList(_) => 0,
            _ => {
                writer.add_block(&BomBlock {
                    name: None,
                    byte_offset: value_start,
                    byte_count: writer.current_offset() - value_start,
                });
                writer.get_block_count() - 1
            }
        }
    }
}

impl WriteToCar for carinfo::BomTree {
    fn write_to_car(&self, writer: &mut CarWriter) -> u32 {
        let tree_entry_start = writer.current_offset();
        let root_block_index = writer.get_block_count();

        // Magic "tree".
        writer.write_bytes(b"tree");
        // Tree version (always 1, big endian).
        writer.write_bytes(&1u32.to_be_bytes());

        // Placeholder for the tree-header block pointer; patched after the
        // header is laid out below.
        let tree_header_pointer_index = writer.current_offset();
        writer.write_bytes(&[0; 4]);

        // Tree block size.
        writer.write_bytes(&self.block_size.to_be_bytes());

        // Path count + 1 byte of unknown data.
        writer.write_bytes(&(self.keys.len() as u32).to_be_bytes());
        writer.write_byte(0);

        writer.add_block(&BomBlock {
            name: self.block_name.clone(),
            byte_offset: tree_entry_start,
            byte_count: writer.current_offset() - tree_entry_start,
        });

        // Tree header.
        let tree_header_start = writer.current_offset();
        let tree_header_block_index = writer.get_block_count();

        // Patch the tree-header pointer we left a placeholder for.
        writer.set_bytes(
            tree_header_pointer_index as usize,
            &tree_header_block_index.to_be_bytes(),
        );

        // isleaf = yes (u16 big-endian 1 stored as [0, 1]).
        writer.write_bytes(&[0, 1]);

        // Key count (u16).
        writer.write_bytes(&(self.keys.len() as u16).to_be_bytes());

        // Forward + backward sibling pointers (8 zero bytes).
        writer.write_bytes(&[0; 8]);

        // Make room for key-value pointer pairs (8 bytes each: key u32 +
        // value u32). They're patched in below as each pair gets written.
        let key_value_start = writer.current_offset();
        for _ in 0..self.keys.len() {
            writer.write_bytes(&[0; 8]);
        }

        // Pad the paths block out to block_size. The tree reader reads the
        // full block_size span for each paths block, so the block occupies
        // that many bytes in the file.
        let header_written = writer.current_offset() - tree_header_start;
        for _ in header_written..self.block_size {
            writer.write_byte(0);
        }

        writer.add_block(&BomBlock {
            name: None,
            byte_offset: tree_header_start,
            byte_count: writer.current_offset() - tree_header_start,
        });

        // Now emit each key/value pair, patching the pointer table as we go.
        for (kv_idx, (key, value)) in self.keys.iter().zip(self.values.iter()).enumerate() {
            let key_block_index = key.write_to_car(writer);
            let value_block_index = value.write_to_car(writer);

            // Layout per entry: value pointer first, then key pointer.
            let entry_offset = key_value_start as usize + kv_idx * 8;
            writer.set_bytes(entry_offset, &value_block_index.to_be_bytes());
            writer.set_bytes(entry_offset + 4, &key_block_index.to_be_bytes());
        }

        root_block_index
    }
}

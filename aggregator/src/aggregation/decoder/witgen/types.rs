use std::{
    collections::BTreeMap,
    io::Cursor,
};

use bitstream_io::{BitRead, BitReader, LittleEndian};
use eth_types::Field;
use gadgets::impl_expr;
use halo2_proofs::{circuit::Value, plonk::Expression};
use itertools::Itertools;
use strum_macros::EnumIter;

use super::{
    params::N_BITS_PER_BYTE,
    util::{read_variable_bit_packing, smaller_powers_of_two, value_bits_le},
};

// witgen_debug
// use std::{
//     // io, 
//     // witgen_debug
//     // io::Write
// };

/// A read-only memory table (fixed table) for decompression circuit to verify that the next tag
/// fields are assigned correctly.
#[derive(Clone, Debug)]
pub struct RomTagTableRow {
    /// The current tag.
    tag: ZstdTag,
    /// The tag that will be processed after the current tag is finished processing.
    tag_next: ZstdTag,
    /// The maximum number of bytes that are needed to represent the current tag.
    max_len: u64,
    /// Whether this tag outputs a decoded byte or not.
    is_output: bool,
    /// Whether this tag is processed from back-to-front or not.
    is_reverse: bool,
    /// Whether this tag belongs to a ``block`` in zstd or not.
    is_block: bool,
}

impl RomTagTableRow {
    pub(crate) fn rows() -> Vec<Self> {
        use ZstdTag::{
            BlockHeader, FrameContentSize, FrameHeaderDescriptor, ZstdBlockLiteralsHeader,
            ZstdBlockLiteralsRawBytes, ZstdBlockSequenceHeader,
        };

        [
            (FrameHeaderDescriptor, FrameContentSize, 1),
            (FrameContentSize, BlockHeader, 8),
            (BlockHeader, ZstdBlockLiteralsHeader, 3),
            (ZstdBlockLiteralsHeader, ZstdBlockLiteralsRawBytes, 5),
            (ZstdBlockLiteralsRawBytes, ZstdBlockSequenceHeader, 1048575), // (1 << 20) - 1
        ]
        .map(|(tag, tag_next, max_len)| Self {
            tag,
            tag_next,
            max_len,
            is_output: tag.is_output(),
            is_reverse: tag.is_reverse(),
            is_block: tag.is_block(),
        })
        .to_vec()
    }

    pub(crate) fn values<F: Field>(&self) -> Vec<Value<F>> {
        vec![
            Value::known(F::from(usize::from(self.tag) as u64)),
            Value::known(F::from(usize::from(self.tag_next) as u64)),
            Value::known(F::from(self.max_len)),
            Value::known(F::from(self.is_output as u64)),
            Value::known(F::from(self.is_reverse as u64)),
            Value::known(F::from(self.is_block as u64)),
        ]
    }
}

/// The symbol emitted by FSE table. This is also the weight in the canonical Huffman code.
#[derive(Clone, Copy, Debug, EnumIter, PartialEq, Eq, PartialOrd, Ord)]
pub enum FseSymbol {
    ///
    S0 = 0,
    ///
    S1,
    ///
    S2,
    ///
    S3,
    ///
    S4,
    ///
    S5,
    ///
    S6,
    ///
    S7,
}

impl_expr!(FseSymbol);

impl From<FseSymbol> for usize {
    fn from(value: FseSymbol) -> Self {
        value as usize
    }
}

impl From<FseSymbol> for u64 {
    fn from(value: FseSymbol) -> Self {
        value as u64
    }
}

impl From<usize> for FseSymbol {
    fn from(value: usize) -> Self {
        match value {
            0 => Self::S0,
            1 => Self::S1,
            2 => Self::S2,
            3 => Self::S3,
            4 => Self::S4,
            5 => Self::S5,
            6 => Self::S6,
            7 => Self::S7,
            _ => unreachable!("FseSymbol in [0, 8)"),
        }
    }
}

#[derive(Debug)]
pub enum BlockType {
    RawBlock = 0,
    RleBlock,
    ZstdCompressedBlock,
    Reserved,
}

impl From<u8> for BlockType {
    fn from(src: u8) -> Self {
        match src {
            0 => Self::RawBlock,
            1 => Self::RleBlock,
            2 => Self::ZstdCompressedBlock,
            3 => Self::Reserved,
            _ => unreachable!("BlockType is 2 bits"),
        }
    }
}

/// The type of Lstream.
#[derive(Clone, Copy, Debug, EnumIter)]
pub enum LstreamNum {
    /// Lstream 1.
    Lstream1 = 0,
    /// Lstream 2.
    Lstream2,
    /// Lstream 3.
    Lstream3,
    /// Lstream 4.
    Lstream4,
}

impl From<LstreamNum> for usize {
    fn from(value: LstreamNum) -> Self {
        value as usize
    }
}
impl From<usize> for LstreamNum {
    fn from(value: usize) -> LstreamNum {
        match value {
            0 => LstreamNum::Lstream1,
            1 => LstreamNum::Lstream2,
            2 => LstreamNum::Lstream3,
            3 => LstreamNum::Lstream4,
            _ => unreachable!("Wrong stream_idx"),
        }
    }
}

impl_expr!(LstreamNum);

/// Various tags that we can decode from a zstd encoded data.
#[derive(Clone, Copy, Debug, EnumIter, PartialEq, Eq, Hash)]
pub enum ZstdTag {
    /// Null should not occur.
    Null = 0,
    /// The frame header's descriptor.
    FrameHeaderDescriptor,
    /// The frame's content size.
    FrameContentSize,
    /// The block's header.
    BlockHeader,
    /// Raw bytes.
    RawBlockBytes,
    /// Run-length encoded bytes.
    RleBlockBytes,
    /// Zstd block's literals header.
    ZstdBlockLiteralsHeader,
    /// Zstd blocks might contain raw bytes.
    ZstdBlockLiteralsRawBytes,
    /// Zstd blocks might contain rle bytes.
    ZstdBlockLiteralsRleBytes,
    /// Zstd block's huffman header and FSE code.
    ZstdBlockFseCode,
    /// Zstd block's huffman code.
    ZstdBlockHuffmanCode,
    /// Zstd block's jump table.
    ZstdBlockJumpTable,
    /// Literal stream.
    ZstdBlockLstream,
    /// Beginning of sequence section.
    ZstdBlockSequenceHeader,
    /// sequence bitstream for recovering instructions
    ZstdBlockSequenceData,
}

impl ZstdTag {
    /// Whether this tag produces an output or not.
    pub fn is_output(&self) -> bool {
        match self {
            Self::Null => false,
            Self::FrameHeaderDescriptor => false,
            Self::FrameContentSize => false,
            Self::BlockHeader => false,
            Self::RawBlockBytes => true,
            Self::RleBlockBytes => true,
            Self::ZstdBlockLiteralsHeader => false,
            Self::ZstdBlockLiteralsRawBytes => false,
            Self::ZstdBlockLiteralsRleBytes => false,
            Self::ZstdBlockFseCode => false,
            Self::ZstdBlockHuffmanCode => false,
            Self::ZstdBlockJumpTable => false,
            Self::ZstdBlockLstream => false,
            Self::ZstdBlockSequenceHeader => false,
            Self::ZstdBlockSequenceData => true,
        }
    }

    /// Whether this tag is a part of block or not.
    pub fn is_block(&self) -> bool {
        match self {
            Self::Null => false,
            Self::FrameHeaderDescriptor => false,
            Self::FrameContentSize => false,
            Self::BlockHeader => false,
            Self::RawBlockBytes => true,
            Self::RleBlockBytes => true,
            Self::ZstdBlockLiteralsHeader => true,
            Self::ZstdBlockLiteralsRawBytes => true,
            Self::ZstdBlockLiteralsRleBytes => true,
            Self::ZstdBlockFseCode => true,
            Self::ZstdBlockHuffmanCode => true,
            Self::ZstdBlockJumpTable => true,
            Self::ZstdBlockLstream => true,
            Self::ZstdBlockSequenceHeader => true,
            Self::ZstdBlockSequenceData => true,
        }
    }

    /// Whether this tag is processed in back-to-front order.
    pub fn is_reverse(&self) -> bool {
        match self {
            Self::Null => false,
            Self::FrameHeaderDescriptor => false,
            Self::FrameContentSize => true,
            Self::BlockHeader => true,
            Self::RawBlockBytes => false,
            Self::RleBlockBytes => false,
            Self::ZstdBlockLiteralsHeader => false,
            Self::ZstdBlockLiteralsRawBytes => false,
            Self::ZstdBlockLiteralsRleBytes => false,
            Self::ZstdBlockFseCode => false,
            Self::ZstdBlockHuffmanCode => true,
            Self::ZstdBlockJumpTable => false,
            Self::ZstdBlockLstream => true,
            Self::ZstdBlockSequenceHeader => false,
            Self::ZstdBlockSequenceData => true,
        }
    }
}

impl_expr!(ZstdTag);

impl From<ZstdTag> for usize {
    fn from(value: ZstdTag) -> Self {
        value as usize
    }
}

impl ToString for ZstdTag {
    fn to_string(&self) -> String {
        String::from(match self {
            Self::Null => "null",
            Self::FrameHeaderDescriptor => "FrameHeaderDescriptor",
            Self::FrameContentSize => "FrameContentSize",
            Self::BlockHeader => "BlockHeader",
            Self::RawBlockBytes => "RawBlockBytes",
            Self::RleBlockBytes => "RleBlockBytes",
            Self::ZstdBlockLiteralsHeader => "ZstdBlockLiteralsHeader",
            Self::ZstdBlockLiteralsRawBytes => "ZstdBlockLiteralsRawBytes",
            Self::ZstdBlockLiteralsRleBytes => "ZstdBlockLiteralsRleBytes",
            Self::ZstdBlockFseCode => "ZstdBlockFseCode",
            Self::ZstdBlockHuffmanCode => "ZstdBlockHuffmanCode",
            Self::ZstdBlockJumpTable => "ZstdBlockJumpTable",
            Self::ZstdBlockLstream => "ZstdBlockLstream",
            Self::ZstdBlockSequenceHeader => "ZstdBlockSequenceHeader",
            Self::ZstdBlockSequenceData => "ZstdBlockSequenceData",
        })
    }
}

#[derive(Clone, Debug)]
pub struct ZstdState<F> {
    pub tag: ZstdTag,
    pub tag_next: ZstdTag,
    pub max_tag_len: u64,
    pub tag_len: u64,
    pub tag_idx: u64,
    pub tag_value: Value<F>,
    pub tag_value_acc: Value<F>,
    pub is_tag_change: bool,
    // Unlike tag_value, tag_rlc only uses challenge as multiplier
    pub tag_rlc: Value<F>,
    pub tag_rlc_acc: Value<F>,
}

impl<F: Field> Default for ZstdState<F> {
    fn default() -> Self {
        Self {
            tag: ZstdTag::Null,
            tag_next: ZstdTag::FrameHeaderDescriptor,
            max_tag_len: 0,
            tag_len: 0,
            tag_idx: 0,
            tag_value: Value::known(F::zero()),
            tag_value_acc: Value::known(F::zero()),
            is_tag_change: false,
            tag_rlc: Value::known(F::zero()),
            tag_rlc_acc: Value::known(F::zero()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EncodedData<F> {
    pub byte_idx: u64,
    pub encoded_len: u64,
    pub value_byte: u8,
    pub reverse: bool,
    pub reverse_idx: u64,
    pub reverse_len: u64,
    pub aux_1: Value<F>,
    pub aux_2: Value<F>,
    pub value_rlc: Value<F>,
}

impl<F: Field> EncodedData<F> {
    pub fn value_bits_le(&self) -> [u8; N_BITS_PER_BYTE] {
        value_bits_le(self.value_byte)
    }
}

impl<F: Field> Default for EncodedData<F> {
    fn default() -> Self {
        Self {
            byte_idx: 0,
            encoded_len: 0,
            value_byte: 0,
            reverse: false,
            reverse_idx: 0,
            reverse_len: 0,
            aux_1: Value::known(F::zero()),
            aux_2: Value::known(F::zero()),
            value_rlc: Value::known(F::zero()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DecodedData<F> {
    pub decoded_len: u64,
    pub decoded_len_acc: u64,
    pub total_decoded_len: u64,
    pub decoded_byte: u8,
    pub decoded_value_rlc: Value<F>,
}

#[derive(Clone, Debug, Default)]
pub struct HuffmanData {
    pub byte_offset: u64,
    pub bit_value: u8,
    pub stream_idx: usize,
    pub k: (u8, u8),
}

/// A single row in the FSE table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FseTableRow {
    /// The FSE state at this row in the FSE table.
    pub state: u64,
    /// The baseline associated with this state.
    pub baseline: u64,
    /// The number of bits to be read from the input bitstream at this state.
    pub num_bits: u64,
    /// The symbol emitted by the FSE table at this state.
    pub symbol: u64,
    /// During FSE table decoding, keep track of the number of symbol emitted
    pub num_emitted: u64,
    /// During FSE table decoding, keep track of accumulated states assigned
    pub n_acc: u64,
}

// Used for tracking bit markers for non-byte-aligned bitstream decoding
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BitstreamReadRow {
    /// Start of the bit location within a byte [0, 8)
    pub bit_start_idx: usize,
    /// End of the bit location within a byte (0, 16)
    pub bit_end_idx: usize,
    /// The value of the bitstring
    pub bit_value: u64,
    /// Whether 0 bit is read
    pub is_zero_bit_read: bool,
}

/// Sequence data is interleaved with 6 bitstreams. Each producing a different type of value.
#[derive(Clone, Copy, Debug)]
pub enum SequenceDataTag {
    Null = 0,
    LiteralLengthFse,
    MatchLengthFse,
    CookedMatchOffsetFse,
    LiteralLengthValue,
    MatchLengthValue,
    CookedMatchOffsetValue,
}

/// A single row in the Address table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AddressTableRow {
    /// Whether this row is padding for positional alignment with input
    pub s_padding: u64,
    /// Instruction Index
    pub instruction_idx: u64,
    /// Literal Length (directly decoded from sequence bitstream)
    pub literal_length: u64,
    /// Cooked Match Offset (directly decoded from sequence bitstream)
    pub cooked_match_offset: u64,
    /// Match Length (directly decoded from sequence bitstream)
    pub match_length: u64,
    /// Accumulation of literal length
    pub literal_length_acc: u64,
    /// Repeated offset 1
    pub repeated_offset1: u64,
    /// Repeated offset 2
    pub repeated_offset2: u64,
    /// Repeated offset 3
    pub repeated_offset3: u64,
    /// The actual match offset derived from cooked match offset
    pub actual_offset: u64,
}

/// Data for BL and Number of Bits for a state in LLT, CMOT and MLT
#[derive(Clone, Debug)]
pub struct SequenceFixedStateActionTable {
    /// Represent the state, BL and NB
    pub states_to_actions: Vec<(u64, (u64, u64))>,
}

impl SequenceFixedStateActionTable {
    /// Reconstruct action state table for literal length recovery
    pub fn reconstruct_lltv() -> Self {
        let mut states_to_actions = vec![];

        for idx in 0..=15 {
            states_to_actions.push((idx as u64, (idx as u64, 0u64)))
        }

        let rows: Vec<(u64, u64, u64)> = vec![
            (16, 16, 1),
            (17, 18, 1),
            (18, 20, 1),
            (19, 22, 1),
            (20, 24, 2),
            (21, 28, 2),
            (22, 32, 3),
            (23, 40, 3),
            (24, 48, 4),
            (25, 64, 6),
            (26, 128, 7),
            (27, 256, 8),
            (28, 512, 9),
            (29, 1024, 10),
            (30, 2048, 11),
            (31, 4096, 12),
            (32, 8192, 13),
            (33, 16384, 14),
            (34, 32768, 15),
            (35, 65536, 16),
        ];

        for row in rows {
            states_to_actions.push((row.0, (row.1, row.2)));
        }

        Self { states_to_actions }
    }

    /// Reconstruct action state table for match length recovery
    pub fn reconstruct_mltv() -> Self {
        let mut states_to_actions = vec![];

        for idx in 0..=31 {
            states_to_actions.push((idx as u64, (idx as u64 + 3, 0u64)))
        }

        let rows: Vec<(u64, u64, u64)> = vec![
            (32, 35, 1),
            (33, 37, 1),
            (34, 39, 1),
            (35, 41, 1),
            (36, 43, 2),
            (37, 47, 2),
            (38, 51, 3),
            (39, 59, 3),
            (40, 67, 4),
            (41, 83, 4),
            (42, 99, 5),
            (43, 131, 7),
            (44, 259, 8),
            (45, 515, 9),
            (46, 1027, 10),
            (47, 2051, 11),
            (48, 4099, 12),
            (49, 8195, 13),
            (50, 16387, 14),
            (51, 32771, 15),
            (52, 65539, 16),
        ];

        for row in rows {
            states_to_actions.push((row.0, (row.1, row.2)));
        }

        Self { states_to_actions }
    }

    /// Reconstruct action state table for offset recovery
    pub fn reconstruct_cmotv(n: u64) -> Self {
        let mut states_to_actions = vec![];

        for idx in 0..=n {
            states_to_actions.push((idx, ((1 << idx) as u64, idx)))
        }

        Self { states_to_actions }
    }
}

/// Data for the FSE table's witness values.
#[derive(Clone, Debug)]
pub struct FseTableData {
    /// The byte offset in the frame at which the FSE table is described.
    pub byte_offset: u64,
    /// The FSE table's size, i.e. 1 << AL (accuracy log).
    pub table_size: u64,
    /// Represent the states, symbols, and so on of this FSE table.
    pub rows: Vec<FseTableRow>,
}

/// Auxiliary data accompanying the FSE table's witness values.
#[derive(Clone, Debug)]
pub struct FseAuxiliaryTableData {
    /// The byte offset in the frame at which the FSE table is described.
    pub byte_offset: u64,
    /// The FSE table's size, i.e. 1 << AL (accuracy log).
    pub table_size: u64,
    /// A map from FseSymbol (weight) to states, also including fields for that state, for
    /// instance, the baseline and the number of bits to read from the FSE bitstream.
    ///
    /// For each symbol, the states are in strictly increasing order.
    pub sym_to_states: BTreeMap<u64, Vec<FseTableRow>>,
}

/// Another form of Fse table that has state as key instead of the FseSymbol.
/// In decoding, symbols are emitted from state-chaining.
/// This representation makes it easy to look up decoded symbol from current state.   
/// Map<state, (symbol, baseline, num_bits)>.
type FseStateMapping = BTreeMap<u64, (u64, u64, u64)>;
type ReconstructedFse = (usize, Vec<(u32, u64)>, FseAuxiliaryTableData);

impl FseAuxiliaryTableData {
    #[allow(non_snake_case)]
    /// While we reconstruct an FSE table from a bitstream, we do not know before reconstruction
    /// how many exact bytes we would finally be reading.
    ///
    /// The number of bytes actually read while reconstruction is called `t` and is returned along
    /// with the reconstructed FSE table. After processing the entire bitstream to reconstruct the
    /// FSE table, if the read bitstream was not byte aligned, then we discard the 1..8 bits from
    /// the last byte that we read from.
    pub fn reconstruct(src: &[u8], byte_offset: usize) -> std::io::Result<ReconstructedFse> {
        // construct little-endian bit-reader.
        let data = src.iter().skip(byte_offset).cloned().collect::<Vec<u8>>();
        let mut reader = BitReader::endian(Cursor::new(&data), LittleEndian);
        let mut bit_boundaries: Vec<(u32, u64)> = vec![];

        // number of bits read by the bit-reader from the bistream.
        let mut offset = 0;

        let accuracy_log = {
            offset += 4;
            reader.read::<u8>(offset)? + 5
        };
        bit_boundaries.push((offset, accuracy_log as u64 - 5));
        let table_size = 1 << accuracy_log;

        let mut sym_to_states = BTreeMap::new();
        let mut R = table_size;
        let mut state = 0x00;
        let mut symbol = 0;
        while R > 0 {
            // number of bits and value read from the variable bit-packed data.
            // And update the total number of bits read so far.
            let (n_bits_read, value) = read_variable_bit_packing(&data, offset, R + 1)?;
            reader.skip(n_bits_read)?;
            offset += n_bits_read;
            bit_boundaries.push((offset, value));

            if value == 0 {
                unimplemented!("value=0 => prob=-1: scenario unimplemented");
            }

            let N = value - 1;

            // When a symbol has a probability of zero, it is followed by a 2-bits repeat flag. This
            // repeat flag tells how many probabilities of zeroes follow the current one. It
            // provides a number ranging from 0 to 3. If it is a 3, another 2-bits repeat flag
            // follows, and so on.
            if N == 0 {
                sym_to_states.insert(symbol, vec![]);
                symbol += 1;

                loop {
                    let repeat_bits = reader.read::<u8>(2)?;
                    offset += 2;
                    bit_boundaries.push((offset, repeat_bits as u64));

                    for k in 0..repeat_bits {
                        sym_to_states.insert(symbol + (k as u64), vec![]);
                    }
                    symbol += repeat_bits as u64;

                    if repeat_bits < 3 {
                        break;
                    }
                }
            }

            if N >= 1 {
                let states = std::iter::once(state)
                    .chain((1..N).map(|_| {
                        state += (table_size >> 1) + (table_size >> 3) + 3;
                        state &= table_size - 1;
                        state
                    }))
                    .sorted()
                    .collect::<Vec<u64>>();
                let (smallest_spot_idx, nbs) = smaller_powers_of_two(table_size, N);
                let baselines = if N == 1 {
                    vec![0x00]
                } else {
                    let mut rotated_nbs = nbs.clone();
                    rotated_nbs.rotate_left(smallest_spot_idx);

                    let mut baselines = std::iter::once(0x00)
                        .chain(rotated_nbs.iter().scan(0x00, |baseline, nb| {
                            *baseline += 1 << nb;
                            Some(*baseline)
                        }))
                        .take(N as usize)
                        .collect::<Vec<u64>>();

                    baselines.rotate_right(smallest_spot_idx);
                    baselines
                };
                sym_to_states.insert(
                    symbol,
                    states
                        .iter()
                        .zip(nbs.iter())
                        .zip(baselines.iter())
                        .map(|((&state, &nb), &baseline)| FseTableRow {
                            state,
                            num_bits: nb,
                            baseline,
                            symbol,
                            num_emitted: 0,
                            n_acc: 0,
                        })
                        .collect(),
                );

                // increment symbol.
                symbol += 1;

                // update state.
                state += (table_size >> 1) + (table_size >> 3) + 3;
                state &= table_size - 1;
            }

            // remove N slots from a total of R.
            R -= N;
        }

        // ignore any bits left to be read until byte-aligned.
        let t = (((offset as usize) - 1) / N_BITS_PER_BYTE) + 1;

        // read the trailing section
        if t * N_BITS_PER_BYTE > (offset as usize) {
            let bits_remaining = t * N_BITS_PER_BYTE - offset as usize;
            bit_boundaries.push((
                offset + bits_remaining as u32,
                reader.read::<u8>(bits_remaining as u32)? as u64,
            ));
        }

        Ok((
            t,
            bit_boundaries,
            Self {
                byte_offset: byte_offset as u64,
                table_size,
                sym_to_states,
            },
        ))
    }

    /// Convert an FseAuxiliaryTableData into a state-mapped representation.
    /// This makes it easier to lookup state-chaining during decoding.
    pub fn parse_state_table(&self) -> FseStateMapping {
        let rows: Vec<FseTableRow> = self
            .sym_to_states
            .values()
            .flat_map(|v| v.clone())
            .collect();
        let mut state_table: FseStateMapping = BTreeMap::new();

        for row in rows {
            state_table.insert(row.state, (row.symbol, row.baseline, row.num_bits));
        }

        state_table
    }
}

#[derive(Clone, Debug)]
/// Row witness value for decompression circuit
pub struct ZstdWitnessRow<F> {
    /// Current decoding state during Zstd decompression
    pub state: ZstdState<F>,
    /// Data on compressed data
    pub encoded_data: EncodedData<F>,
    /// Data on decompressed data
    pub decoded_data: DecodedData<F>,
    /// Fse decoding state transition data
    pub fse_data: FseTableRow,
    /// Bitstream reader
    pub bitstream_read_data: BitstreamReadRow,
}

impl<F: Field> ZstdWitnessRow<F> {
    /// Construct the first row of witnesses for decompression circuit
    pub fn init(src_len: usize) -> Self {
        Self {
            state: ZstdState::default(),
            encoded_data: EncodedData {
                encoded_len: src_len as u64,
                ..Default::default()
            },
            decoded_data: DecodedData::default(),
            fse_data: FseTableRow::default(),
            bitstream_read_data: BitstreamReadRow::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fse_reconstruction() -> std::io::Result<()> {
        // The first 3 bytes are garbage data and the offset == 3 passed to the function should
        // appropriately ignore those bytes. Only the next 4 bytes are meaningful and the FSE
        // reconstruction should read bitstreams only until the end of the 4th byte. The 3
        // other bytes are garbage (for the purpose of this test case), and we want to make
        // sure FSE reconstruction ignores them.
        let src = vec![0xff, 0xff, 0xff, 0x30, 0x6f, 0x9b, 0x03, 0xff, 0xff, 0xff];

        let (n_bytes, _bit_boundaries, table) = FseAuxiliaryTableData::reconstruct(&src, 3)?;

        // TODO: assert equality for the entire table.
        // for now only comparing state/baseline/nb for S1, i.e. weight == 1.

        assert_eq!(n_bytes, 4);
        assert_eq!(
            table.sym_to_states.get(&1).cloned().unwrap(),
            [
                (0x03, 0x10, 3),
                (0x0c, 0x18, 3),
                (0x11, 0x00, 2),
                (0x15, 0x04, 2),
                (0x1a, 0x08, 2),
                (0x1e, 0x0c, 2),
            ]
            .iter()
            .enumerate()
            .map(|(_i, &(state, baseline, num_bits))| FseTableRow {
                state,
                symbol: 1,
                baseline,
                num_bits,
                num_emitted: 0,
                n_acc: 0,
            })
            .collect::<Vec<FseTableRow>>(),
        );

        Ok(())
    }

    #[test]
    fn test_sequences_fse_reconstruction() -> std::io::Result<()> {
        let src = vec![
            0x21, 0x9d, 0x51, 0xcc, 0x18, 0x42, 0x44, 0x81, 0x8c, 0x94, 0xb4, 0x50, 0x1e,
        ];

        let (_n_bytes, _bit_boundaries, table) = FseAuxiliaryTableData::reconstruct(&src, 0)?;
        let _parsed_state_map = table.parse_state_table();

        // TODO: assertions

        Ok(())
    }
}

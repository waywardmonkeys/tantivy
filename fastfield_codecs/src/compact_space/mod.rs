/// This codec takes a large number space (u128) and reduces it to a compact number space.
///
/// It will find spaces in the number range. For example:
///
/// 100, 101, 102, 103, 104, 50000, 50001
/// could be mapped to
/// 100..104 -> 0..4
/// 50000..50001 -> 5..6
///
/// Compact space 0..=6 requires much less bits than 100..=50001
///
/// The codec is created to compress ip addresses, but may be employed in other use cases.
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    io::{self, Write},
    net::{IpAddr, Ipv6Addr},
    ops::RangeInclusive,
};

use common::{BinarySerializable, CountingWriter, VInt, VIntU128};
use ownedbytes::OwnedBytes;
use tantivy_bitpacker::{self, BitPacker, BitUnpacker};

use crate::column::{ColumnV2, ColumnV2Ext};
use crate::compact_space::build_compact_space::get_compact_space;

mod blank_range;
mod build_compact_space;

pub fn ip_to_u128(ip_addr: IpAddr) -> u128 {
    let ip_addr_v6: Ipv6Addr = match ip_addr {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    };
    u128::from_be_bytes(ip_addr_v6.octets())
}

const NULL_VALUE_COMPACT_SPACE: u64 = 0;

/// The cost per blank is quite hard actually, since blanks are delta encoded, the actual cost of
/// blanks depends on the number of blanks.
///
/// The number is taken by looking at a real dataset. It is optimized for larger datasets.
const COST_PER_BLANK_IN_BITS: usize = 36;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CompactSpace {
    ranges_mapping: Vec<RangeMapping>,
}

/// Maps the range from the original space to compact_start + range.len()
#[derive(Debug, Clone, Eq, PartialEq)]
struct RangeMapping {
    value_range: RangeInclusive<u128>,
    compact_start: u64,
}
impl RangeMapping {
    fn range_length(&self) -> u64 {
        (self.value_range.end() - self.value_range.start()) as u64 + 1
    }

    // The last value of the compact space in this range
    fn compact_end(&self) -> u64 {
        self.compact_start + self.range_length() - 1
    }
}

impl BinarySerializable for CompactSpace {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        VInt(self.ranges_mapping.len() as u64).serialize(writer)?;

        let mut prev_value = 0;
        for value_range in self
            .ranges_mapping
            .iter()
            .map(|range_mapping| &range_mapping.value_range)
        {
            let blank_delta_start = value_range.start() - prev_value;
            VIntU128(blank_delta_start).serialize(writer)?;
            prev_value = *value_range.start();

            let blank_delta_end = value_range.end() - prev_value;
            VIntU128(blank_delta_end).serialize(writer)?;
            prev_value = *value_range.end();
        }

        Ok(())
    }

    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let num_ranges = VInt::deserialize(reader)?.0;
        let mut ranges_mapping: Vec<RangeMapping> = vec![];
        let mut value = 0u128;
        let mut compact_start = 1u64; // 0 is reserved for `null`
        for _ in 0..num_ranges {
            let blank_delta_start = VIntU128::deserialize(reader)?.0;
            value += blank_delta_start;
            let blank_start = value;

            let blank_delta_end = VIntU128::deserialize(reader)?.0;
            value += blank_delta_end;
            let blank_end = value;

            let range_mapping = RangeMapping {
                value_range: blank_start..=blank_end,
                compact_start,
            };
            let range_length = range_mapping.range_length();
            ranges_mapping.push(range_mapping);
            compact_start += range_length as u64;
        }

        Ok(Self { ranges_mapping })
    }
}

impl CompactSpace {
    /// Amplitude is the value range of the compact space including the sentinel value used to
    /// identify null values. The compact space is 0..=amplitude .
    ///
    /// It's only used to verify we don't exceed u64 number space, which would indicate a bug.
    fn amplitude_compact_space(&self) -> u128 {
        self.ranges_mapping
            .last()
            .map(|last_range| last_range.compact_end() as u128)
            .unwrap_or(1) // compact space starts at 1, 0 == null
    }

    fn get_range_mapping(&self, pos: usize) -> &RangeMapping {
        &self.ranges_mapping[pos]
    }

    /// Returns either Ok(the value in the compact space) or if it is outside the compact space the
    /// Err(position where it would be inserted)
    fn u128_to_compact(&self, value: u128) -> Result<u64, usize> {
        self.ranges_mapping
            .binary_search_by(|probe| {
                let value_range = &probe.value_range;
                if value < *value_range.start() {
                    Ordering::Greater
                } else if value > *value_range.end() {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
            .map(|pos| {
                let range_mapping = &self.ranges_mapping[pos];
                let pos_in_range = (value - range_mapping.value_range.start()) as u64;
                range_mapping.compact_start + pos_in_range
            })
    }

    /// Unpacks a value from compact space u64 to u128 space
    fn compact_to_u128(&self, compact: u64) -> u128 {
        let pos = self
            .ranges_mapping
            .binary_search_by_key(&compact, |range_mapping| range_mapping.compact_start)
            // Correctness: Overflow. The first range starts at compact space 0, the error from
            // binary search can never be 0
            .map_or_else(|e| e - 1, |v| v);

        let range_mapping = &self.ranges_mapping[pos];
        let diff = compact - range_mapping.compact_start;
        range_mapping.value_range.start() + diff as u128
    }
}

pub struct CompactSpaceCompressor {
    params: IPCodecParams,
}
#[derive(Debug, Clone)]
pub struct IPCodecParams {
    compact_space: CompactSpace,
    bit_unpacker: BitUnpacker,
    min_value: u128,
    max_value: u128,
    num_vals: u64,
    num_bits: u8,
}

impl CompactSpaceCompressor {
    /// Taking the vals as Vec may cost a lot of memory. It is used to sort the vals.
    pub fn train_from(column: impl ColumnV2<u128>) -> Self {
        let mut values_sorted = BTreeSet::new();
        values_sorted.extend(column.iter().flatten());
        let total_num_values = column.num_vals();

        let compact_space =
            get_compact_space(&values_sorted, total_num_values, COST_PER_BLANK_IN_BITS);
        let amplitude_compact_space = compact_space.amplitude_compact_space();

        assert!(
            amplitude_compact_space <= u64::MAX as u128,
            "case unsupported."
        );

        let num_bits = tantivy_bitpacker::compute_num_bits(amplitude_compact_space as u64);
        let min_value = *values_sorted.iter().next().unwrap_or(&0);
        let max_value = *values_sorted.iter().last().unwrap_or(&0);
        assert_eq!(
            compact_space
                .u128_to_compact(max_value)
                .expect("could not convert max value to compact space"),
            amplitude_compact_space as u64
        );
        CompactSpaceCompressor {
            params: IPCodecParams {
                compact_space,
                bit_unpacker: BitUnpacker::new(num_bits),
                min_value,
                max_value,
                num_vals: total_num_values as u64,
                num_bits,
            },
        }
    }

    fn write_footer(self, writer: &mut impl Write) -> io::Result<()> {
        let writer = &mut CountingWriter::wrap(writer);
        self.params.serialize(writer)?;

        let footer_len = writer.written_bytes() as u32;
        footer_len.serialize(writer)?;

        Ok(())
    }

    pub fn compress(self, vals: impl Iterator<Item = Option<u128>>) -> io::Result<Vec<u8>> {
        let mut output = vec![];
        self.compress_into(vals, &mut output)?;
        Ok(output)
    }

    pub fn compress_into(
        self,
        vals: impl Iterator<Item = Option<u128>>,
        write: &mut impl Write,
    ) -> io::Result<()> {
        let mut bitpacker = BitPacker::default();
        for val in vals {
            let compact = if let Some(val) = val {
                self.params
                    .compact_space
                    .u128_to_compact(val)
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Could not convert value to compact_space. This is a bug.",
                        )
                    })?
            } else {
                NULL_VALUE_COMPACT_SPACE
            };
            bitpacker.write(compact, self.params.num_bits, write)?;
        }
        bitpacker.close(write)?;
        self.write_footer(write)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CompactSpaceDecompressor {
    data: OwnedBytes,
    params: IPCodecParams,
}

impl BinarySerializable for IPCodecParams {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        // header flags for future optional dictionary encoding
        let footer_flags = 0u64;
        footer_flags.serialize(writer)?;

        VIntU128(self.min_value).serialize(writer)?;
        VIntU128(self.max_value).serialize(writer)?;
        VIntU128(self.num_vals as u128).serialize(writer)?;
        self.num_bits.serialize(writer)?;

        self.compact_space.serialize(writer)?;

        Ok(())
    }

    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let _header_flags = u64::deserialize(reader)?;
        let min_value = VIntU128::deserialize(reader)?.0;
        let max_value = VIntU128::deserialize(reader)?.0;
        let num_vals = VIntU128::deserialize(reader)?.0 as u64;
        let num_bits = u8::deserialize(reader)?;
        let compact_space = CompactSpace::deserialize(reader)?;

        Ok(Self {
            compact_space,
            bit_unpacker: BitUnpacker::new(num_bits),
            min_value,
            max_value,
            num_vals,
            num_bits,
        })
    }
}

impl ColumnV2<u128> for CompactSpaceDecompressor {
    #[inline]
    fn get_val(&self, doc: u64) -> Option<u128> {
        self.get(doc)
    }

    fn min_value(&self) -> u128 {
        self.min_value()
    }

    fn max_value(&self) -> u128 {
        self.max_value()
    }

    fn num_vals(&self) -> u64 {
        self.params.num_vals
    }

    #[inline]
    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = Option<u128>> + 'a> {
        Box::new(self.iter())
    }
}

impl ColumnV2Ext<u128> for CompactSpaceDecompressor {
    fn get_between_vals(&self, range: RangeInclusive<u128>) -> Vec<u64> {
        self.get_range(range)
    }
}

impl CompactSpaceDecompressor {
    pub fn open(data: OwnedBytes) -> io::Result<CompactSpaceDecompressor> {
        let (data_slice, footer_len_bytes) = data.split_at(data.len() - 4);
        let footer_len = u32::deserialize(&mut &footer_len_bytes[..])?;

        let data_footer = &data_slice[data_slice.len() - footer_len as usize..];
        let params = IPCodecParams::deserialize(&mut &data_footer[..])?;
        let decompressor = CompactSpaceDecompressor { data, params };

        Ok(decompressor)
    }

    /// Converting to compact space for the decompressor is more complex, since we may get values
    /// which are outside the compact space. e.g. if we map
    /// 1000 => 5
    /// 2000 => 6
    ///
    /// and we want a mapping for 1005, there is no equivalent compact space. We instead return an
    /// error with the index of the next range.
    fn u128_to_compact(&self, value: u128) -> Result<u64, usize> {
        self.params.compact_space.u128_to_compact(value)
    }

    fn compact_to_u128(&self, compact: u64) -> u128 {
        self.params.compact_space.compact_to_u128(compact)
    }

    /// Comparing on compact space: 1.08 GElements/s, which equals a throughput of 17,3 Gb/s
    /// (based on u128 = 16byte)
    ///
    /// Comparing on original space: .06 GElements/s (not completely optimized)
    pub fn get_range(&self, range: RangeInclusive<u128>) -> Vec<u64> {
        if range.start() > range.end() {
            return Vec::new();
        }
        let from_value = *range.start();
        let to_value = *range.end();
        assert!(to_value >= from_value);
        let compact_from = self.u128_to_compact(from_value);
        let compact_to = self.u128_to_compact(to_value);

        // Quick return, if both ranges fall into the same non-mapped space, the range can't cover
        // any values, so we can early exit
        match (compact_to, compact_from) {
            (Err(pos1), Err(pos2)) if pos1 == pos2 => return Vec::new(),
            _ => {}
        }

        let compact_from = compact_from.unwrap_or_else(|pos| {
            // Correctness: Out of bounds, if this value is Err(last_index + 1), we early exit,
            // since the to_value also mapps into the same non-mapped space
            let range_mapping = self.params.compact_space.get_range_mapping(pos);
            range_mapping.compact_start
        });
        // If there is no compact space, we go to the closest upperbound compact space
        let compact_to = compact_to.unwrap_or_else(|pos| {
            // Correctness: Overflow, if this value is Err(0), we early exit,
            // since the from_value also mapps into the same non-mapped space

            // Get end of previous range
            let pos = pos - 1;
            let range_mapping = self.params.compact_space.get_range_mapping(pos);
            range_mapping.compact_end()
        });

        let range = compact_from..=compact_to;
        let mut positions = Vec::new();

        let step_size = 4;
        let cutoff = self.params.num_vals - self.params.num_vals % step_size;

        let mut push_if_in_range = |idx, val| {
            if range.contains(&val) {
                positions.push(idx);
            }
        };
        let get_val = |idx| self.params.bit_unpacker.get(idx as u64, &self.data);
        // unrolled loop
        for idx in (0..cutoff).step_by(step_size as usize) {
            let idx1 = idx;
            let idx2 = idx + 1;
            let idx3 = idx + 2;
            let idx4 = idx + 3;
            let val1 = get_val(idx1);
            let val2 = get_val(idx2);
            let val3 = get_val(idx3);
            let val4 = get_val(idx4);
            push_if_in_range(idx1, val1);
            push_if_in_range(idx2, val2);
            push_if_in_range(idx3, val3);
            push_if_in_range(idx4, val4);
        }

        // handle rest
        for idx in cutoff..self.params.num_vals {
            push_if_in_range(idx, get_val(idx));
        }

        positions
    }

    #[inline]
    fn iter_compact(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.params.num_vals)
            .map(move |idx| self.params.bit_unpacker.get(idx as u64, &self.data) as u64)
    }

    #[inline]
    fn iter(&self) -> impl Iterator<Item = Option<u128>> + '_ {
        // TODO: Performance. It would be better to iterate on the ranges and check existence via
        // the bit_unpacker.
        self.iter_compact().map(|compact| {
            if compact == NULL_VALUE_COMPACT_SPACE {
                None
            } else {
                Some(self.compact_to_u128(compact))
            }
        })
    }

    #[inline]
    pub fn get(&self, idx: u64) -> Option<u128> {
        let compact = self.params.bit_unpacker.get(idx, &self.data);
        if compact == NULL_VALUE_COMPACT_SPACE {
            None
        } else {
            Some(self.compact_to_u128(compact))
        }
    }

    pub fn min_value(&self) -> u128 {
        self.params.min_value
    }

    pub fn max_value(&self) -> u128 {
        self.params.max_value
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::VecColumn;

    #[test]
    fn compact_space_test() {
        let ips = &[
            2u128, 4u128, 1000, 1001, 1002, 1003, 1004, 1005, 1008, 1010, 1012, 1260,
        ]
        .into_iter()
        .collect();
        let compact_space = get_compact_space(ips, ips.len() as u64, 11);
        let amplitude = compact_space.amplitude_compact_space();
        assert_eq!(amplitude, 17);
        assert_eq!(1, compact_space.u128_to_compact(2).unwrap());
        assert_eq!(2, compact_space.u128_to_compact(3).unwrap());
        assert_eq!(compact_space.u128_to_compact(100).unwrap_err(), 1);

        for (num1, num2) in (0..3).tuple_windows() {
            assert_eq!(
                compact_space.get_range_mapping(num1).compact_end() + 1,
                compact_space.get_range_mapping(num2).compact_start
            );
        }

        let mut output: Vec<u8> = Vec::new();
        compact_space.serialize(&mut output).unwrap();

        assert_eq!(
            compact_space,
            CompactSpace::deserialize(&mut &output[..]).unwrap()
        );

        for ip in ips {
            let compact = compact_space.u128_to_compact(*ip).unwrap();
            assert_eq!(compact_space.compact_to_u128(compact), *ip);
        }
    }

    #[test]
    fn compact_space_amplitude_test() {
        let ips = &[100000u128, 1000000].into_iter().collect();
        let compact_space = get_compact_space(ips, ips.len() as u64, 1);
        let amplitude = compact_space.amplitude_compact_space();
        assert_eq!(amplitude, 2);
    }

    fn test_all(data: OwnedBytes, expected: &[Option<u128>]) {
        let decompressor = CompactSpaceDecompressor::open(data).unwrap();
        for (idx, expected_val) in expected.iter().cloned().enumerate() {
            let val = decompressor.get(idx as u64);
            assert_eq!(val, expected_val);

            if let Some(expected_val) = expected_val {
                let test_range = |range: RangeInclusive<u128>| {
                    let expected_positions = expected
                        .iter()
                        .positions(|val| val.map(|val| range.contains(&val)).unwrap_or(false))
                        .map(|pos| pos as u64)
                        .collect::<Vec<_>>();
                    let positions = decompressor.get_range(range);
                    assert_eq!(positions, expected_positions);
                };

                test_range(expected_val.saturating_sub(1)..=expected_val);
                test_range(expected_val..=expected_val);
                test_range(expected_val..=expected_val.saturating_add(1));
                test_range(expected_val.saturating_sub(1)..=expected_val.saturating_add(1));
            }
        }
    }

    fn test_aux_vals_opt(u128_vals: &[Option<u128>]) -> OwnedBytes {
        let compressor = CompactSpaceCompressor::train_from(VecColumn::from(u128_vals));
        let data = compressor.compress(u128_vals.iter().cloned()).unwrap();
        let data = OwnedBytes::new(data);
        test_all(data.clone(), u128_vals);
        data
    }

    fn test_aux_vals(u128_vals: &[u128]) -> OwnedBytes {
        test_aux_vals_opt(&u128_vals.iter().cloned().map(Some).collect::<Vec<_>>())
    }

    #[test]
    fn test_range_1() {
        let vals = &[
            1u128,
            100u128,
            3u128,
            99999u128,
            100000u128,
            100001u128,
            4_000_211_221u128,
            4_000_211_222u128,
            333u128,
        ];
        let data = test_aux_vals(vals);
        let decomp = CompactSpaceDecompressor::open(data).unwrap();
        let positions = decomp.get_range(0..=1);
        assert_eq!(positions, vec![0]);
        let positions = decomp.get_range(0..=2);
        assert_eq!(positions, vec![0]);
        let positions = decomp.get_range(0..=3);
        assert_eq!(positions, vec![0, 2]);
        assert_eq!(decomp.get_range(99999u128..=99999u128), vec![3]);
        assert_eq!(decomp.get_range(99999u128..=100000u128), vec![3, 4]);
        assert_eq!(decomp.get_range(99998u128..=100000u128), vec![3, 4]);
        assert_eq!(decomp.get_range(99998u128..=99999u128), vec![3]);
        assert_eq!(decomp.get_range(99998u128..=99998u128), vec![]);
        assert_eq!(decomp.get_range(333u128..=333u128), vec![8]);
        assert_eq!(decomp.get_range(332u128..=333u128), vec![8]);
        assert_eq!(decomp.get_range(332u128..=334u128), vec![8]);
        assert_eq!(decomp.get_range(333u128..=334u128), vec![8]);

        assert_eq!(
            decomp.get_range(4_000_211_221u128..=5_000_000_000u128),
            vec![6, 7]
        );
    }

    #[test]
    fn test_empty() {
        let vals = &[];
        let data = test_aux_vals(vals);
        let _decomp = CompactSpaceDecompressor::open(data).unwrap();
    }

    #[test]
    fn test_range_2() {
        let vals = &[
            100u128,
            99999u128,
            100000u128,
            100001u128,
            4_000_211_221u128,
            4_000_211_222u128,
            333u128,
        ];
        let data = test_aux_vals(vals);
        let decomp = CompactSpaceDecompressor::open(data).unwrap();
        let positions = decomp.get_range(0..=5);
        assert_eq!(positions, vec![]);
        let positions = decomp.get_range(0..=100);
        assert_eq!(positions, vec![0]);
        let positions = decomp.get_range(0..=105);
        assert_eq!(positions, vec![0]);
    }

    #[test]
    fn test_null() {
        let vals = vec![None, Some(2u128)];
        let compressor = CompactSpaceCompressor::train_from(VecColumn::from(&vals));
        let data = compressor.compress(vals.iter().cloned()).unwrap();
        let decomp = CompactSpaceDecompressor::open(OwnedBytes::new(data)).unwrap();
        let positions = decomp.get_range(0..=1);
        assert_eq!(positions, vec![]);
        let positions = decomp.get_range(2..=2);
        assert_eq!(positions, vec![1]);

        let positions = decomp.get_range(2..=3);
        assert_eq!(positions, vec![1]);

        let positions = decomp.get_range(1..=3);
        assert_eq!(positions, vec![1]);

        let positions = decomp.get_range(2..=3);
        assert_eq!(positions, vec![1]);

        let positions = decomp.get_range(3..=3);
        assert_eq!(positions, vec![]);
    }

    #[test]
    fn test_range_3() {
        let vals = &[
            200u128,
            201,
            202,
            203,
            204,
            204,
            206,
            207,
            208,
            209,
            210,
            1_000_000,
            5_000_000_000,
        ];
        let compressor = CompactSpaceCompressor::train_from(VecColumn::from(vals));
        let data = compressor.compress(vals.iter().cloned().map(Some)).unwrap();
        let decomp = CompactSpaceDecompressor::open(OwnedBytes::new(data)).unwrap();

        assert_eq!(decomp.get_range(199..=200), vec![0]);
        assert_eq!(decomp.get_range(199..=201), vec![0, 1]);
        assert_eq!(decomp.get_range(200..=200), vec![0]);
        assert_eq!(decomp.get_range(1_000_000..=1_000_000), vec![11]);
    }

    #[test]
    fn test_bug1() {
        let vals = &[9223372036854775806];
        let _data = test_aux_vals(vals);
    }

    #[test]
    fn test_bug2() {
        let vals = &[340282366920938463463374607431768211455u128];
        let _data = test_aux_vals(vals);
    }

    #[test]
    fn test_bug3() {
        let vals = &[340282366920938463463374607431768211454];
        let _data = test_aux_vals(vals);
    }

    #[test]
    fn test_bug4() {
        let vals = &[340282366920938463463374607431768211455, 0];
        let _data = test_aux_vals(vals);
    }

    #[test]
    fn test_first_large_gaps() {
        let vals = &[1_000_000_000u128; 100];
        let _data = test_aux_vals(vals);
    }
    use itertools::Itertools;
    use proptest::prelude::*;

    fn num_strategy() -> impl Strategy<Value = u128> {
        prop_oneof![
            1 => prop::num::u128::ANY.prop_map(|num| u128::MAX - (num % 10) ),
            1 => prop::num::u128::ANY.prop_map(|num| i64::MAX as u128 + 5 - (num % 10) ),
            1 => prop::num::u128::ANY.prop_map(|num| i128::MAX as u128 + 5 - (num % 10) ),
            1 => prop::num::u128::ANY.prop_map(|num| num % 10 ),
            20 => prop::num::u128::ANY,
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10))]

            #[test]
            fn compress_decompress_random(vals in proptest::collection::vec(num_strategy()
    , 1..1000)) {
                let _data = test_aux_vals(&vals);
            }
        }
}

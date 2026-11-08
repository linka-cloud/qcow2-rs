//! QCOW2 cluster codecs and packed compressed output.
use super::advanced::Mutation;
use super::graph::invalid;
use super::*;
use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    Deflate,
    Zstd,
}

pub(crate) fn decode(codec: u8, input: &[u8], size: usize) -> Qcow2Result<Vec<u8>> {
    // An extra output byte distinguishes a complete stream from an oversized
    // stream that happened to fill the guest cluster before its end marker.
    let mut data = vec![0; size + 1];
    let written = match codec {
        0 => {
            let (status, _, written) = decompress(
                &mut DecompressorOxide::new(),
                input,
                &mut data,
                0,
                inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
            );
            if status != TINFLStatus::Done {
                return Err(invalid("Incomplete or malformed Deflate cluster"));
            }
            written
        }
        1 => {
            let frame_size = zstd::zstd_safe::find_frame_compressed_size(input)
                .map_err(|_| invalid("Incomplete or malformed Zstd frame"))?;
            zstd::bulk::decompress_to_buffer(&input[..frame_size], &mut data[..])
                .map_err(|_| invalid("Invalid or oversized Zstd cluster"))?
        }
        _ => return Err(invalid("Unsupported compression codec")),
    };
    if written != size {
        return Err(invalid("Decompressed cluster has the wrong length"));
    }
    data.truncate(size);
    Ok(data)
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    /// Select the compression format before allocating any payload storage.
    pub async fn set_compression_type(&self, codec: CompressionType) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        if self.header.read().await.version() != 3
            || self.info.encrypted()
            || self.info.external_data()
        {
            return Err(invalid(
                "Compression selection requires an unencrypted v3 image without external data",
            ));
        }
        self.flush_meta_locked().await?;
        if self.graph().await?.data_mappings.0 != 0 {
            return Err(invalid("Select compression before allocating data"));
        }
        self.begin_transaction().await?;
        self.header
            .write()
            .await
            .set_compression_type(if codec == CompressionType::Zstd { 1 } else { 0 });
        let result = self.rebuild_refcounts().await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }
    /// Write complete guest clusters with the image's compression codec.
    /// A final partial cluster is allowed only at virtual EOF. Incompressible
    /// clusters fall back to ordinary storage. A batch is limited to 32 MiB.
    pub async fn write_compressed_at(&self, buf: &[u8], offset: u64) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        let cs = self.info.cluster_size();
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or("Write range overflow")?;
        if self.info.external_data() || self.header.read().await.crypt_method() != 0 {
            return Err(invalid(
                "Compression is incompatible with external data or encryption",
            ));
        }
        if offset % cs as u64 != 0
            || (buf.len() % cs != 0 && end != self.info.virtual_size())
            || buf.len() > 32 << 20
        {
            return Err(invalid(
                "Compressed writes require whole clusters (at most 32 MiB)",
            ));
        }
        self.advanced_modify(offset, buf.len(), Mutation::Compressed(buf))
            .await
    }

    pub(crate) async fn pack_compressed(&self, data: &[u8]) -> Qcow2Result<Vec<Option<u64>>> {
        let cs = self.info.cluster_size();
        let codec = self.header.read().await.compression_type();
        let mut packed = Vec::new();
        let mut entries = Vec::new();
        let bits = 1_usize << self.info.refcount_order();
        let max_refs = if bits == 64 {
            u64::MAX
        } else {
            (1_u64 << bits) - 1
        };
        let mut references: Vec<u64> = Vec::new();
        for chunk in data.chunks(cs) {
            let mut padded = vec![0; cs];
            padded[..chunk.len()].copy_from_slice(chunk);
            let encoded = if codec == 1 {
                zstd::bulk::compress(&padded, 3).map_err(|_| invalid("Zstd compression failed"))?
            } else {
                miniz_oxide::deflate::compress_to_vec(&padded, 6)
            };
            if encoded.len() >= cs {
                entries.push(None);
            } else {
                let last = (packed.len() + encoded.len() - 1) / cs;
                references.resize(references.len().max(last + 1), 0);
                if references[packed.len() / cs..=last]
                    .iter()
                    .any(|n| *n >= max_refs)
                {
                    packed.resize(packed.len().div_ceil(cs) * cs, 0);
                }
                let last = (packed.len() + encoded.len() - 1) / cs;
                references.resize(references.len().max(last + 1), 0);
                for count in &mut references[packed.len() / cs..=last] {
                    *count += 1;
                }
                entries.push(Some((packed.len(), encoded.len())));
                packed.extend(encoded);
            }
        }
        let base = self.allocate_object(&packed).await?;
        let shift = 62 - (self.info.cluster_bits() - 8);
        entries
            .into_iter()
            .map(|entry| {
                entry
                    .map(|(at, len)| {
                        let host = base + at as u64;
                        if host >= 1_u64 << shift {
                            return Err(invalid("Compressed offset exceeds descriptor capacity"));
                        }
                        let sectors = ((host as usize & 511) + len).div_ceil(512);
                        Ok((1 << 62) | host | (((sectors - 1) as u64) << shift))
                    })
                    .transpose()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn codecs_require_stream_completion_and_exact_output_size() {
        for codec in [0, 1] {
            for length in [16383, 16384, 16385, 32768] {
                let input = vec![0x5a; length];
                let compressed = if codec == 0 {
                    miniz_oxide::deflate::compress_to_vec(&input, 6)
                } else {
                    zstd::bulk::compress(&input, 3).unwrap()
                };
                assert_eq!(decode(codec, &compressed, 16384).is_ok(), length == 16384);
                assert!(decode(codec, &compressed[..compressed.len() / 2], 16384).is_err());
            }
            assert!(decode(codec, &[0xff; 512], 16384).is_err());
        }
    }
}

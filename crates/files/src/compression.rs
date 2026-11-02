//! Optional wire compression. File identities, offsets, and durable journals
//! always use the original bytes, independently of this transport encoding.
use crate::{MAX_CHUNK, Response};
use std::{io, time::Duration};

const MIN_INPUT: usize = 4 * 1024;
const SAMPLE_PART: usize = 16 * 1024;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid compressed Files frame")
}

/// Decode at most one bounded zstd frame, rejecting unknown codecs, concatenated
/// frames, trailing bytes, and declared or actual output sizes above MAX_CHUNK.
/// Call this on a blocking worker when a response contains an encoded payload.
pub fn decode_data_response(response: Response) -> io::Result<Response> {
    let Response::EncodedData {
        offset,
        decoded_bytes,
        encoding,
        data,
    } = response
    else {
        return Ok(response);
    };
    let length = usize::try_from(decoded_bytes).map_err(|_| invalid())?;
    if encoding != "zstd"
        || length == 0
        || length > MAX_CHUNK
        || data.is_empty()
        || data.len() > MAX_CHUNK
        || zstd::zstd_safe::find_frame_compressed_size(&data).map_err(|_| invalid())? != data.len()
    {
        return Err(invalid());
    }
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(23))?;
    let decoded = decoder.decompress(&data, length).map_err(|_| invalid())?;
    if decoded.len() != length {
        return Err(invalid());
    }
    Ok(Response::Data {
        offset,
        data: decoded,
    })
}

/// Per-stream adaptive encoder. Sampling spans the beginning, middle, and end
/// of a chunk rather than assuming its header represents the whole file.
/// Poorly compressible/CPU-bound chunks trigger bounded periodic resampling.
#[derive(Default)]
pub struct AdaptiveEncoder {
    source: String,
    skip: usize,
    failures: u32,
    network_bytes_per_sec: Option<f64>,
}

impl AdaptiveEncoder {
    /// Supply measured downstream backpressure. Short buffered writes are not
    /// useful throughput measurements, so only substantial blocked writes are
    /// included. This estimate is a hint, never a correctness requirement.
    pub fn observe_write(&mut self, payload_bytes: usize, elapsed: Duration) {
        if payload_bytes < 32 * 1024 || elapsed < Duration::from_millis(2) {
            return;
        }
        let rate = payload_bytes as f64 / elapsed.as_secs_f64();
        self.network_bytes_per_sec = Some(match self.network_bytes_per_sec {
            Some(previous) => previous * 0.75 + rate * 0.25,
            None => rate,
        });
    }

    /// Return either original Data or a zstd level-1 EncodedData response.
    /// Storage and compression run together on the source's blocking worker.
    pub fn encode(&mut self, source: &str, response: Response) -> Response {
        let Response::Data { offset, data } = response else {
            return response;
        };
        if source != self.source {
            self.source.clear();
            self.source.push_str(source);
            self.skip = 0;
            self.failures = 0;
        }
        if data.len() < MIN_INPUT || data.len() > MAX_CHUNK {
            return Response::Data { offset, data };
        }
        if self.skip > 0 {
            self.skip -= 1;
            return Response::Data { offset, data };
        }
        if data.len() >= SAMPLE_PART * 6 {
            let sample_start = std::time::Instant::now();
            let positions = [
                0,
                data.len() / 2 - SAMPLE_PART / 2,
                data.len() - SAMPLE_PART,
            ];
            let mut packed = 0;
            for position in positions {
                match zstd::bulk::compress(&data[position..position + SAMPLE_PART], 1) {
                    Ok(bytes) => packed += bytes.len(),
                    Err(_) => {
                        self.back_off();
                        return Response::Data { offset, data };
                    }
                }
            }
            if !worth_encoding(
                SAMPLE_PART * 3,
                packed,
                sample_start.elapsed(),
                self.network_bytes_per_sec,
            ) {
                self.back_off();
                return Response::Data { offset, data };
            }
        }
        let started = std::time::Instant::now();
        let encoded = zstd::bulk::compress(&data, 1);
        match encoded {
            Ok(encoded)
                if worth_encoding(
                    data.len(),
                    encoded.len(),
                    started.elapsed(),
                    self.network_bytes_per_sec,
                ) =>
            {
                self.failures = 0;
                Response::EncodedData {
                    offset,
                    decoded_bytes: data.len() as u64,
                    encoding: "zstd".into(),
                    data: encoded,
                }
            }
            _ => {
                self.back_off();
                Response::Data { offset, data }
            }
        }
    }

    fn back_off(&mut self) {
        self.failures = (self.failures + 1).min(6);
        self.skip = 1 << self.failures;
    }
}

fn worth_encoding(original: usize, packed: usize, cpu: Duration, network: Option<f64>) -> bool {
    // Leave room for the additional encoding metadata and measurement noise.
    if packed.saturating_add(64) >= original || packed > original - original / 16 {
        return false;
    }
    // Compression must process original bytes faster than the observed raw
    // network pace. It can otherwise become the bottleneck of a streaming pipe.
    network.is_none_or(|rate| cpu.as_secs_f64() < original as f64 / rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(bytes: &[u8]) -> Response {
        Response::EncodedData {
            offset: 17,
            decoded_bytes: bytes.len() as u64,
            encoding: "zstd".into(),
            data: zstd::bulk::compress(bytes, 1).unwrap(),
        }
    }

    #[test]
    fn decoder_rejects_bombs_wrong_lengths_codecs_and_trailing_frames() {
        let bytes = vec![b'x'; MAX_CHUNK];
        assert_eq!(
            decode_data_response(encoded(&bytes)).unwrap(),
            Response::Data {
                offset: 17,
                data: bytes.clone()
            }
        );
        for declared in [0, bytes.len() as u64 - 1, bytes.len() as u64 + 1, u64::MAX] {
            let Response::EncodedData {
                offset,
                encoding,
                data,
                ..
            } = encoded(&bytes)
            else {
                unreachable!()
            };
            assert!(
                decode_data_response(Response::EncodedData {
                    offset,
                    decoded_bytes: declared,
                    encoding,
                    data
                })
                .is_err()
            );
        }
        let Response::EncodedData {
            offset,
            decoded_bytes,
            encoding,
            data,
        } = encoded(b"original")
        else {
            unreachable!()
        };
        for bad in [
            data[..data.len() - 1].to_vec(),
            [data.clone(), vec![0]].concat(),
            [data.clone(), data.clone()].concat(),
        ] {
            assert!(
                decode_data_response(Response::EncodedData {
                    offset,
                    decoded_bytes,
                    encoding: encoding.clone(),
                    data: bad
                })
                .is_err()
            );
        }
        assert!(
            decode_data_response(Response::EncodedData {
                offset,
                decoded_bytes,
                encoding: "other".into(),
                data
            })
            .is_err()
        );
    }

    #[test]
    fn adaptive_encoder_preserves_offsets_and_resamples_after_incompressible_data() {
        let mut encoder = AdaptiveEncoder::default();
        let mut random = vec![0; 256 * 1024];
        let mut seed = 0x1234_5678_u64;
        for byte in &mut random {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *byte = seed as u8;
        }
        assert!(matches!(
            encoder.encode(
                "checkpoint:v1",
                Response::Data {
                    offset: 0,
                    data: random.clone()
                }
            ),
            Response::Data { .. }
        ));
        let bytes = vec![b'x'; random.len()];
        let mut found = false;
        for index in 1..=4 {
            let offset = index * bytes.len() as u64;
            let response = encoder.encode(
                "checkpoint:v1",
                Response::Data {
                    offset,
                    data: bytes.clone(),
                },
            );
            found |= matches!(response, Response::EncodedData { .. });
            assert_eq!(
                decode_data_response(response).unwrap(),
                Response::Data {
                    offset,
                    data: bytes.clone()
                }
            );
        }
        assert!(
            found,
            "later file contents must be sampled after an initial failure"
        );
    }

    #[test]
    fn compression_policy_checks_cpu_against_observed_network_pace() {
        assert!(worth_encoding(
            1_000_000,
            500_000,
            Duration::from_millis(10),
            Some(50_000_000.0)
        ));
        assert!(!worth_encoding(
            1_000_000,
            500_000,
            Duration::from_millis(10),
            Some(200_000_000.0)
        ));
        assert!(!worth_encoding(1_000_000, 990_000, Duration::ZERO, None));
        let mut encoder = AdaptiveEncoder::default();
        encoder.observe_write(4_000_000, Duration::from_millis(40));
        assert_eq!(encoder.network_bytes_per_sec, Some(100_000_000.0));
        encoder.observe_write(1, Duration::from_secs(1));
        assert_eq!(encoder.network_bytes_per_sec, Some(100_000_000.0));
    }
}

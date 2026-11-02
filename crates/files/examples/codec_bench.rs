//! Opt-in CPU/size comparison; this does not measure network or disk throughput.
//! cargo run --release -p wispnet-files --example codec_bench -- [optional-file]
use std::{
    fs::File,
    io::{self, Read},
    time::{Duration, Instant},
};
use wispnet_files::{AdaptiveEncoder, MAX_CHUNK, Response, decode_data_response};

#[derive(Default)]
struct Totals {
    input: u64,
    wire: u64,
    encode: Duration,
    decode: Duration,
    compressed: u64,
    raw: u64,
}

fn main() -> io::Result<()> {
    if let Some(path) = std::env::args_os().nth(1) {
        return bench("file", &mut File::open(path)?);
    }
    let count = MAX_CHUNK * 16;
    let text = b"{\"step\":42,\"loss\":0.123,\"status\":\"training\"}\n";
    let mut data: Vec<u8> = text.iter().copied().cycle().take(count).collect();
    bench("repeated-json", &mut data.as_slice())?;
    let mut seed = 0x1234_5678_9abc_def0_u64;
    for byte in &mut data {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *byte = seed as u8;
    }
    bench("incompressible", &mut data.as_slice())?;
    // A changed content region checks that later chunks are sampled even after
    // an incompressible beginning. This is deliberately not a checkpoint model.
    data[MAX_CHUNK * 4..].fill(0);
    bench("random-then-zero", &mut data.as_slice())
}

fn bench(label: &str, source: &mut impl Read) -> io::Result<()> {
    let mut adaptive = AdaptiveEncoder::default();
    let mut totals = [Totals::default(), Totals::default(), Totals::default()];
    let mut chunk = vec![0; MAX_CHUNK];
    let mut offset = 0;
    loop {
        let mut length = 0;
        while length < chunk.len() {
            let read = source.read(&mut chunk[length..])?;
            if read == 0 {
                break;
            }
            length += read;
        }
        if length == 0 {
            break;
        }
        for (mode, result) in totals.iter_mut().enumerate() {
            // Copying input is outside the timed section for every mode. The
            // transport already owns a read buffer before choosing its encoding.
            let input = chunk[..length].to_vec();
            let started = Instant::now();
            let response = match mode {
                0 => Response::Data {
                    offset,
                    data: input,
                },
                1 => {
                    let packed = zstd::bulk::compress(&input, 1)?;
                    // An expanded chunk cannot fit the bounded wire payload.
                    // This mode still measures the cost of trying every chunk.
                    if packed.len() > MAX_CHUNK {
                        Response::Data {
                            offset,
                            data: input,
                        }
                    } else {
                        Response::EncodedData {
                            offset,
                            decoded_bytes: length as u64,
                            encoding: "zstd".into(),
                            data: packed,
                        }
                    }
                }
                _ => adaptive.encode(
                    label,
                    Response::Data {
                        offset,
                        data: input,
                    },
                ),
            };
            result.encode += started.elapsed();
            result.input += length as u64;
            match &response {
                Response::Data { data, .. } => {
                    result.wire += data.len() as u64;
                    result.raw += 1;
                }
                Response::EncodedData { data, .. } => {
                    result.wire += data.len() as u64;
                    result.compressed += 1;
                }
                _ => unreachable!(),
            }
            let started = Instant::now();
            let decoded = decode_data_response(response)?;
            result.decode += started.elapsed();
            let Response::Data {
                offset: actual,
                data,
            } = decoded
            else {
                unreachable!()
            };
            assert_eq!(actual, offset);
            assert_eq!(data, chunk[..length]);
        }
        offset += length as u64;
    }
    for (mode, result) in ["raw", "zstd-1", "adaptive"].into_iter().zip(totals) {
        println!(
            "{}",
            serde_json::json!({
                "dataset": label, "mode": mode, "original_bytes": result.input,
                "payload_bytes": result.wire, "encode_seconds": result.encode.as_secs_f64(),
                "decode_seconds": result.decode.as_secs_f64(),
                "compressed_chunks": result.compressed, "raw_chunks": result.raw,
                "network_and_storage_measured": false
            })
        );
    }
    Ok(())
}

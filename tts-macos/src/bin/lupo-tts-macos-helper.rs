// SPDX-License-Identifier: Apache-2.0

#[path = "../protocol.rs"]
mod protocol;
#[path = "../synthesis.rs"]
mod synthesis;

use std::io::{self, BufRead, BufReader, BufWriter, Write};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use protocol::{PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request = match serde_json::from_str::<RequestEnvelope>(trimmed) {
            Ok(request) => request,
            Err(err) => {
                write_response(
                    &mut writer,
                    &ResponseEnvelope::Error {
                        request_id: None,
                        code: "invalid_request".to_string(),
                        message: format!("failed to decode request: {err}"),
                    },
                )?;
                continue;
            }
        };

        match request {
            RequestEnvelope::Hello { protocol_version } => {
                if protocol_version != PROTOCOL_VERSION {
                    write_response(
                        &mut writer,
                        &ResponseEnvelope::Error {
                            request_id: None,
                            code: "protocol_version_mismatch".to_string(),
                            message: format!(
                                "expected protocol version {PROTOCOL_VERSION}, got {protocol_version}"
                            ),
                        },
                    )?;
                    break;
                }

                write_response(
                    &mut writer,
                    &ResponseEnvelope::HelloOk {
                        protocol_version: PROTOCOL_VERSION,
                    },
                )?;
            }
            RequestEnvelope::Synthesize { request_id, text } => {
                let response = match synthesis::synthesize_on_main_thread(&text) {
                    Ok(wav_bytes) => ResponseEnvelope::SynthesizeOk {
                        request_id,
                        wav_base64: BASE64_STANDARD.encode(wav_bytes),
                    },
                    Err(err) => ResponseEnvelope::Error {
                        request_id: Some(request_id),
                        code: "synthesis_failed".to_string(),
                        message: err.to_string(),
                    },
                };
                write_response(&mut writer, &response)?;
            }
            RequestEnvelope::Shutdown { request_id } => {
                write_response(&mut writer, &ResponseEnvelope::ShutdownOk { request_id })?;
                break;
            }
        }
    }

    writer.flush()
}

fn write_response<W>(writer: &mut BufWriter<W>, response: &ResponseEnvelope) -> io::Result<()>
where
    W: Write,
{
    let json = serde_json::to_string(response)
        .map_err(|err| io::Error::other(format!("failed to encode response: {err}")))?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

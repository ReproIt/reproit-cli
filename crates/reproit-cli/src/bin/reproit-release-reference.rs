use std::io::{Read as _, Write as _};

use serde::{Deserialize, Serialize};

const MAX_INPUT_BYTES: u64 = 1_048_576;
const MAX_RECORDS: usize = 1_024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRecord {
    case_id: String,
    input_text: String,
}

#[derive(Serialize)]
struct OutputRecord<'a> {
    case_id: &'a str,
    output_text: &'a str,
}

fn main() -> std::process::ExitCode {
    let Some(mode) = std::env::args().nth(1) else {
        return std::process::ExitCode::from(2);
    };
    if mode == "nonzero" {
        return std::process::ExitCode::from(3);
    }
    let mut input = String::new();
    if std::io::stdin()
        .take(MAX_INPUT_BYTES + 1)
        .read_to_string(&mut input)
        .is_err()
        || input.len() as u64 > MAX_INPUT_BYTES
    {
        return std::process::ExitCode::from(2);
    }
    if mode == "invalid" {
        let _ = std::io::stdout().write_all(b"not-json\n");
        return std::process::ExitCode::SUCCESS;
    }
    let mut output = std::io::stdout().lock();
    for (index, line) in input.lines().enumerate() {
        if index >= MAX_RECORDS {
            return std::process::ExitCode::from(2);
        }
        let Ok(record) = serde_json::from_str::<InputRecord>(line) else {
            return std::process::ExitCode::from(2);
        };
        let _ = record.input_text;
        let output_text = if mode == "regression" { "red" } else { "blue" };
        if serde_json::to_writer(
            &mut output,
            &OutputRecord {
                case_id: &record.case_id,
                output_text,
            },
        )
        .is_err()
            || output.write_all(b"\n").is_err()
        {
            return std::process::ExitCode::from(2);
        }
    }
    std::process::ExitCode::SUCCESS
}

//! Shared types for the REPE+BEVE vs gRPC+protobuf shootout.

/// Generated protobuf messages and tonic service stubs.
pub mod pb {
    tonic::include_proto!("shootout");
}

use serde::{Deserialize, Serialize};

/// REPE/BEVE mirror of `pb::Small`. The shootout compares equivalent payloads,
/// so the serde struct must carry exactly the same fields as the proto message.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Small {
    pub id: u64,
    pub name: String,
    pub ok: bool,
    pub score: f64,
}

impl Small {
    pub fn sample() -> Self {
        Small {
            id: 0x1122_3344_5566_7788,
            name: "spectrometer-07".to_string(),
            ok: true,
            score: 0.987654321,
        }
    }
}

/// Element counts swept by both the codec microbench and the e2e harness.
pub const ELEM_COUNTS: &[usize] = &[64, 4_096, 65_536, 1_048_576];

pub fn sample_f64(n: usize) -> Vec<f64> {
    (0..n).map(|i| i as f64 * 0.5).collect()
}

pub fn sample_i64(n: usize) -> Vec<i64> {
    // Mix of magnitudes so sint64 varint lengths vary realistically (1-10 bytes)
    // rather than all landing in the cheap single-byte range.
    (0..n).map(|i| (i as i64) * 0x0001_0001 - 7).collect()
}

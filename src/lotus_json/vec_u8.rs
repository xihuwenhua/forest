// Copyright 2019-2025 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use super::*;

// This code looks odd so we can
// - use #[serde(with = "...")]
// - de/ser empty vecs as null
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct VecU8LotusJson(Option<Inner>);

impl JsonSchema for VecU8LotusJson {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Base64String".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": ["string", "null"]
        })
    }
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
struct Inner(#[serde(with = "base64_standard")] Vec<u8>);

impl HasLotusJson for Vec<u8> {
    type LotusJson = VecU8LotusJson;

    #[cfg(test)]
    fn snapshots() -> Vec<(serde_json::Value, Self)> {
        vec![
            (json!("aGVsbG8gd29ybGQh"), Vec::from_iter(*b"hello world!")),
            (json!(null), Vec::new()),
        ]
    }

    fn into_lotus_json(self) -> Self::LotusJson {
        match self.is_empty() {
            true => VecU8LotusJson(None),
            false => VecU8LotusJson(Some(Inner(self))),
        }
    }

    fn from_lotus_json(value: Self::LotusJson) -> Self {
        match value {
            VecU8LotusJson(Some(Inner(vec))) => vec,
            VecU8LotusJson(None) => Vec::new(),
        }
    }
}

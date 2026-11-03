//! Retrieval-study switches (branch `retrieval-study` only): `WS_EXP="k1=1.5,b=0.5,…"`.
//! An unset key keeps the shipped behaviour.

use std::{collections::HashMap, sync::LazyLock};

static EXP: LazyLock<HashMap<String, f32>> = LazyLock::new(|| {
    std::env::var("WS_EXP")
        .unwrap_or_default()
        .split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse().ok()?))
        })
        .collect()
});

pub fn get(key: &str, default: f32) -> f32 {
    EXP.get(key).copied().unwrap_or(default)
}

//! Serde helpers shared by the `cargo metadata` and compiler-message parsers.

use serde::{Deserialize, Deserializer};

/// Deserialize `null` as `T::default()`. Pair with `#[serde(default)]` so a missing field is
/// defaulted too: cargo's JSON is generated, but a wrapper or an older cargo may differ in which
/// optional fields it writes.
pub(crate) fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

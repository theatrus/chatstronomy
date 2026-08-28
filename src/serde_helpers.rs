//! Shared serde deserializers for NINA's "unknown" payload shapes.
//!
//! N.I.N.A. payloads have two recurring ways of saying "this value isn't
//! available":
//!
//!   * the field comes back as an empty JSON array `[]` (e.g. filter
//!     wheel `Name`/`Id` when no slot is selected, or every TS-TARGETSTART
//!     `Coordinates.*` when the target has no coords),
//!   * the field comes back as the JSON *string* `"NaN"` (the focuser
//!     `Temperature` when no sensor is attached; also `"Infinity"` /
//!     `"-Infinity"` show up in autofocus reports for unreached limits).
//!
//! These helpers accept the normal typed payload plus those sentinels and
//! map them to a per-type unknown value (NaN for floats, `-1` for filter
//! IDs, empty string for names/coords).

use serde::{Deserialize, Deserializer, de::Error};

/// `f64` field that may also arrive as a stringified `"NaN"` / `"Infinity"`
/// / `"-Infinity"`, as an empty array `[]`, or as `null`. All "unknown"
/// sentinels resolve to the appropriate `f64` value (`NAN` by default).
pub fn de_f64_tolerant<'de, D>(d: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => n.as_f64().ok_or_else(|| D::Error::custom("not f64")),
        serde_json::Value::String(s) => match s.as_str() {
            "NaN" => Ok(f64::NAN),
            "Infinity" => Ok(f64::INFINITY),
            "-Infinity" => Ok(f64::NEG_INFINITY),
            other => other.parse::<f64>().map_err(D::Error::custom),
        },
        serde_json::Value::Array(a) if a.is_empty() => Ok(f64::NAN),
        serde_json::Value::Null => Ok(f64::NAN),
        other => Err(D::Error::custom(format!(
            "expected number, NaN string, [], or null; got {other}"
        ))),
    }
}

/// Finite `f64` field for focus positions. Newer autofocus engines may return
/// a fractional fitted position; the legacy empty-array sentinel retains its
/// historical `-1` value. Null and non-finite sentinels are rejected because
/// they cannot be used safely as chart coordinates.
pub fn de_finite_f64<'de, D>(d: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(d)?;
    let value = match raw {
        serde_json::Value::Number(number) => {
            number.as_f64().ok_or_else(|| D::Error::custom("not f64"))?
        }
        serde_json::Value::String(value) => value.parse::<f64>().map_err(D::Error::custom)?,
        serde_json::Value::Array(values) if values.is_empty() => return Ok(-1.0),
        other => {
            return Err(D::Error::custom(format!(
                "expected a finite number or []; got {other}"
            )));
        }
    };
    value
        .is_finite()
        .then_some(value)
        .ok_or_else(|| D::Error::custom("expected a finite number"))
}

/// Optional `f64` field using the same N.I.N.A. sentinel handling as
/// [`de_f64_tolerant`]. Missing fields are handled by `#[serde(default)]` at
/// the call site; explicit `null`, `[]`, `"NaN"`, and infinities become
/// `None` so unavailable device readings never reach chat rendering.
pub fn de_optional_finite_f64<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = de_f64_tolerant(d)?;
    Ok(value.is_finite().then_some(value))
}

/// Optional non-negative count. Hocus Focus uses `-1` when a star-count
/// statistic was not calculated; null and an empty array are also treated as
/// unavailable for compatibility with N.I.N.A.'s common sentinel shapes.
pub fn de_optional_nonnegative_i32<'de, D>(d: D) -> Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(d)?;
    let value = match raw {
        serde_json::Value::Null => return Ok(None),
        serde_json::Value::Array(values) if values.is_empty() => return Ok(None),
        serde_json::Value::Number(number) => number
            .as_i64()
            .ok_or_else(|| D::Error::custom("not an integral i32"))?,
        serde_json::Value::String(value) => value.parse::<i64>().map_err(D::Error::custom)?,
        other => {
            return Err(D::Error::custom(format!(
                "expected a non-negative integer, [], or null; got {other}"
            )));
        }
    };
    if value < 0 {
        return Ok(None);
    }
    Ok(Some(i32::try_from(value).map_err(D::Error::custom)?))
}

/// `String` field that may also arrive as an empty array `[]` — empty
/// arrays become the empty string.
pub fn de_string_tolerant<'de, D>(d: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Array(a) if a.is_empty() => Ok(String::new()),
        other => Err(D::Error::custom(format!(
            "expected string or []; got {other}"
        ))),
    }
}

/// `i32` field that may also arrive as an empty array `[]` — empty arrays
/// become `-1` (used as the "unknown filter id" sentinel).
pub fn de_i32_tolerant<'de, D>(d: D) -> Result<i32, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => {
            if let Some(value) = n.as_i64() {
                return i32::try_from(value).map_err(D::Error::custom);
            }
            // Native N.I.N.A. autofocus reports model focus positions as
            // doubles (`4068.0`) while older payloads emit integers.
            let value = n
                .as_f64()
                .filter(|value| value.is_finite() && value.fract() == 0.0)
                .ok_or_else(|| D::Error::custom("not an integral i32"))?;
            if value < i32::MIN as f64 || value > i32::MAX as f64 {
                return Err(D::Error::custom("integer is outside the i32 range"));
            }
            Ok(value as i32)
        }
        serde_json::Value::Array(a) if a.is_empty() => Ok(-1),
        other => Err(D::Error::custom(format!(
            "expected integer or []; got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct F(#[serde(deserialize_with = "de_f64_tolerant")] f64);
    #[derive(Deserialize)]
    struct Finite(#[serde(deserialize_with = "de_finite_f64")] f64);
    #[derive(Deserialize)]
    struct S(#[serde(deserialize_with = "de_string_tolerant")] String);
    #[derive(Deserialize)]
    struct I(#[serde(deserialize_with = "de_i32_tolerant")] i32);
    #[derive(Deserialize)]
    struct O(#[serde(deserialize_with = "de_optional_finite_f64")] Option<f64>);
    #[derive(Deserialize)]
    struct C(#[serde(deserialize_with = "de_optional_nonnegative_i32")] Option<i32>);

    #[test]
    fn f64_number() {
        let F(v) = serde_json::from_str("14.7").unwrap();
        assert!((v - 14.7).abs() < 1e-9);
    }

    #[test]
    fn f64_nan_string() {
        let F(v) = serde_json::from_str("\"NaN\"").unwrap();
        assert!(v.is_nan());
    }

    #[test]
    fn f64_inf_strings() {
        let F(v) = serde_json::from_str("\"Infinity\"").unwrap();
        assert!(v.is_infinite() && v > 0.0);
        let F(v) = serde_json::from_str("\"-Infinity\"").unwrap();
        assert!(v.is_infinite() && v < 0.0);
    }

    #[test]
    fn f64_empty_array_is_nan() {
        let F(v) = serde_json::from_str("[]").unwrap();
        assert!(v.is_nan());
    }

    #[test]
    fn f64_null_is_nan() {
        let F(v) = serde_json::from_str("null").unwrap();
        assert!(v.is_nan());
    }

    #[test]
    fn finite_f64_accepts_fractional_positions_and_rejects_sentinels() {
        let Finite(value) = serde_json::from_str("4188.955065493704").unwrap();
        assert!((value - 4188.955065493704).abs() < 1e-12);
        let Finite(value) = serde_json::from_str("\"4068.25\"").unwrap();
        assert_eq!(value, 4068.25);
        let Finite(value) = serde_json::from_str("[]").unwrap();
        assert_eq!(value, -1.0);
        for json in ["null", "\"NaN\"", "\"Infinity\"", "\"-Infinity\""] {
            assert!(
                serde_json::from_str::<Finite>(json).is_err(),
                "sentinel {json}"
            );
        }
    }

    #[test]
    fn optional_finite_f64_discards_nina_unknown_sentinels() {
        let O(value) = serde_json::from_str("12.5").unwrap();
        assert_eq!(value, Some(12.5));
        for json in ["null", "[]", "\"NaN\"", "\"Infinity\"", "\"-Infinity\""] {
            let O(value) = serde_json::from_str(json).unwrap();
            assert_eq!(value, None, "sentinel {json}");
        }
    }

    #[test]
    fn optional_nonnegative_i32_discards_hocus_unknown_sentinels() {
        let C(value) = serde_json::from_str("83").unwrap();
        assert_eq!(value, Some(83));
        let C(value) = serde_json::from_str("\"119\"").unwrap();
        assert_eq!(value, Some(119));
        for json in ["-1", "null", "[]"] {
            let C(value) = serde_json::from_str(json).unwrap();
            assert_eq!(value, None, "sentinel {json}");
        }
    }

    #[test]
    fn string_normal_and_empty_array() {
        let S(s) = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(s, "hello");
        let S(s) = serde_json::from_str("[]").unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn int_normal_and_empty_array() {
        let I(i) = serde_json::from_str("42").unwrap();
        assert_eq!(i, 42);
        let I(i) = serde_json::from_str("42.0").unwrap();
        assert_eq!(i, 42);
        let I(i) = serde_json::from_str("[]").unwrap();
        assert_eq!(i, -1);
    }

    #[test]
    fn int_rejects_fractional_and_out_of_range_numbers() {
        assert!(serde_json::from_str::<I>("42.5").is_err());
        assert!(serde_json::from_str::<I>("2147483648").is_err());
    }
}

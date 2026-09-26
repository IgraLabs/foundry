use eyre::{Result, bail, eyre};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fmt};

/// Canonicalization of bundle-identity `payloadVersion` 1 and 2, and of manifests without
/// `bundleIdentity` (legacy bundles).
const LEGACY_CANONICALIZATION: &str = "json-c14n-sorted-keys-no-whitespace-utf8";
/// Canonicalization of bundle-identity `payloadVersion` 3.
const JCS_CANONICALIZATION: &str = "rfc8785-jcs-utf8";
/// `Number.MAX_SAFE_INTEGER`: the largest integer whose ES6 serialization, which RFC 8785
/// mandates, is guaranteed to be its plain decimal form.
const JCS_MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// Prefix of the object keys that serde_json reserves for its `arbitrary_precision` and
/// `raw_value` features, both enabled in this workspace. An object with such a key is read as a
/// number or as embedded JSON, not as an object.
const SERDE_JSON_RESERVED_KEY_PREFIX: &str = "$serde_json::private::";

#[derive(Clone, Debug, Serialize)]
pub struct VerifyBundleIntegralReport {
    pub ok: bool,
    pub algorithm: String,
    pub canonicalization: String,
    pub expected_value: String,
    pub computed_value: String,
    pub matches: bool,
}

/// Parses a bundle manifest for [`verify_bundle_integral`], refusing JSON that other parsers
/// could read differently:
///
/// - duplicate object keys, of which serde_json keeps the last and other parsers may keep the
///   first. RFC 8785 also requires I-JSON input (RFC 7493), which forbids them;
/// - keys starting with `$serde_json::private::`, which serde_json does not read as objects;
/// - numbers that are not 64-bit integers, because serde_json passes them through the same reserved
///   key. Manifests only carry integers.
pub fn parse_bundle_manifest(json: &str) -> Result<Value> {
    serde_json::from_str::<StrictJson>(json)?;
    Ok(serde_json::from_str(json)?)
}

pub fn verify_bundle_integral(manifest: &Value) -> Result<VerifyBundleIntegralReport> {
    let Some(root) = manifest.as_object() else {
        bail!("manifest must be a JSON object");
    };

    let bundle_integral = root
        .get("bundleIntegral")
        .ok_or_else(|| eyre!("manifest is missing top-level bundleIntegral"))?;
    let Some(bundle_integral_obj) = bundle_integral.as_object() else {
        bail!("manifest bundleIntegral must be a JSON object");
    };

    let algorithm = bundle_integral_obj
        .get("algorithm")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("manifest bundleIntegral.algorithm must be a string"))?;
    if algorithm != "sha256" {
        bail!("unsupported bundleIntegral.algorithm: {algorithm}");
    }

    let canonicalization = bundle_integral_obj
        .get("canonicalization")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("manifest bundleIntegral.canonicalization must be a string"))?;
    if canonicalization != LEGACY_CANONICALIZATION && canonicalization != JCS_CANONICALIZATION {
        bail!("unsupported bundleIntegral.canonicalization: {canonicalization}");
    }
    check_canonicalization_matches_payload_version(root, canonicalization)?;

    let expected_value = bundle_integral_obj
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("manifest bundleIntegral.value must be a string"))?
        .to_owned();

    let prepared = prepare_bundle_integral_payload(manifest)?;
    let canonical_json = if canonicalization == JCS_CANONICALIZATION {
        jcs_string(&prepared)?
    } else {
        canonical_json_string(&prepared)
    };
    let computed_value = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));

    let matches = computed_value == expected_value;

    Ok(VerifyBundleIntegralReport {
        ok: matches,
        algorithm: algorithm.to_owned(),
        canonicalization: canonicalization.to_owned(),
        expected_value,
        computed_value,
        matches,
    })
}

/// The bundle-identity `payloadVersion` selects the canonicalization: 1 and 2 use the legacy
/// one and 3 uses RFC 8785 JCS, so a manifest never mixes the two. Any other `payloadVersion` is
/// refused. Manifests without `bundleIdentity` predate it and use the legacy canonicalization.
fn check_canonicalization_matches_payload_version(
    root: &Map<String, Value>,
    canonicalization: &str,
) -> Result<()> {
    let expected = match root.get("bundleIdentity") {
        None => LEGACY_CANONICALIZATION,
        Some(identity) => {
            let payload_version = identity
                .as_object()
                .ok_or_else(|| eyre!("manifest bundleIdentity must be a JSON object"))?
                .get("payloadVersion")
                .unwrap_or(&Value::Null);
            match payload_version.as_u64() {
                Some(1 | 2) => LEGACY_CANONICALIZATION,
                Some(3) => JCS_CANONICALIZATION,
                _ => bail!("unsupported bundleIdentity.payloadVersion: {payload_version}"),
            }
        }
    };
    if canonicalization != expected {
        bail!(
            "bundleIntegral.canonicalization {canonicalization} does not match \
             bundleIdentity.payloadVersion, which requires {expected}"
        );
    }
    Ok(())
}

fn prepare_bundle_integral_payload(manifest: &Value) -> Result<Value> {
    let mut prepared = manifest.clone();
    let Some(root) = prepared.as_object_mut() else {
        bail!("manifest must be a JSON object");
    };

    root.remove("signature");

    let bundle_integral = root
        .get_mut("bundleIntegral")
        .ok_or_else(|| eyre!("manifest is missing top-level bundleIntegral"))?;
    let Some(bundle_integral_obj) = bundle_integral.as_object_mut() else {
        bail!("manifest bundleIntegral must be a JSON object");
    };
    bundle_integral_obj.remove("value");

    Ok(prepared)
}

fn canonical_json_string(value: &Value) -> String {
    let mut out = String::new();
    write_canonical_json(value, &mut out);
    out
}

fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            out.push_str(&serde_json::to_string(value).expect("primitive JSON serialization"))
        }
        Value::Array(values) => {
            out.push('[');
            for (idx, item) in values.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => write_canonical_object(map, out),
    }
}

fn write_canonical_object(map: &Map<String, Value>, out: &mut String) {
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_unstable();

    out.push('{');
    for (idx, key) in keys.into_iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(key).expect("JSON object key serialization"));
        out.push(':');
        write_canonical_json(&map[key], out);
    }
    out.push('}');
}

/// RFC 8785 (JCS) serialization, restricted to the subset these manifests use.
///
/// Strings and keys use serde_json's escaping, which matches RFC 8785 section 3.2.2.2: short
/// escapes for `\b \t \n \f \r`, lowercase `\u00xx` for other control characters, everything
/// else as literal UTF-8. Object keys are ordered by UTF-16 code units (section 3.2.3), which
/// differs from byte order for characters outside the Basic Multilingual Plane. Numbers are
/// limited to integers with `|n| <= 2^53 - 1`, whose ES6 form is the plain decimal; anything
/// else is refused rather than risk hashing a form the producer did not use.
fn jcs_string(value: &Value) -> Result<String> {
    let mut out = String::new();
    write_jcs(value, &mut out)?;
    Ok(out)
}

fn write_jcs(value: &Value, out: &mut String) -> Result<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => {
            out.push_str(&serde_json::to_string(value)?)
        }
        Value::Number(number) => out.push_str(&jcs_number(number)?),
        Value::Array(values) => {
            out.push('[');
            for (idx, item) in values.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_jcs(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));

            out.push('{');
            for (idx, key) in keys.into_iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key)?);
                out.push(':');
                write_jcs(&map[key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn jcs_number(number: &Number) -> Result<String> {
    if let Some(value) = number.as_u64() {
        if value <= JCS_MAX_SAFE_INTEGER {
            return Ok(value.to_string());
        }
    } else if let Some(value) = number.as_i64()
        && value.unsigned_abs() <= JCS_MAX_SAFE_INTEGER
    {
        return Ok(value.to_string());
    }
    bail!(
        "{JCS_CANONICALIZATION}: number {number} is outside the supported exact-integer subset \
         (|n| <= 2^53 - 1)"
    )
}

/// Validation-only view of a JSON document, see [`parse_bundle_manifest`].
struct StrictJson;

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON with unique keys and only 64-bit integer numbers")
    }

    fn visit_unit<E>(self) -> Result<StrictJson, E> {
        Ok(StrictJson)
    }

    fn visit_bool<E>(self, _: bool) -> Result<StrictJson, E> {
        Ok(StrictJson)
    }

    fn visit_i64<E>(self, _: i64) -> Result<StrictJson, E> {
        Ok(StrictJson)
    }

    fn visit_u64<E>(self, _: u64) -> Result<StrictJson, E> {
        Ok(StrictJson)
    }

    fn visit_str<E>(self, _: &str) -> Result<StrictJson, E> {
        Ok(StrictJson)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<StrictJson, A::Error> {
        while seq.next_element::<StrictJson>()?.is_some() {}
        Ok(StrictJson)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<StrictJson, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if key.starts_with(SERDE_JSON_RESERVED_KEY_PREFIX) {
                return Err(de::Error::custom(format_args!(
                    "unsupported number or reserved key {key:?}: only 64-bit integers are \
                     accepted, and keys must not start with {SERDE_JSON_RESERVED_KEY_PREFIX:?}"
                )));
            }
            if keys.contains(&key) {
                return Err(de::Error::custom(format_args!("duplicate object key {key:?}")));
            }
            map.next_value::<StrictJson>()?;
            keys.insert(key);
        }
        Ok(StrictJson)
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_json_string, jcs_string, parse_bundle_manifest, verify_bundle_integral};
    use serde_json::json;

    // Expected values in the rfc8785-jcs-utf8 tests below were produced by an
    // independent Python RFC 8785 implementation (keys ordered by UTF-16 code
    // units), not by this module.
    const JCS_MANIFEST_INTEGRAL: &str =
        "8ebbd8c7ef7d5500216da3232ff26c5bff3a18175ddd27636ba80c252d4ca599";

    fn jcs_manifest() -> serde_json::Value {
        json!({
            "schemaVersion": 4,
            "kind": "bundle",
            "bundleIdentity": {"payloadVersion": 3, "value": "00"},
            "bundleIntegral": {
                "algorithm": "sha256",
                "canonicalization": "rfc8785-jcs-utf8",
                "value": JCS_MANIFEST_INTEGRAL
            },
            "files": [
                {"path": "derived/b.json", "sha256": "11", "size": 12},
                {"path": "derived/a.json", "sha256": "22", "size": 3}
            ],
            "Zeta": true,
            "note": "caf\u{e9} \u{20ac} tab\there \"q\" line\nbreak",
            "signature": {"algorithm": "sha256-sign", "value": "sig"}
        })
    }

    fn legacy_manifest() -> serde_json::Value {
        json!({
            "schemaVersion": 1,
            "kind": "bundle",
            "bundleIntegral": {
                "algorithm": "sha256",
                "canonicalization": "json-c14n-sorted-keys-no-whitespace-utf8",
                "value": "796b22341072c0a219c0ff865623b712a5318746065f66cc3e0cf8cb2f01cfb9"
            },
            "files": [{"path":"a","sha256":"x"}],
            "signature": {"algorithm":"sha256-sign"}
        })
    }

    #[test]
    fn jcs_sorts_keys_by_utf16_code_units_rfc8785_example() {
        // RFC 8785 section 3.2.3 sorting example.
        let value = json!({
            "\u{20ac}": "Euro Sign",
            "\r": "Carriage Return",
            "\u{fb33}": "Hebrew Letter Dalet With Dagesh",
            "1": "One",
            "\u{1f600}": "Emoji: Grinning Face",
            "\u{80}": "Control",
            "\u{f6}": "Latin Small Letter O With Diaeresis"
        });
        let expected = "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{80}\":\"Control\",\
                        \"\u{f6}\":\"Latin Small Letter O With Diaeresis\",\"\u{20ac}\":\"Euro Sign\",\
                        \"\u{1f600}\":\"Emoji: Grinning Face\",\
                        \"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}";
        assert_eq!(jcs_string(&value).expect("jcs"), expected);
        // Byte / code-point ordering puts the emoji last; JCS must not.
        assert_ne!(canonical_json_string(&value), expected);
    }

    #[test]
    fn verify_bundle_integral_accepts_rfc8785_jcs_payload_version_3() {
        let report = verify_bundle_integral(&jcs_manifest()).expect("jcs manifest should verify");
        assert!(report.ok);
        assert!(report.matches);
        assert_eq!(report.canonicalization, "rfc8785-jcs-utf8");
        assert_eq!(report.computed_value, JCS_MANIFEST_INTEGRAL);
    }

    #[test]
    fn verify_bundle_integral_jcs_detects_tampering() {
        let mut manifest = jcs_manifest();
        manifest["files"][0]["sha256"] = json!("12");
        let report = verify_bundle_integral(&manifest).expect("tampered manifest still parses");
        assert!(!report.ok);
        assert!(!report.matches);
    }

    #[test]
    fn jcs_refuses_numbers_outside_the_exact_integer_subset() {
        assert!(jcs_string(&json!({"a": 1.5})).is_err());
        assert!(jcs_string(&json!({"a": 9_007_199_254_740_992u64})).is_err());
        assert_eq!(
            jcs_string(&json!({"a": -9_007_199_254_740_991i64, "b": 0})).expect("jcs"),
            r#"{"a":-9007199254740991,"b":0}"#
        );
    }

    #[test]
    fn jcs_requires_payload_version_3() {
        let mut manifest = jcs_manifest();
        manifest["bundleIdentity"]["payloadVersion"] = json!(2);
        assert!(verify_bundle_integral(&manifest).is_err());

        let mut manifest = jcs_manifest();
        manifest.as_object_mut().expect("object").remove("bundleIdentity");
        assert!(verify_bundle_integral(&manifest).is_err());
    }

    #[test]
    fn legacy_canonicalization_rejects_payload_version_3() {
        let mut manifest = legacy_manifest();
        manifest["bundleIdentity"] = json!({"payloadVersion": 3, "value": "00"});
        assert!(verify_bundle_integral(&manifest).is_err());

        // payloadVersion 2 with the legacy canonicalization still verifies.
        let mut manifest = legacy_manifest();
        manifest["bundleIdentity"] = json!({"payloadVersion": 2, "value": "00"});
        manifest["bundleIntegral"]["value"] =
            json!("5dc9fad17d9e05f884e61c4402c1b1ff377ef435955a3e6a38f56ab77a6fde64");
        let report = verify_bundle_integral(&manifest).expect("legacy v2 manifest should verify");
        assert!(report.matches);
    }

    #[test]
    fn unknown_canonicalization_is_rejected() {
        let mut manifest = legacy_manifest();
        manifest["bundleIntegral"]["canonicalization"] = json!("rfc8785-something-else");
        assert!(verify_bundle_integral(&manifest).is_err());
    }

    #[test]
    fn parse_bundle_manifest_round_trips_manifests() {
        for manifest in [jcs_manifest(), legacy_manifest()] {
            assert_eq!(parse_bundle_manifest(&manifest.to_string()).expect("parse"), manifest);
        }
    }

    #[test]
    fn parse_bundle_manifest_rejects_duplicate_keys() {
        for json in [
            r#"{"a":1,"a":2}"#,
            r#"{"files":[{"path":"a","path":"b"}]}"#,
            r#"{"a":{"b":1},"c":{"d":{"e":1,"e":1}}}"#,
            r#"{"a":1,"\u0061":2}"#,
        ] {
            let err = parse_bundle_manifest(json).expect_err(json);
            assert!(err.to_string().contains("duplicate object key"), "{json}: {err}");
        }
        // The same key in different objects is not a duplicate.
        assert!(
            parse_bundle_manifest(r#"{"a":{"k":1},"b":{"k":1},"k":[{"k":1},{"k":2}]}"#).is_ok()
        );
    }

    #[test]
    fn parse_bundle_manifest_rejects_reserved_keys_and_non_integer_numbers() {
        // With the arbitrary_precision and raw_value features enabled in this workspace,
        // serde_json reads these objects as a number or as embedded JSON, not as objects.
        // Numbers that are not 64-bit integers reach it through the same reserved key.
        for json in [
            r#"{"a":{"$serde_json::private::Number":"5"}}"#,
            r#"{"a":{"\u0024serde_json::private::Number":"5"}}"#,
            r#"{"a":{"$serde_json::private::RawValue":"{}"}}"#,
            r#"{"a":1.5}"#,
            r#"{"a":1e2}"#,
            r#"{"a":18446744073709551616}"#,
            r#"{"a":-9223372036854775809}"#,
        ] {
            let err = parse_bundle_manifest(json).expect_err(json);
            assert!(
                err.to_string().contains("unsupported number or reserved key"),
                "{json}: {err}"
            );
        }

        let manifest =
            parse_bundle_manifest(r#"{"a":18446744073709551615,"b":-9223372036854775808}"#)
                .expect("64-bit integers are accepted");
        assert_eq!(manifest, json!({"a": u64::MAX, "b": i64::MIN}));
    }

    #[test]
    fn unknown_payload_versions_are_rejected() {
        for payload_version in [json!(0), json!(4), json!("2"), json!(2.0), json!(null)] {
            let mut manifest = legacy_manifest();
            manifest["bundleIdentity"] = json!({"payloadVersion": payload_version, "value": "00"});
            assert!(verify_bundle_integral(&manifest).is_err(), "{payload_version}");
        }

        let mut manifest = legacy_manifest();
        manifest["bundleIdentity"] = json!({"value": "00"});
        assert!(verify_bundle_integral(&manifest).is_err());

        let mut manifest = legacy_manifest();
        manifest["bundleIdentity"] = json!("00");
        assert!(verify_bundle_integral(&manifest).is_err());

        // payloadVersion 1 selects the legacy canonicalization.
        let mut manifest = legacy_manifest();
        manifest["bundleIdentity"] = json!({"payloadVersion": 1, "value": "00"});
        assert!(verify_bundle_integral(&manifest).is_ok());
    }

    #[test]
    fn jcs_escapes_strings_like_rfc8785_example() {
        // RFC 8785 section 3.2.2 example without its "numbers" member (non-integers are
        // outside the supported subset), parsed from its escaped input text.
        let value: serde_json::Value = serde_json::from_str(
            r#"{"string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/", "literals": [null, true, false]}"#,
        )
        .expect("valid JSON");
        assert_eq!(
            jcs_string(&value).expect("jcs"),
            r#"{"literals":[null,true,false],"string":"€$\u000f\nA'B\"\\\\\"/"}"#
        );
    }

    #[test]
    fn canonical_json_sorts_object_keys_and_preserves_array_order() {
        let value = json!({
            "b": {"z": 1, "a": 2},
            "a": [3, {"d": 4, "c": 5}]
        });

        assert_eq!(canonical_json_string(&value), r#"{"a":[3,{"c":5,"d":4}],"b":{"a":2,"z":1}}"#);
    }

    #[test]
    fn verify_bundle_integral_removes_signature_and_bundle_integral_value() {
        let manifest = legacy_manifest();

        let report = verify_bundle_integral(&manifest).expect("verification should succeed");
        assert!(report.ok);
        assert!(report.matches);
        assert_eq!(
            report.computed_value,
            "796b22341072c0a219c0ff865623b712a5318746065f66cc3e0cf8cb2f01cfb9"
        );
    }
}

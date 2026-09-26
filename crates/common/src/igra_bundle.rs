use eyre::{Result, bail, eyre};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

/// Canonicalization of bundle-identity `payloadVersion` 1 and 2 (legacy bundles).
const LEGACY_CANONICALIZATION: &str = "json-c14n-sorted-keys-no-whitespace-utf8";
/// Canonicalization of bundle-identity `payloadVersion` 3.
const JCS_CANONICALIZATION: &str = "rfc8785-jcs-utf8";
/// `Number.MAX_SAFE_INTEGER`: the largest integer whose ES6 serialization, which RFC 8785
/// mandates, is guaranteed to be its plain decimal form.
const JCS_MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Clone, Debug, Serialize)]
pub struct VerifyBundleIntegralReport {
    pub ok: bool,
    pub algorithm: String,
    pub canonicalization: String,
    pub expected_value: String,
    pub computed_value: String,
    pub matches: bool,
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

/// `payloadVersion` 1 and 2 use the legacy canonicalization and `payloadVersion` 3 uses RFC 8785
/// JCS; a manifest never mixes the two.
fn check_canonicalization_matches_payload_version(
    root: &Map<String, Value>,
    canonicalization: &str,
) -> Result<()> {
    let payload_version = root
        .get("bundleIdentity")
        .and_then(|identity| identity.get("payloadVersion"))
        .and_then(Value::as_u64);

    if canonicalization == JCS_CANONICALIZATION && payload_version != Some(3) {
        bail!(
            "{JCS_CANONICALIZATION} requires bundleIdentity.payloadVersion 3, found {payload_version:?}"
        );
    }
    if canonicalization != JCS_CANONICALIZATION && payload_version == Some(3) {
        bail!(
            "bundleIdentity.payloadVersion 3 requires {JCS_CANONICALIZATION}, found {canonicalization}"
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
    } else if let Some(value) = number.as_i64() {
        if value.unsigned_abs() <= JCS_MAX_SAFE_INTEGER {
            return Ok(value.to_string());
        }
    }
    bail!(
        "{JCS_CANONICALIZATION}: number {number} is outside the supported exact-integer subset \
         (|n| <= 2^53 - 1)"
    )
}

#[cfg(test)]
mod tests {
    use super::{canonical_json_string, jcs_string, verify_bundle_integral};
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

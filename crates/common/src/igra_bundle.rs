use eyre::{Result, bail, eyre};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

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
    if canonicalization != "json-c14n-sorted-keys-no-whitespace-utf8" {
        bail!("unsupported bundleIntegral.canonicalization: {canonicalization}");
    }

    let expected_value = bundle_integral_obj
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("manifest bundleIntegral.value must be a string"))?
        .to_owned();

    let prepared = prepare_bundle_integral_payload(manifest)?;
    let canonical_json = canonical_json_string(&prepared);
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

#[cfg(test)]
mod tests {
    use super::{canonical_json_string, verify_bundle_integral};
    use serde_json::json;

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
        let manifest = json!({
            "schemaVersion": 1,
            "kind": "bundle",
            "bundleIntegral": {
                "algorithm": "sha256",
                "canonicalization": "json-c14n-sorted-keys-no-whitespace-utf8",
                "value": "796b22341072c0a219c0ff865623b712a5318746065f66cc3e0cf8cb2f01cfb9"
            },
            "files": [{"path":"a","sha256":"x"}],
            "signature": {"algorithm":"sha256-sign"}
        });

        let report = verify_bundle_integral(&manifest).expect("verification should succeed");
        assert!(report.ok);
        assert!(report.matches);
        assert_eq!(
            report.computed_value,
            "796b22341072c0a219c0ff865623b712a5318746065f66cc3e0cf8cb2f01cfb9"
        );
    }
}

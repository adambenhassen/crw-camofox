//! JSON-mode per-field diff. Walks two extractions and emits a map keyed by
//! field path (`plans[0].price`, Firecrawl style) to `{previous, current}`
//! pairs. Added fields have `previous: null`; removed fields `current: null`.

use serde_json::{Map, Value};

/// Compute the per-field diff between two extractions. Returns an empty object
/// when nothing tracked changed.
pub fn compute(previous: &Value, current: &Value) -> Value {
    let mut out = Map::new();
    walk("", previous, current, &mut out);
    Value::Object(out)
}

/// True when the two extractions differ on any leaf.
pub fn changed(previous: &Value, current: &Value) -> bool {
    let mut out = Map::new();
    walk("", previous, current, &mut out);
    !out.is_empty()
}

fn record(path: &str, previous: Value, current: Value, out: &mut Map<String, Value>) {
    let mut entry = Map::new();
    entry.insert("previous".into(), previous);
    entry.insert("current".into(), current);
    out.insert(path.to_string(), Value::Object(entry));
}

fn walk(path: &str, prev: &Value, cur: &Value, out: &mut Map<String, Value>) {
    match (prev, cur) {
        (Value::Object(pm), Value::Object(cm)) => {
            // union of keys
            let mut keys: Vec<&String> = pm.keys().chain(cm.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let child = if path.is_empty() {
                    k.to_string()
                } else {
                    format!("{path}.{k}")
                };
                match (pm.get(k), cm.get(k)) {
                    (Some(pv), Some(cv)) => walk(&child, pv, cv, out),
                    (Some(pv), None) => record(&child, pv.clone(), Value::Null, out),
                    (None, Some(cv)) => record(&child, Value::Null, cv.clone(), out),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(pa), Value::Array(ca)) => {
            let max = pa.len().max(ca.len());
            for i in 0..max {
                let child = format!("{path}[{i}]");
                match (pa.get(i), ca.get(i)) {
                    (Some(pv), Some(cv)) => walk(&child, pv, cv, out),
                    (Some(pv), None) => record(&child, pv.clone(), Value::Null, out),
                    (None, Some(cv)) => record(&child, Value::Null, cv.clone(), out),
                    (None, None) => {}
                }
            }
        }
        _ => {
            if prev != cur {
                record(path, prev.clone(), cur.clone(), out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_change_is_empty() {
        let a = json!({"plans": [{"price": "$19"}]});
        assert!(!changed(&a, &a));
        assert_eq!(compute(&a, &a), json!({}));
    }

    #[test]
    fn leaf_change_keyed_by_path() {
        let a = json!({"plans": [{"price": "$19"}, {"price": "$49"}]});
        let b = json!({"plans": [{"price": "$24"}, {"price": "$49"}]});
        let d = compute(&a, &b);
        assert_eq!(
            d["plans[0].price"],
            json!({"previous": "$19", "current": "$24"})
        );
        assert!(d.get("plans[1].price").is_none());
    }

    #[test]
    fn added_and_removed_fields() {
        let a = json!({"a": 1});
        let b = json!({"b": 2});
        let d = compute(&a, &b);
        assert_eq!(d["a"], json!({"previous": 1, "current": null}));
        assert_eq!(d["b"], json!({"previous": null, "current": 2}));
    }
}

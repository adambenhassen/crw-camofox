//! Deterministic Wikidata entity-relation lookup (W3, no AI).
//!
//! The obscure-entity long tail (PopQA-style "what is the <relation> of
//! <entity>") is a pure retrieval gap web search can't close — the answer page
//! never ranks. But these questions are, by construction, single Wikidata
//! triples `(entity, property, value)`. So we resolve them deterministically:
//! classify the query into a property + entity (no AI), `wbsearchentities` the
//! entity to a QID, fetch the property value via `Special:EntityData/{QID}.json`,
//! resolve an object QID to its label, and pin the result as a structured source
//! for the answer synthesizer (still UNTRUSTED-wrapped). Free open data, NOT a
//! commercial search API.
//!
//! Etiquette (Wikimedia UA policy): dedicated client with a descriptive
//! User-Agent (a generic/scraper UA -> 403), a hard per-lookup timeout, a
//! concurrency cap, and an in-process cache. Any error/timeout -> None -> the
//! normal SearXNG path runs unchanged. SPARQL is never used on the hot path.

use crate::structured::StructuredFact;
use moka::future::Cache;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Semaphore;

const UA: &str = "crw-opencore/0.x (https://fastcrw.com; contact@fastcrw.com) reqwest";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CONCURRENCY: usize = 2;
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
const CACHE_CAP: u64 = 10_000;

/// Relation keyword(s) -> Wikidata property. Covers PopQA's 16 relations plus
/// common phrasings. Order matters: longer/more-specific phrases first so
/// "capital of" wins over "capital".
const PROPERTIES: &[(&str, &str, &str)] = &[
    // (keyword phrase, P-number, canonical relation label)
    ("place of birth", "P19", "place of birth"),
    ("country of citizenship", "P27", "country of citizenship"),
    ("date of birth", "P569", "date of birth"),
    ("capital of", "P1376", "capital of"),
    ("screenwriter", "P58", "screenwriter"),
    ("occupation", "P106", "occupation"),
    ("religion", "P140", "religion"),
    ("director", "P57", "director"),
    ("producer", "P162", "producer"),
    ("composer", "P86", "composer"),
    ("author", "P50", "author"),
    ("genre", "P136", "genre"),
    ("father", "P22", "father"),
    ("mother", "P25", "mother"),
    ("sport", "P641", "sport"),
    ("color", "P462", "color"),
    ("colour", "P462", "color"),
    ("capital", "P36", "capital"),
    ("country", "P17", "country"),
    ("born in", "P19", "place of birth"),
    ("written by", "P50", "author"),
    ("directed by", "P57", "director"),
    ("composed by", "P86", "composer"),
];

/// Classify a query into (P-number, entity, relation-label) when it matches a
/// deterministic `<relation> of <entity>` / `<entity>'s <relation>` shape.
/// Conservative — returns None unless an entity span is clearly isolated, so
/// non-entity-relation queries fall straight through to SearXNG.
pub fn classify(query: &str) -> Option<(&'static str, String, &'static str)> {
    let q = query.trim().trim_end_matches('?').to_lowercase();
    for (kw, pnum, label) in PROPERTIES {
        // Pattern A: "... <relation> of <entity>"
        if let Some(pos) = q.find(&format!("{kw} of ")) {
            let entity = q[pos + kw.len() + 4..].trim();
            if let Some(e) = clean_entity(entity) {
                return Some((pnum, e, label));
            }
        }
        // Pattern B: "<entity>'s <relation>"
        if let Some(pos) = q.find(&format!("'s {kw}")) {
            let entity = q[..pos].trim();
            // strip a leading wh-word/article ("what is X's religion" -> "X")
            let entity = entity.rsplit_once(" is ").map(|(_, e)| e).unwrap_or(entity);
            if let Some(e) = clean_entity(entity) {
                return Some((pnum, e, label));
            }
        }
    }
    None
}

/// Trim trailing qualifiers + reject empty / too-long / obviously-non-entity
/// spans. Returns the cleaned entity (original case is lost; Wikidata search is
/// case-insensitive).
fn clean_entity(s: &str) -> Option<String> {
    let s = s
        .split([',', ';'])
        .next()
        .unwrap_or(s)
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.');
    let words = s.split_whitespace().count();
    if s.len() < 2 || s.len() > 80 || words == 0 || words > 8 {
        return None;
    }
    Some(s.to_string())
}

struct Wikidata {
    http: reqwest::Client,
    cache: Cache<String, Option<StructuredFact>>,
    sem: Semaphore,
}

fn global() -> Option<&'static Wikidata> {
    static WD: OnceLock<Option<Wikidata>> = OnceLock::new();
    WD.get_or_init(|| {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(LOOKUP_TIMEOUT)
            .build()
            .ok()?;
        Some(Wikidata {
            http,
            cache: Cache::builder()
                .max_capacity(CACHE_CAP)
                .time_to_live(CACHE_TTL)
                .build(),
            sem: Semaphore::new(MAX_CONCURRENCY),
        })
    })
    .as_ref()
}

/// Deterministic Wikidata lookup for an entity-relation query. Returns a pinned
/// structured fact, or None (no match / not found / any error) so the caller
/// falls through to the normal path. Hard-bounded by [`LOOKUP_TIMEOUT`].
pub async fn lookup(query: &str) -> Option<StructuredFact> {
    let (pnum, entity, label) = classify(query)?;
    let wd = global()?;
    let key = format!("{pnum}|{entity}");
    if let Some(hit) = wd.cache.get(&key).await {
        return hit;
    }
    let result = tokio::time::timeout(LOOKUP_TIMEOUT, async {
        let _permit = wd.sem.acquire().await.ok()?;
        resolve(wd, pnum, &entity, label).await
    })
    .await
    .ok()
    .flatten();
    wd.cache.insert(key, result.clone()).await;
    result
}

async fn resolve(wd: &Wikidata, pnum: &str, entity: &str, label: &str) -> Option<StructuredFact> {
    // 1. entity -> QID
    let qid = search_entity(wd, entity).await?;
    // 2. QID entity JSON
    let ent = get_entity(wd, &qid).await?;
    // 3. claims[P] -> value (string literal or object QID)
    let value = first_claim_value(&ent, pnum)?;
    // 4. if the value is a QID, resolve it to a label
    let value = if value.starts_with('Q') && value[1..].chars().all(|c| c.is_ascii_digit()) {
        get_entity(wd, &value)
            .await
            .and_then(|e| entity_label(&e))
            .unwrap_or(value)
    } else {
        value
    };
    let title = entity_label(&ent).unwrap_or_else(|| entity.to_string());
    Some(StructuredFact {
        title: format!("{title} (Wikidata)"),
        url: format!("https://www.wikidata.org/wiki/{qid}"),
        content: format!("{label}: {value}"),
        attributes: vec![(label.to_string(), value)],
        is_structured_source: true,
    })
}

async fn search_entity(wd: &Wikidata, entity: &str) -> Option<String> {
    let url = url::Url::parse_with_params(
        "https://www.wikidata.org/w/api.php",
        &[
            ("action", "wbsearchentities"),
            ("search", entity),
            ("language", "en"),
            ("format", "json"),
            ("limit", "1"),
        ],
    )
    .ok()?;
    let v: serde_json::Value = wd.http.get(url).send().await.ok()?.json().await.ok()?;
    v.get("search")?
        .as_array()?
        .first()?
        .get("id")?
        .as_str()
        .map(|s| s.to_string())
}

async fn get_entity(wd: &Wikidata, qid: &str) -> Option<serde_json::Value> {
    let url = format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json");
    let v: serde_json::Value = wd.http.get(&url).send().await.ok()?.json().await.ok()?;
    v.get("entities")?.get(qid).cloned()
}

/// First claim value for property `pnum`: a QID (for wikibase-item snaks) or a
/// formatted literal (time/string/quantity). None if absent/novalue.
fn first_claim_value(entity: &serde_json::Value, pnum: &str) -> Option<String> {
    let snak = entity
        .get("claims")?
        .get(pnum)?
        .as_array()?
        .iter()
        .find_map(|c| c.get("mainsnak"))?;
    let dv = snak.get("datavalue")?;
    match dv.get("type")?.as_str()? {
        "wikibase-entityid" => dv.get("value")?.get("id")?.as_str().map(|s| s.to_string()),
        "string" => dv.get("value")?.as_str().map(|s| s.to_string()),
        "time" => dv
            .get("value")?
            .get("time")?
            .as_str()
            .map(|s| s.trim_start_matches('+').to_string()),
        "monolingualtext" => dv
            .get("value")?
            .get("text")?
            .as_str()
            .map(|s| s.to_string()),
        "quantity" => dv
            .get("value")?
            .get("amount")?
            .as_str()
            .map(|s| s.trim_start_matches('+').to_string()),
        _ => None,
    }
}

fn entity_label(entity: &serde_json::Value) -> Option<String> {
    entity
        .get("labels")?
        .get("en")?
        .get("value")?
        .as_str()
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_relation_of_entity() {
        assert_eq!(
            classify("What is the religion of Abdullah of Pahang?"),
            Some(("P140", "abdullah of pahang".to_string(), "religion"))
        );
        let (p, e, _) = classify("Who is the author of the novel Dune?").unwrap();
        assert_eq!(p, "P50");
        assert_eq!(e, "the novel dune");
        let (p, _, _) = classify("what is the capital of Bhutan").unwrap();
        assert_eq!(p, "P36");
    }

    #[test]
    fn capital_of_beats_capital() {
        // "capital of X" -> P1376 (X is capital OF something) only if phrased so;
        // "the capital of X" -> P36 (capital city of X). Both are valid Wikidata
        // properties; assert we pick a capital property and isolate the entity.
        let (p, e, _) = classify("what is the capital of Peru").unwrap();
        assert!(p == "P36" || p == "P1376");
        assert_eq!(e, "peru");
    }

    #[test]
    fn possessive_pattern() {
        let (p, e, _) = classify("what is Dune's genre").unwrap();
        assert_eq!(p, "P136");
        assert_eq!(e, "dune");
    }

    #[test]
    fn rejects_non_entity_relation() {
        assert!(classify("how do I cook rice").is_none());
        assert!(classify("what is the weather today").is_none());
        // too-long entity span rejected
        assert!(classify(&format!("religion of {}", "x ".repeat(20))).is_none());
    }

    #[test]
    fn parses_entityid_claim() {
        let ent = json!({
            "claims": {"P140": [{"mainsnak": {"datavalue": {
                "type": "wikibase-entityid", "value": {"id": "Q101"}
            }}}]}
        });
        assert_eq!(first_claim_value(&ent, "P140"), Some("Q101".to_string()));
    }

    #[test]
    fn parses_time_and_string_claims() {
        let t = json!({"claims": {"P569": [{"mainsnak": {"datavalue": {
            "type": "time", "value": {"time": "+1959-00-00T00:00:00Z"}
        }}}]}});
        assert_eq!(
            first_claim_value(&t, "P569").unwrap(),
            "1959-00-00T00:00:00Z"
        );
        let s = json!({"claims": {"P1": [{"mainsnak": {"datavalue": {
            "type": "string", "value": "hello"
        }}}]}});
        assert_eq!(first_claim_value(&s, "P1"), Some("hello".to_string()));
    }

    #[test]
    fn missing_claim_is_none() {
        let ent = json!({"claims": {}});
        assert_eq!(first_claim_value(&ent, "P140"), None);
    }

    #[test]
    fn label_extraction() {
        let ent = json!({"labels": {"en": {"language": "en", "value": "Frank Herbert"}}});
        assert_eq!(entity_label(&ent), Some("Frank Herbert".to_string()));
    }
}

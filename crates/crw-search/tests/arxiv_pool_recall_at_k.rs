//! Does adding the arXiv pool help or hurt what the caller actually SEES?
//!
//! Union recall (does any pool contain the paper) is the easy number and it is
//! not the one that matters. `merge_rank` sorts by CROSS-POOL FREQUENCY first
//! and then truncates to `k`, so a fourth pool can raise a non-relevant paper's
//! frequency past a ground-truth paper's and push the ground-truth paper out of
//! the top k — union recall up, recall@k down.
//!
//! arXiv returns 37-44 candidates per query where OpenAlex returns single
//! digits, so it has the volume to do exactly that. This measures it instead of
//! arguing about it: ONE set of live fetches, then `merge_rank` run twice, on
//! the three original pools and on all four, comparing recall@k against the
//! real ArXivQA ground truth.
//!
//! Live and slow (arXiv is paced to one request every 3.5s). Run with:
//!   ARXIVQA=/path/to/arxivqa_full.json OPENALEX_KEY=.. S2_KEY=.. \
//!     cargo test -p crw-search --test arxiv_pool_recall_at_k -- --ignored --nocapture
use crw_search::research::{self, ResearchKeys, SearchFilters};

fn norm(id: &str) -> String {
    let s = id.trim().to_lowercase();
    let s = s.strip_prefix("arxiv:").unwrap_or(&s).to_string();
    match s.rfind('v') {
        Some(i) if s[i + 1..].chars().all(|c| c.is_ascii_digit()) && i + 1 < s.len() => {
            s[..i].to_string()
        }
        _ => s,
    }
}

#[tokio::test]
#[ignore = "live network, ~15 minutes"]
async fn arxiv_pool_does_not_reduce_recall_at_k() {
    let path = std::env::var("ARXIVQA").expect("set ARXIVQA to arxivqa_full.json");
    let raw = std::fs::read_to_string(&path).expect("read dataset");
    let data: serde_json::Value = serde_json::from_str(&raw).expect("parse dataset");
    let items = data.as_array().expect("dataset is an array");

    let oa_key = std::env::var("OPENALEX_KEY").ok();
    let s2_key = std::env::var("S2_KEY").ok();
    let keys = ResearchKeys {
        openalex_key: oa_key.as_deref(),
        openalex_mailto: Some("contact@fastcrw.com"),
        s2_key: s2_key.as_deref(),
    };
    let k = 40usize;

    let (mut gt_total, mut hit3, mut hit4) = (0usize, 0usize, 0usize);
    let (mut worse, mut better) = (0usize, 0usize);

    for (n, item) in items.iter().enumerate() {
        let Some(query) = item.get("query").and_then(|q| q.as_str()) else {
            continue;
        };
        let gt: Vec<String> = item
            .get("papers")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(norm)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if gt.is_empty() {
            continue;
        }

        // ONE set of fetches. The 4th pool is arXiv (see search_papers_pools).
        let pools = research::search_papers_pools(&keys, query, k, &SearchFilters::default()).await;
        assert_eq!(
            pools.len(),
            4,
            "pool count changed; this test measures 3-vs-4"
        );

        let without: Vec<_> = pools[..3].to_vec();
        let with = pools.clone();
        let ranked3 = research::merge_rank(without, k);
        let ranked4 = research::merge_rank(with, k);

        let ids = |r: &[crw_core::research_types::ResearchPaperResult]| -> Vec<String> {
            r.iter().map(|x| norm(&x.primary_id)).collect()
        };
        let (i3, i4) = (ids(&ranked3), ids(&ranked4));
        let h3 = gt.iter().filter(|g| i3.contains(g)).count();
        let h4 = gt.iter().filter(|g| i4.contains(g)).count();

        gt_total += gt.len();
        hit3 += h3;
        hit4 += h4;
        if h4 < h3 {
            worse += 1;
            println!("REGRESSION q{n}: recall@k {h3} -> {h4}  ({query:.70})");
        } else if h4 > h3 {
            better += 1;
        }
    }

    let r3 = 100.0 * hit3 as f64 / gt_total as f64;
    let r4 = 100.0 * hit4 as f64 / gt_total as f64;
    println!("\nrecall@{k} on {gt_total} ground-truth papers");
    println!("  3 pools (no arXiv): {hit3} matched, {r3:.1}%");
    println!("  4 pools (arXiv):    {hit4} matched, {r4:.1}%");
    println!("  questions improved: {better}, regressed: {worse}");

    // The whole point of the change. If the ranker's frequency-first truncation
    // eats the gain, this fails and the pool does not ship.
    assert!(
        hit4 >= hit3,
        "adding the arXiv pool REDUCED recall@{k} ({hit3} -> {hit4})"
    );
}

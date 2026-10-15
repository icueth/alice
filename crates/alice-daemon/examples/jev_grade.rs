//! Blind relevance grading through Alice's own Jev path (`JevJudge::grade_relevance`).
//!
//! usage: jev_grade INPUT.json OUTPUT.json [--only N] [--full-text] [--max-usd X]
//!   INPUT:  {"queries":[{"id","q","candidates":[{"label","text"}]}]}
//!   OUTPUT: {query_id:{label:{"grade":0|1|2,"expected":f,"dist":[p0,p1,p2]}}}
//! Grade is the argmax of the distribution over relevance_choices(): "answers" -> 2,
//! "partially" -> 1, "unrelated" -> 0 (ties to the lower grade); expected = 2*p2 + p1.
//! Every candidate is shown with the same project, kind, title, importance and age, so nothing
//! says which system produced it. The key comes only from TYPESAFE_API_KEY. Text goes to Jev
//! (redacted with the built-in rules) and nowhere else; stdout shows counts and ids only.
//! `--full-text` asks the same questions with the whole candidate text instead of the 700
//! characters `grade_relevance` keeps. The spend is the judge's own accounting: USD at
//! `USD_PER_MTOK_INPUT` per million input tokens as reported by Jev.
use alice_core::judge::jev::{
    FixedPolicy, JevConfig, JevJudge, MemoryJevStore, Question, UNSCOPED,
};
use alice_core::judge::{
    Distribution, Judge, JudgeMemory, RELEVANCE_ANSWERS, RELEVANCE_PARTIAL, relevance_choices,
    verbal,
};
use alice_core::model::Kind;
use alice_core::redact::Redactor;
use alice_core::settings::SecretValue;
use alice_daemon::judge_http::ReqwestTransport;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn grade(d: &Distribution) -> Option<(u8, f64, [f64; 3])> {
    let p = [
        d.p("unrelated"),
        d.p(RELEVANCE_PARTIAL),
        d.p(RELEVANCE_ANSWERS),
    ];
    if p.iter().sum::<f64>() <= 0.0 {
        return None;
    }
    let mut g = 0;
    for i in 1..3 {
        if p[i] > p[g] {
            g = i;
        }
    }
    Some((g as u8, 2.0 * p[2] + p[1], p))
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        a.len() >= 2,
        "usage: jev_grade INPUT OUTPUT [--only N] [--full-text] [--max-usd X]"
    );
    let flag = |f: &str| a.iter().position(|x| x == f);
    let only = flag("--only")
        .map(|i| a[i + 1].parse::<usize>())
        .transpose()?;
    let full = flag("--full-text").is_some();
    let max_usd = flag("--max-usd")
        .map(|i| a[i + 1].parse::<f64>())
        .transpose()?
        .unwrap_or(20.0);
    let input: Value = serde_json::from_slice(&std::fs::read(&a[0])?)?;
    let key = std::env::var("TYPESAFE_API_KEY")?;
    let store = Arc::new(MemoryJevStore::default());
    let judge = JevJudge::new(
        JevConfig {
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            model: "jev-1.13.0".into(),
            api_key: SecretValue::new(key),
            rate_per_sec: 5.0,
            timeout: Duration::from_secs(60),
            daily_budget_usd: 1.0e6,
            max_retries: 3,
        },
        Arc::new(ReqwestTransport::new()?),
        store.clone(),
        Arc::new(FixedPolicy::allow_all(Redactor::builtin())),
    );
    let mem = |body: &str| JudgeMemory {
        id: "m".into(),
        project: UNSCOPED.into(),
        kind: Kind::Note,
        title: "Memory".into(),
        body: body.into(),
        importance: 3,
        created_at: chrono::Utc::now().to_rfc3339(),
        similarity: None,
    };
    let (mut out, mut failed, mut graded) = (serde_json::Map::new(), Vec::new(), 0usize);
    let mut counts = [0usize; 3];
    let queries = input["queries"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no queries"))?;
    for q in queries.iter().take(only.unwrap_or(usize::MAX)) {
        let (id, text) = (
            q["id"].as_str().unwrap_or(""),
            q["q"].as_str().unwrap_or(""),
        );
        let cands = q["candidates"].as_array().cloned().unwrap_or_default();
        let results: Vec<JudgeMemory> = cands
            .iter()
            .map(|c| mem(c["text"].as_str().unwrap_or("")))
            .collect();
        let dists = if full {
            // The same state and questions as `grade_relevance`, without the 700-character cut.
            let now = chrono::Utc::now();
            let mut state = format!("Query: {text}\n");
            let mut qs = Vec::new();
            for (i, r) in results.iter().enumerate() {
                let k = i + 1;
                let head = verbal::memory_block(&format!("Result {k}"), &mem("@@"), now);
                state.push('\n');
                state.push_str(&head.replacen("@@", &r.body, 1));
                qs.push(Question {
                    id: format!("r_{k}"),
                    instructions: format!("How well does result {k} answer the query?"),
                    options: relevance_choices(),
                });
            }
            judge.ask(&[UNSCOPED], &state, &qs).map(|m| {
                (0..results.len())
                    .map(|i| m.get(&format!("r_{}", i + 1)).cloned().unwrap_or_default())
                    .collect::<Vec<_>>()
            })
        } else {
            judge.grade_relevance(UNSCOPED, text, &results)
        };
        match dists {
            Ok(ds) => {
                let mut m = serde_json::Map::new();
                for (c, d) in cands.iter().zip(&ds) {
                    let label = c["label"].as_str().unwrap_or("").to_string();
                    match grade(d) {
                        Some((g, e, p)) => {
                            counts[g as usize] += 1;
                            graded += 1;
                            m.insert(label, json!({"grade": g, "expected": e, "dist": p}));
                        }
                        None => failed.push(format!("{id}/{label}")),
                    }
                }
                out.insert(id.to_string(), Value::Object(m));
            }
            Err(e) => {
                eprintln!("query {id} failed: {e}");
                failed.push(id.to_string());
            }
        }
        let spent: f64 = store.calls().iter().map(|c| c.usd).sum();
        if spent >= max_usd {
            eprintln!("spend guard reached: ${spent:.4}");
            break;
        }
    }
    std::fs::write(&a[1], serde_json::to_vec(&Value::Object(out))?)?;
    let calls = store.calls();
    let usd: f64 = calls.iter().map(|c| c.usd).sum();
    let tok: u64 = calls.iter().map(|c| c.input_tokens).sum();
    let s = judge.stats();
    println!(
        "graded {graded} grades(0/1/2) {counts:?} failures {} {:?}\nrequests sent {} cache_hits {} retries {} input_tokens {tok} spend_usd {usd:.6}",
        failed.len(),
        failed,
        calls.len(),
        s.cache_hits,
        s.retries
    );
    Ok(())
}

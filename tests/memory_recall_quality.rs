//! Recall quality: which notes a question brings into the prompt.
//!
//! Thirty questions, half of them in Indonesian, run against the real SQLite
//! store through the path that builds the `[Memory context]` block. Each
//! question lists the notes it should recall and the notes it should not. The
//! store also holds hundreds of raw conversation rows that repeat the
//! questions' words, because that is what a busy chat leaves behind.
//!
//! Scoring is by keyword only: no embedding provider, no network, no clock, so
//! a run is deterministic.
//!
//! The data lives in `tests/fixtures/memory_recall/`. `baseline.json` records,
//! per question, which wanted notes arrive and which unwanted ones leak at the
//! default threshold. The test fails when a wanted note stops arriving or an
//! unwanted one starts to. To accept a change on purpose, rerun with
//! `RECALL_QUALITY_UPDATE_BASELINE=1` and commit the new file.
//!
//! `recall_threshold_sweep` prints how many wanted notes survive and how many
//! unwanted ones leak at each `min_relevance_score`, for choosing the default.

use rantaiclaw::memory::{
    build_memory_context_in_view, Memory, MemoryCategory, MemoryContextLimits, MemoryView,
    SqliteMemory,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

/// The `min_relevance_score` the fixture is judged at.
const THRESHOLD: f64 = 0.6;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/memory_recall")
        .join(name)
}

fn read_json(name: &str) -> Value {
    let text = std::fs::read_to_string(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn ids(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .expect("an array of ids")
        .iter()
        .map(|v| v.as_str().expect("an id").to_string())
        .collect()
}

/// What one question brought into the prompt.
struct Outcome {
    id: String,
    lang: String,
    question: String,
    should: BTreeSet<String>,
    should_not: BTreeSet<String>,
    /// Note ids that reached the block. Raw conversation rows show as `noise`.
    injected: Vec<String>,
}

impl Outcome {
    fn kept(&self) -> BTreeSet<String> {
        self.should
            .iter()
            .filter(|id| self.injected.contains(id))
            .cloned()
            .collect()
    }

    fn leaked(&self) -> BTreeSet<String> {
        self.should_not
            .iter()
            .filter(|id| self.injected.contains(id))
            .cloned()
            .collect()
    }
}

async fn seeded_store() -> (tempfile::TempDir, SqliteMemory, HashMap<String, String>) {
    let tmp = tempfile::TempDir::new().unwrap();
    let mem = SqliteMemory::new(tmp.path()).unwrap();
    let mut id_of_key = HashMap::new();

    for note in read_json("notes.json").as_array().unwrap() {
        let key = note["key"].as_str().unwrap();
        let category = match note["category"].as_str().unwrap() {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            other => panic!("unknown category {other}"),
        };
        mem.store(key, note["content"].as_str().unwrap(), category, None)
            .await
            .unwrap();
        id_of_key.insert(key.to_string(), note["id"].as_str().unwrap().to_string());
    }

    let mut n = 0;
    for row in read_json("noise.json").as_array().unwrap() {
        for _ in 0..row["copies"].as_u64().unwrap() {
            mem.store(
                &format!("telegram_chat_{n}"),
                row["content"].as_str().unwrap(),
                MemoryCategory::Conversation,
                None,
            )
            .await
            .unwrap();
            n += 1;
        }
    }
    (tmp, mem, id_of_key)
}

async fn run(threshold: f64) -> Vec<Outcome> {
    let (_tmp, mem, id_of_key) = seeded_store().await;
    let mut outcomes = Vec::new();
    for q in read_json("questions.json").as_array().unwrap() {
        let question = q["question"].as_str().unwrap();
        let ctx = build_memory_context_in_view(
            &mem,
            question,
            threshold,
            &MemoryView::All,
            MemoryContextLimits::default(),
        )
        .await;
        outcomes.push(Outcome {
            id: q["id"].as_str().unwrap().to_string(),
            lang: q["lang"].as_str().unwrap().to_string(),
            question: question.to_string(),
            should: ids(&q["should_recall"]),
            should_not: ids(&q["should_not_recall"]),
            injected: ctx
                .keys
                .iter()
                .map(|k| id_of_key.get(k).cloned().unwrap_or_else(|| "noise".into()))
                .collect(),
        });
    }
    outcomes
}

fn baseline_of(outcomes: &[Outcome]) -> Value {
    let questions: BTreeMap<String, Value> = outcomes
        .iter()
        .map(|o| {
            (
                o.id.clone(),
                json!({ "kept": o.kept(), "leaked": o.leaked() }),
            )
        })
        .collect();
    json!({ "threshold": THRESHOLD, "questions": questions })
}

#[test]
fn the_fixture_has_thirty_questions_and_half_are_indonesian() {
    let questions = read_json("questions.json");
    let questions = questions.as_array().unwrap();
    assert_eq!(questions.len(), 30);
    let indonesian = questions.iter().filter(|q| q["lang"] == "id").count();
    assert_eq!(indonesian, 15);
}

#[tokio::test]
async fn recall_quality_does_not_regress_against_the_baseline() {
    let outcomes = run(THRESHOLD).await;

    eprintln!("threshold {THRESHOLD}");
    for o in &outcomes {
        eprintln!(
            "{} [{}] kept {}/{} leaked {}/{} injected {:?}  {}",
            o.id,
            o.lang,
            o.kept().len(),
            o.should.len(),
            o.leaked().len(),
            o.should_not.len(),
            o.injected,
            o.question
        );
    }

    if std::env::var_os("RECALL_QUALITY_UPDATE_BASELINE").is_some() {
        let text = serde_json::to_string_pretty(&baseline_of(&outcomes)).unwrap();
        std::fs::write(fixture("baseline.json"), text + "\n").unwrap();
        return;
    }

    let baseline = read_json("baseline.json");
    assert_eq!(
        baseline["threshold"].as_f64().unwrap(),
        THRESHOLD,
        "the baseline was recorded at another threshold"
    );
    let mut regressions = Vec::new();
    for o in &outcomes {
        let recorded = &baseline["questions"][&o.id];
        assert!(!recorded.is_null(), "{} is not in the baseline", o.id);
        let was_kept = ids(&recorded["kept"]);
        let was_leaked = ids(&recorded["leaked"]);
        for lost in was_kept.difference(&o.kept()) {
            regressions.push(format!("{}: {lost} no longer arrives", o.id));
        }
        for new in o.leaked().difference(&was_leaked) {
            regressions.push(format!("{}: {new} now leaks", o.id));
        }
    }
    assert!(regressions.is_empty(), "{}", regressions.join("\n"));
}

#[tokio::test]
async fn raw_conversation_rows_never_reach_the_block() {
    for o in run(0.0).await {
        assert!(
            !o.injected.iter().any(|id| id == "noise"),
            "{} injected a raw conversation row",
            o.id
        );
    }
}

#[tokio::test]
async fn recall_threshold_sweep() {
    let mut rows = Vec::new();
    let mut threshold = 0.30;
    while threshold < 0.851 {
        let outcomes = run(threshold).await;
        let wanted: usize = outcomes.iter().map(|o| o.should.len()).sum();
        let unwanted: usize = outcomes.iter().map(|o| o.should_not.len()).sum();
        let kept: usize = outcomes.iter().map(|o| o.kept().len()).sum();
        let leaked: usize = outcomes.iter().map(|o| o.leaked().len()).sum();
        let en = |lang: &str| -> (usize, usize) {
            outcomes
                .iter()
                .filter(|o| o.lang == lang)
                .fold((0, 0), |(k, l), o| {
                    (k + o.kept().len(), l + o.leaked().len())
                })
        };
        let (kept_en, leaked_en) = en("en");
        let (kept_id, leaked_id) = en("id");
        eprintln!(
            "threshold {threshold:.2}  kept {kept}/{wanted} (en {kept_en}, id {kept_id})  \
             leaked {leaked}/{unwanted} (en {leaked_en}, id {leaked_id})"
        );
        rows.push((kept, leaked));
        threshold += 0.05;
    }
    assert!(
        rows.windows(2)
            .all(|w| w[1].0 <= w[0].0 && w[1].1 <= w[0].1),
        "a higher threshold must not keep or leak more: {rows:?}"
    );
}

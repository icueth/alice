//! Evaluation metrics for judges and search.

use super::Distribution;

pub fn accuracy(gold: &[String], pred: &[String]) -> f64 {
    if gold.is_empty() {
        return 0.0;
    }
    gold.iter().zip(pred).filter(|(g, p)| g == p).count() as f64 / gold.len() as f64
}

/// Mean of per-label F1 (a label never predicted nor present is skipped).
pub fn macro_f1(gold: &[String], pred: &[String], labels: &[&str]) -> f64 {
    let mut f1s = Vec::new();
    for l in labels {
        let count = |g_is: bool, p_is: bool| {
            gold.iter()
                .zip(pred)
                .filter(|(g, p)| (g.as_str() == *l) == g_is && (p.as_str() == *l) == p_is)
                .count() as f64
        };
        let (tp, fp, fn_) = (count(true, true), count(false, true), count(true, false));
        if tp + fp + fn_ == 0.0 {
            continue;
        }
        f1s.push(if tp == 0.0 {
            0.0
        } else {
            2.0 * tp / (2.0 * tp + fp + fn_)
        });
    }
    if f1s.is_empty() {
        0.0
    } else {
        f1s.iter().sum::<f64>() / f1s.len() as f64
    }
}

/// Multi-class Brier score: mean over items of Σ_label (p − 1[label = gold])². 0 is perfect, 2
/// the worst.
pub fn brier(gold: &[String], probs: &[Distribution], labels: &[&str]) -> f64 {
    if gold.is_empty() {
        return 0.0;
    }
    gold.iter()
        .zip(probs)
        .map(|(g, d)| {
            labels
                .iter()
                .map(|l| {
                    let y = if g == l { 1.0 } else { 0.0 };
                    (d.p(l) - y).powi(2)
                })
                .sum::<f64>()
        })
        .sum::<f64>()
        / gold.len() as f64
}

/// Expected calibration error over `bins` equal-width bins of the top probability.
pub fn ece(gold: &[String], probs: &[Distribution], bins: usize) -> f64 {
    let n = gold.len() as f64;
    if n == 0.0 || bins == 0 {
        return 0.0;
    }
    let mut acc = vec![0.0; bins];
    let mut conf = vec![0.0; bins];
    let mut count = vec![0usize; bins];
    for (g, d) in gold.iter().zip(probs) {
        let Some((label, p)) = d.best() else { continue };
        let b = ((p * bins as f64) as usize).min(bins - 1);
        count[b] += 1;
        conf[b] += p;
        acc[b] += if &label == g { 1.0 } else { 0.0 };
    }
    (0..bins)
        .filter(|&b| count[b] > 0)
        .map(|b| (count[b] as f64 / n) * ((acc[b] - conf[b]) / count[b] as f64).abs())
        .sum()
}

pub fn dcg(grades: &[f64], k: usize) -> f64 {
    grades
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, g)| (2f64.powf(*g) - 1.0) / ((i + 2) as f64).log2())
        .sum()
}

/// nDCG@k of a ranking, with the ideal ranking taken from the pooled grades of every system.
pub fn ndcg_at(grades_in_rank_order: &[f64], pool: &[f64], k: usize) -> f64 {
    let mut ideal = pool.to_vec();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let idcg = dcg(&ideal, k);
    if idcg == 0.0 {
        0.0
    } else {
        dcg(grades_in_rank_order, k) / idcg
    }
}

pub fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn classification_metrics() {
        let gold = s(&["a", "a", "b", "c"]);
        let pred = s(&["a", "b", "b", "c"]);
        assert_eq!(accuracy(&gold, &pred), 0.75);
        // a: tp1 fp0 fn1 → 2/3; b: tp1 fp1 fn0 → 2/3; c: 1.
        assert!(
            (macro_f1(&gold, &pred, &["a", "b", "c", "d"]) - (2.0 / 3.0 + 2.0 / 3.0 + 1.0) / 3.0)
                .abs()
                < 1e-9
        );
        let certain_right: Vec<Distribution> =
            gold.iter().map(|g| Distribution::certain(g)).collect();
        assert_eq!(brier(&gold, &certain_right, &["a", "b", "c"]), 0.0);
        assert_eq!(ece(&gold, &certain_right, 10), 0.0);
        let wrong = vec![Distribution::certain("b"); 4];
        assert!((brier(&s(&["a"; 4]), &wrong, &["a", "b"]) - 2.0).abs() < 1e-9);
        assert!((ece(&s(&["a"; 4]), &wrong, 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_is_one_for_the_ideal_ranking_and_lower_otherwise() {
        let pool = [2.0, 1.0, 0.0, 0.0];
        assert!((ndcg_at(&[2.0, 1.0, 0.0], &pool, 10) - 1.0).abs() < 1e-9);
        assert!(ndcg_at(&[0.0, 1.0, 2.0], &pool, 10) < 0.8);
        assert_eq!(ndcg_at(&[0.0], &[0.0], 10), 0.0);
        assert_eq!(percentile(&mut [3.0, 1.0, 2.0], 0.5), 2.0);
    }
}

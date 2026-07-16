//! Subsequence fuzzy matcher and scorer.
//!
//! `score(query, candidate)` returns `Some(FuzzyMatch)` when every character
//! of the query appears, in order, somewhere in the candidate (a
//! subsequence match), and `None` otherwise. Matching is case-insensitive.
//! The empty query matches every candidate with score 0.
//!
//! Scoring is pure integer arithmetic and fully deterministic: an optimal
//! alignment is chosen by dynamic programming, ties broken toward the
//! earliest candidate positions, so the same inputs always yield the same
//! score and the same highlight positions.
//!
//! The weights (locked by golden tests; changing any of these is a
//! ranking-behavior change and must update the goldens):
//!
//! | constant            | value | meaning                                     |
//! |---------------------|-------|---------------------------------------------|
//! | MATCH_BASE          |   16  | every matched character                     |
//! | BONUS_CONSECUTIVE   |   24  | match immediately follows the previous one  |
//! | BONUS_BOUNDARY      |   32  | match starts a word (after space, -, _, ., /, or a lower-to-upper camel step) |
//! | BONUS_START_EXTRA   |    8  | on top of BONUS_BOUNDARY at candidate index 0 |
//! | BONUS_EXACT         |  100  | query equals the whole candidate (case-insensitive) |
//! | PENALTY_GAP         |    2  | per candidate character skipped between two matches |
//! | PENALTY_LEADING_CAP |    6  | leading skip is charged at PENALTY_GAP per character but capped here, so word-boundary matches deep in a long title are not crushed |
//! | PENALTY_TRAILING    |    1  | per candidate character after the last match (prefers shorter candidates) |
//!
//! Consequences the goldens pin down: consecutive word-boundary runs
//! ("chr" into "Google Chrome") beat broken runs ("chr" into
//! "Character Map"); acronym matches on word boundaries ("gc" into
//! "Google Chrome") beat scattered mid-word matches; an exact match beats
//! any prefix of a longer title.

/// The result of a successful fuzzy match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Integer match quality; higher is better. May be negative for very
    /// weak matches (long gaps in long candidates); a returned value always
    /// means the subsequence exists.
    pub score: i64,
    /// Char indices (not byte indices) of the matched characters in the
    /// candidate, in ascending order, for UI highlighting.
    pub positions: Vec<usize>,
}

const MATCH_BASE: i64 = 16;
const BONUS_CONSECUTIVE: i64 = 24;
const BONUS_BOUNDARY: i64 = 32;
const BONUS_START_EXTRA: i64 = 8;
const BONUS_EXACT: i64 = 100;
const PENALTY_GAP: i64 = 2;
const PENALTY_LEADING_CAP: i64 = 6;
const PENALTY_TRAILING: i64 = 1;

/// Case-fold a single character deterministically. `to_lowercase` can map
/// one char to several; taking the first mapping keeps index arithmetic
/// one-to-one with the original character positions.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Per-position word-boundary bonus, computed on the original (unfolded)
/// candidate so camel-case boundaries survive case folding.
fn boundary_bonus(cand: &[char]) -> Vec<i64> {
    let mut bonus = Vec::with_capacity(cand.len());
    for (j, &ch) in cand.iter().enumerate() {
        let b = if j == 0 {
            BONUS_BOUNDARY + BONUS_START_EXTRA
        } else {
            let prev = cand[j - 1];
            let after_separator = matches!(prev, ' ' | '-' | '_' | '.' | '/');
            let camel_step = prev.is_lowercase() && ch.is_uppercase();
            if after_separator || camel_step {
                BONUS_BOUNDARY
            } else {
                0
            }
        };
        bonus.push(b);
    }
    bonus
}

/// Score `query` against `candidate`. Returns `None` when the query is not
/// a case-insensitive subsequence of the candidate.
pub fn score(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    let q: Vec<char> = query.chars().map(fold).collect();
    if q.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let cand_orig: Vec<char> = candidate.chars().collect();
    let cand: Vec<char> = cand_orig.iter().copied().map(fold).collect();
    let n = q.len();
    let m = cand.len();
    if n > m {
        return None;
    }

    let bonus = boundary_bonus(&cand_orig);

    // dp[i][j]: best score with query char i matched at candidate char j.
    // par[i][j]: the candidate position of query char i-1 on that best path.
    // Candidate strings are launcher titles (short), so the O(n * m^2)
    // transition scan is cheap and keeps the traceback trivially exact.
    const UNREACHED: i64 = i64::MIN;
    let mut dp = vec![vec![UNREACHED; m]; n];
    let mut par = vec![vec![usize::MAX; m]; n];

    for j in 0..m {
        if q[0] != cand[j] {
            continue;
        }
        let leading = (PENALTY_GAP * j as i64).min(PENALTY_LEADING_CAP);
        dp[0][j] = MATCH_BASE + bonus[j] - leading;
    }

    for i in 1..n {
        for j in i..m {
            if q[i] != cand[j] {
                continue;
            }
            let mut best = UNREACHED;
            let mut best_prev = usize::MAX;
            for (j2, &prev) in dp[i - 1].iter().enumerate().take(j).skip(i - 1) {
                if prev == UNREACHED {
                    continue;
                }
                let step = if j2 + 1 == j {
                    BONUS_CONSECUTIVE
                } else {
                    -PENALTY_GAP * (j - j2 - 1) as i64
                };
                // Strict greater-than keeps the earliest previous position
                // on ties, so highlights are stable.
                if prev + step > best {
                    best = prev + step;
                    best_prev = j2;
                }
            }
            if best == UNREACHED {
                continue;
            }
            dp[i][j] = best + MATCH_BASE + bonus[j];
            par[i][j] = best_prev;
        }
    }

    let mut best_total = UNREACHED;
    let mut best_j = usize::MAX;
    for (j, &s) in dp[n - 1].iter().enumerate() {
        if s == UNREACHED {
            continue;
        }
        let total = s - PENALTY_TRAILING * (m - 1 - j) as i64;
        if total > best_total {
            best_total = total;
            best_j = j;
        }
    }
    if best_j == usize::MAX {
        return None;
    }

    if q == cand {
        best_total += BONUS_EXACT;
    }

    let mut positions = vec![0usize; n];
    let mut j = best_j;
    for i in (0..n).rev() {
        positions[i] = j;
        if i > 0 {
            j = par[i][j];
        }
    }

    Some(FuzzyMatch {
        score: best_total,
        positions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(query: &str, candidate: &str) -> i64 {
        score(query, candidate)
            .unwrap_or_else(|| panic!("expected {query:?} to match {candidate:?}"))
            .score
    }

    #[test]
    fn empty_query_matches_everything_with_zero() {
        let m = score("", "anything at all").unwrap();
        assert_eq!(m.score, 0);
        assert!(m.positions.is_empty());
    }

    #[test]
    fn non_subsequence_returns_none() {
        assert_eq!(score("xyz", "Safari"), None);
        assert_eq!(score("chrome", "chr"), None);
        assert_eq!(score("aa", "a"), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(s("SAFARI", "safari"), s("safari", "SAFARI"));
        assert!(score("ChR", "google chrome").is_some());
    }

    // Golden: the exact weights are load-bearing. If a weight changes,
    // these numbers change, and the diff must be a deliberate ranking
    // decision.
    #[test]
    fn golden_locked_scores() {
        assert_eq!(s("chr", "Google Chrome"), 119);
        assert_eq!(s("chr", "Character Map"), 101);
        assert_eq!(s("gc", "Google Chrome"), 87);
        assert_eq!(s("gc", "Magic Wand"), 21);
        assert_eq!(s("sf", "Safari"), 67);
        assert_eq!(s("safari", "Safari"), 356);
    }

    #[test]
    fn golden_chr_ranks_chrome_above_character_map() {
        assert!(s("chr", "Google Chrome") > s("chr", "Character Map"));
    }

    #[test]
    fn golden_acronym_beats_scattered_match() {
        assert!(s("gc", "Google Chrome") > s("gc", "Magic Wand"));
    }

    #[test]
    fn golden_sf_ranks_safari_above_scattered() {
        assert!(s("sf", "Safari") > s("sf", "Transfer"));
    }

    #[test]
    fn exact_match_beats_prefix_of_longer_candidate() {
        assert!(s("safari", "Safari") > s("safari", "Safari Technology Preview"));
    }

    #[test]
    fn prefix_match_beats_mid_word_match() {
        assert!(s("go", "Google") > s("go", "Argo"));
    }

    #[test]
    fn shorter_candidate_wins_on_equal_alignment() {
        assert!(s("note", "Notes") > s("note", "Notesmith"));
    }

    #[test]
    fn camel_case_boundary_gets_the_word_bonus() {
        // 'T' after lowercase 'i' is a camel boundary; 't' inside "notes"
        // is not.
        assert!(s("t", "iTerm") > s("t", "notes"));
    }

    #[test]
    fn positions_point_at_matched_chars() {
        let m = score("chr", "Google Chrome").unwrap();
        assert_eq!(m.positions, vec![7, 8, 9]);
        let m = score("gc", "Google Chrome").unwrap();
        assert_eq!(m.positions, vec![0, 7]);
    }

    #[test]
    fn positions_are_char_indices_not_bytes() {
        // 'É' is two bytes; the position must be the char index.
        let m = score("c", "Éclair").unwrap();
        assert_eq!(m.positions, vec![1]);
    }

    #[test]
    fn unicode_case_folding_matches() {
        let m = score("é", "Éclair").unwrap();
        assert_eq!(m.positions, vec![0]);
    }

    #[test]
    fn deterministic_repeat_calls() {
        let a = score("chr", "Google Chrome").unwrap();
        let b = score("chr", "Google Chrome").unwrap();
        assert_eq!(a, b);
    }
}

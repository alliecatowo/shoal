//! Bounded completion matching policy, independent of candidate discovery.

pub(super) fn candidate_matches(
    name: &str,
    prefix: &str,
    fuzzy: bool,
    case_insensitive: bool,
) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if case_insensitive {
        let name = name.to_lowercase();
        let prefix = prefix.to_lowercase();
        if fuzzy {
            fuzzy_match(&name, &prefix)
        } else {
            name.starts_with(&prefix)
        }
    } else if fuzzy {
        fuzzy_match(name, prefix)
    } else {
        name.starts_with(prefix)
    }
}

pub(super) fn subsequence_match(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|needle| chars.any(|item| item == needle))
}

fn fuzzy_match(candidate: &str, input: &str) -> bool {
    subsequence_match(candidate, input) || one_adjacent_transposition_away(candidate, input)
}

/// Recognize the most common filename typo without turning completion into an
/// unbounded edit-distance search. Both inputs are already case-normalized
/// when case-insensitive completion is configured.
fn one_adjacent_transposition_away(candidate: &str, input: &str) -> bool {
    let mut mismatches = Vec::with_capacity(2);
    let mut candidate_chars = candidate.chars();
    let mut input_chars = input.chars();
    let mut index = 0usize;
    loop {
        match (candidate_chars.next(), input_chars.next()) {
            (Some(left), Some(right)) if left != right => {
                mismatches.push((index, left, right));
                if mismatches.len() > 2 {
                    return false;
                }
            }
            (Some(_), Some(_)) => {}
            (None, None) => break,
            _ => return false,
        }
        index += 1;
    }
    matches!(
        mismatches.as_slice(),
        [(first, left_a, right_a), (second, left_b, right_b)]
            if *second == *first + 1 && left_a == right_b && left_b == right_a
    )
}

use regex_syntax::hir::{Hir, HirKind};
use regex_syntax::ParserBuilder;

const MAX_CLAUSES: usize = 16;
const MAX_ALTERNATIVES: usize = 32;

pub(crate) type Clause = Vec<Vec<u8>>;

/// A bounded CNF proof: every match contains at least one literal from every
/// clause. Failure to prove a useful bound is represented by `None`.
pub(crate) fn required_literals(pattern: &str) -> Option<Vec<Clause>> {
    let hir = ParserBuilder::new()
        .case_insensitive(false)
        .build()
        .parse(pattern)
        .ok()?;
    analyze(&hir)
}

fn analyze(hir: &Hir) -> Option<Vec<Clause>> {
    match hir.kind() {
        HirKind::Literal(literal) => literal_clause(&literal.0),
        HirKind::Capture(capture) => analyze(&capture.sub),
        HirKind::Repetition(repetition) if repetition.min > 0 => analyze(&repetition.sub),
        HirKind::Concat(parts) => {
            let mut clauses = Vec::new();
            for part in parts {
                if let Some(mut found) = analyze(part) {
                    clauses.append(&mut found);
                }
            }
            normalize_clauses(clauses)
        }
        HirKind::Alternation(branches) => analyze_alternation(branches),
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) | HirKind::Repetition(_) => None,
    }
}

fn literal_clause(bytes: &[u8]) -> Option<Vec<Clause>> {
    if bytes.len() < 3 || !bytes.is_ascii() {
        return None;
    }
    Some(vec![vec![bytes
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect()]])
}

fn analyze_alternation(branches: &[Hir]) -> Option<Vec<Clause>> {
    let branch_constraints: Vec<Vec<Clause>> =
        branches.iter().map(analyze).collect::<Option<Vec<_>>>()?;

    let mut product = vec![Vec::<Vec<u8>>::new()];
    for constraints in &branch_constraints {
        let mut next = Vec::new();
        for accumulated in &product {
            for clause in constraints {
                let mut combined = accumulated.clone();
                combined.extend(clause.iter().cloned());
                normalize_clause(&mut combined);
                if combined.len() > MAX_ALTERNATIVES {
                    return alternation_fallback(&branch_constraints);
                }
                next.push(combined);
                if next.len() > MAX_CLAUSES {
                    return alternation_fallback(&branch_constraints);
                }
            }
        }
        product = next;
    }
    normalize_clauses(product)
}

fn alternation_fallback(branches: &[Vec<Clause>]) -> Option<Vec<Clause>> {
    let mut clause = Vec::new();
    for constraints in branches {
        let best = constraints
            .iter()
            .max_by_key(|candidate| clause_score(candidate))?;
        clause.extend(best.iter().cloned());
    }
    normalize_clause(&mut clause);
    (clause.len() <= MAX_ALTERNATIVES).then_some(vec![clause])
}

fn normalize_clauses(mut clauses: Vec<Clause>) -> Option<Vec<Clause>> {
    if clauses.is_empty() {
        return None;
    }
    for clause in &mut clauses {
        normalize_clause(clause);
    }
    clauses.sort_unstable();
    clauses.dedup();
    if clauses.len() > MAX_CLAUSES {
        clauses.sort_by_key(|clause| std::cmp::Reverse(clause_score(clause)));
        clauses.truncate(MAX_CLAUSES);
    }
    Some(clauses)
}

fn normalize_clause(clause: &mut Clause) {
    clause.sort_unstable();
    clause.dedup();
}

fn clause_score(clause: &Clause) -> usize {
    clause.iter().map(Vec::len).min().unwrap_or(0) * 64 / clause.len().max(1)
}

#[cfg(test)]
mod tests {
    use super::required_literals;

    fn clauses(pattern: &str) -> Option<Vec<Vec<Vec<u8>>>> {
        required_literals(pattern)
    }

    #[test]
    fn finds_prefix_suffix_and_infix_literals() {
        assert_eq!(clauses(r"^report.*$"), Some(vec![vec![b"report".to_vec()]]));
        assert_eq!(clauses(r"^.*\.pdf$"), Some(vec![vec![b".pdf".to_vec()]]));
        assert_eq!(
            clauses(r"^.*report.*$"),
            Some(vec![vec![b"report".to_vec()]])
        );
    }

    #[test]
    fn concatenation_produces_independent_required_clauses() {
        assert_eq!(
            clauses(r"foo.*bar"),
            Some(vec![vec![b"bar".to_vec()], vec![b"foo".to_vec()]])
        );
    }

    #[test]
    fn alternation_produces_a_required_or_clause() {
        assert_eq!(
            clauses(r".*\.(png|jpeg)$"),
            Some(vec![vec![b"jpeg".to_vec(), b"png".to_vec()]])
        );
    }

    #[test]
    fn unsafe_or_useless_patterns_fall_back() {
        for pattern in [".*", ".", "[a-z]+", "(report|ab)", "é", "("] {
            assert_eq!(clauses(pattern), None, "{pattern}");
        }
    }

    #[test]
    fn literals_are_ascii_lowercased() {
        assert_eq!(clauses("FooBar"), Some(vec![vec![b"foobar".to_vec()]]));
    }
}

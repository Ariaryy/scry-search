pub const MAX_TERMS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyTerms;

/// Split whitespace-separated terms while preserving quoted phrases.
/// Unterminated quotes are accepted so interactive typing remains searchable.
pub fn parse_terms(input: &str) -> Result<Vec<String>, TooManyTerms> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in input.chars() {
        match character {
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    terms.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
        if terms.len() > MAX_TERMS {
            return Err(TooManyTerms);
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    (terms.len() <= MAX_TERMS)
        .then_some(terms)
        .ok_or(TooManyTerms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_terms_handles_partial_input() {
        assert_eq!(parse_terms("  alpha   beta ").unwrap(), ["alpha", "beta"]);
        assert_eq!(
            parse_terms("\"annual report\" draft").unwrap(),
            ["annual report", "draft"]
        );
        assert_eq!(parse_terms("\"still typing").unwrap(), ["still typing"]);
        assert!(parse_terms("   \t ").unwrap().is_empty());
    }

    #[test]
    fn parse_terms_enforces_mask_width() {
        let sixteen = (0..16)
            .map(|i| format!("t{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(parse_terms(&sixteen).unwrap().len(), MAX_TERMS);
        assert!(parse_terms(&(sixteen + " overflow")).is_err());
    }
}

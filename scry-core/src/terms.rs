pub const MAX_TERMS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyTerms;

use crate::query::{EntryKind, Query, QueryFilter};

/// Parse interactive path terms and optional metadata predicates.
///
/// Supported predicates are `type:file`, `type:dir`, `ext:rs,txt`,
/// `size:>10mb`, and `modified:<7d` (age younger than seven days).
pub fn parse_query(input: &str, now_secs: u32) -> Result<Query, TooManyTerms> {
    let tokens = tokenize(input)?;
    let mut terms = Vec::new();
    let mut filter = QueryFilter::default();
    for token in tokens {
        let lower = token.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("type:") {
            filter.kind = match value {
                "file" => Some(EntryKind::File),
                "dir" | "directory" | "folder" => Some(EntryKind::Directory),
                _ => {
                    terms.push(token);
                    continue;
                }
            };
        } else if let Some(value) = lower.strip_prefix("ext:") {
            let extensions: Vec<_> = value
                .split(',')
                .map(|value| value.trim_start_matches('.'))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect();
            if extensions.is_empty() {
                terms.push(token);
            } else {
                filter.extensions.extend(extensions);
            }
        } else if let Some(value) = lower.strip_prefix("size:") {
            if !apply_u64_comparison(
                value,
                parse_size,
                &mut filter.min_size,
                &mut filter.max_size,
            ) {
                terms.push(token);
            }
        } else if let Some(value) = lower.strip_prefix("modified:") {
            if !apply_age_comparison(
                value,
                now_secs,
                &mut filter.min_mtime,
                &mut filter.max_mtime,
            ) {
                terms.push(token);
            }
        } else {
            terms.push(token);
        }
    }
    let mut terms = split_path_components(terms)?;
    if terms.is_empty() && !filter.is_empty() {
        terms.push(String::new());
    }
    Ok(if filter.is_empty() {
        Query::PathTerms(terms)
    } else {
        Query::FilteredPathTerms { terms, filter }
    })
}

/// Split whitespace-separated terms while preserving quoted phrases and path
/// components. Unterminated quotes are accepted so interactive typing remains
/// searchable.
pub fn parse_terms(input: &str) -> Result<Vec<String>, TooManyTerms> {
    split_path_components(tokenize(input)?)
}

fn tokenize(input: &str) -> Result<Vec<String>, TooManyTerms> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in input.chars() {
        match character {
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted && !current.contains(['\\', '/']) => {
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
    Ok(terms)
}

fn split_path_components(terms: Vec<String>) -> Result<Vec<String>, TooManyTerms> {
    let terms: Vec<_> = terms
        .into_iter()
        .flat_map(|term| {
            term.split(['\\', '/'])
                .filter(|component| !component.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    (terms.len() <= MAX_TERMS)
        .then_some(terms)
        .ok_or(TooManyTerms)
}

fn parse_size(value: &str) -> Option<u64> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u64 = value[..split].parse().ok()?;
    let multiplier = match &value[split..] {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        _ => return None,
    };
    number.checked_mul(multiplier)
}

fn parse_age(value: &str) -> Option<u32> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u32 = value[..split].parse().ok()?;
    let multiplier: u32 = match &value[split..] {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => return None,
    };
    number.checked_mul(multiplier)
}

fn apply_u64_comparison(
    input: &str,
    parse: impl FnOnce(&str) -> Option<u64>,
    min: &mut Option<u64>,
    max: &mut Option<u64>,
) -> bool {
    let (operator, value) = if let Some(value) = input.strip_prefix(">=") {
        (">=", value)
    } else if let Some(value) = input.strip_prefix('>') {
        (">", value)
    } else if let Some(value) = input.strip_prefix("<=") {
        ("<=", value)
    } else if let Some(value) = input.strip_prefix('<') {
        ("<", value)
    } else {
        ("=", input)
    };
    let Some(value) = parse(value) else {
        return false;
    };
    match operator {
        ">=" => *min = Some(value),
        ">" => *min = value.checked_add(1),
        "<=" => *max = Some(value),
        "<" => *max = value.checked_sub(1),
        _ => {
            *min = Some(value);
            *max = Some(value);
        }
    }
    true
}

fn apply_age_comparison(
    input: &str,
    now: u32,
    min: &mut Option<u32>,
    max: &mut Option<u32>,
) -> bool {
    let (younger, value) = if let Some(value) = input.strip_prefix('<') {
        (true, value)
    } else if let Some(value) = input.strip_prefix('>') {
        (false, value)
    } else {
        (true, input)
    };
    let Some(age) = parse_age(value) else {
        return false;
    };
    let boundary = now.saturating_sub(age);
    if younger {
        *min = Some(boundary)
    } else {
        *max = Some(boundary)
    }
    true
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
    fn parse_terms_expands_common_absolute_path_forms() {
        for input in [
            r"C:\Program Files",
            r"C:\\Program Files",
            "C:/Program Files",
            r#""C:\Program Files""#,
        ] {
            assert_eq!(
                parse_terms(input).unwrap(),
                ["C:", "Program Files"],
                "{input:?}"
            );
        }
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

    #[test]
    fn parse_query_extracts_metadata_filters() {
        let query = parse_query(
            "report type:file ext:pdf,docx size:>10mb modified:<7d",
            1_000_000,
        )
        .unwrap();
        let Query::FilteredPathTerms { terms, filter } = query else {
            panic!()
        };
        assert_eq!(terms, ["report"]);
        assert_eq!(filter.kind, Some(EntryKind::File));
        assert_eq!(filter.extensions, ["pdf", "docx"]);
        assert_eq!(filter.min_size, Some(10 * 1024 * 1024 + 1));
        assert_eq!(filter.min_mtime, Some(395_200));
    }
}

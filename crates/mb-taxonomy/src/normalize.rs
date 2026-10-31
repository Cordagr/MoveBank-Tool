pub fn split_taxon_field(raw: &str) -> Vec<&str> {
    raw.split([',', ';'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn normalize_name(raw: &str) -> String {
    let mut name = Vec::new();
    for token in raw.split_whitespace() {
        let token =
            token.trim_matches(|character: char| matches!(character, '(' | ')' | ',' | ';'));
        if token.is_empty() {
            continue;
        }

        let lower = token.to_ascii_lowercase();
        if matches!(lower.as_str(), "sp" | "sp." | "spp" | "spp.") {
            break;
        }
        let is_authorship = token.chars().next().is_some_and(char::is_uppercase)
            && token.chars().skip(1).any(char::is_lowercase);
        if !name.is_empty() && is_authorship {
            break;
        }
        name.push(lower);
    }
    name.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_case_spacing_and_authorship() {
        assert_eq!(
            normalize_name("  Panthera   leo (Linnaeus, 1758) "),
            "panthera leo"
        );
    }

    #[test]
    fn removes_unidentified_species_suffixes() {
        assert_eq!(normalize_name("Canis sp."), "canis");
        assert_eq!(normalize_name("Canis lupus spp."), "canis lupus");
    }
}

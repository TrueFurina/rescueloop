pub(crate) fn select_lines(
    text: &str,
    keywords: &[&str],
    excluded: &[&str],
    limit: usize,
) -> Vec<String> {
    text.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            keywords.iter().any(|key| lower.contains(key))
                && excluded.iter().all(|key| !lower.contains(key))
        })
        .take(limit)
        .map(|line| line.trim().chars().take(500).collect())
        .collect()
}

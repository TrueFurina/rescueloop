use crossterm::event::KeyCode;

pub(super) fn normalize_key_code(code: KeyCode) -> KeyCode {
    let KeyCode::Char(character) = code else {
        return code;
    };
    let character = character.to_lowercase().next().unwrap_or(character);
    let latin = match character {
        'й' => 'q',
        'ф' => 'a',
        'к' => 'r',
        'р' => 'h',
        'в' => 'd',
        'п' => 'g',
        'г' => 'u',
        'н' => 'y',
        'т' => 'n',
        'о' => 'j',
        'л' => 'k',
        value if value.is_ascii_alphabetic() => value,
        _ => return KeyCode::Char(character),
    };
    KeyCode::Char(latin)
}

#[cfg(test)]
mod tests {
    use super::normalize_key_code;
    use crossterm::event::KeyCode;

    #[test]
    fn maps_cyrillic_layout_hotkeys() {
        for (input, expected) in [
            ('й', 'q'),
            ('ф', 'a'),
            ('к', 'r'),
            ('р', 'h'),
            ('в', 'd'),
            ('п', 'g'),
            ('г', 'u'),
            ('н', 'y'),
            ('т', 'n'),
            ('о', 'j'),
            ('л', 'k'),
            ('Й', 'q'),
        ] {
            assert_eq!(
                normalize_key_code(KeyCode::Char(input)),
                KeyCode::Char(expected)
            );
        }
    }

    #[test]
    fn preserves_navigation_and_normalizes_latin_case() {
        assert_eq!(normalize_key_code(KeyCode::Up), KeyCode::Up);
        assert_eq!(normalize_key_code(KeyCode::Char('Q')), KeyCode::Char('q'));
    }
}

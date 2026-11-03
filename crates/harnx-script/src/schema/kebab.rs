pub(crate) fn kebab_case(key: &str) -> String {
    let chars: Vec<_> = key.chars().collect();
    let mut result = String::new();
    for (index, &current) in chars.iter().enumerate() {
        if !current.is_alphanumeric() {
            push_separator(&mut result);
            continue;
        }
        if is_case_boundary(&chars, index) {
            push_separator(&mut result);
        }
        result.extend(current.to_lowercase());
    }
    result.trim_end_matches('-').to_owned()
}

fn push_separator(result: &mut String) {
    if result.is_empty() {
        return;
    }
    if !result.ends_with('-') {
        result.push('-');
    }
}

fn is_case_boundary(chars: &[char], index: usize) -> bool {
    if !chars[index].is_uppercase() {
        return false;
    }
    let Some(previous) = index.checked_sub(1).map(|i| chars[i]) else {
        return false;
    };
    if previous.is_lowercase() {
        return true;
    }
    if previous.is_numeric() {
        return true;
    }
    if !previous.is_uppercase() {
        return false;
    }
    chars.get(index + 1).is_some_and(|next| next.is_lowercase())
}

use super::*;

#[test]
fn sparse_oversized_and_non_utf8_policy_files_fail_typed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("leash.toml");
    let file = fs::File::create(&path).unwrap();
    file.set_len((POLICY_MAX_BYTES + 1) as u64).unwrap();
    assert!(matches!(
        Policy::load(&path),
        Err(PolicyLoadError::TooLarge { path: ref found, .. }) if found == &path
    ));

    fs::write(&path, [0xff]).unwrap();
    assert!(matches!(
        Policy::load(&path),
        Err(PolicyLoadError::Utf8 { path: ref found }) if found == &path
    ));
    assert!(matches!(
        Policy::load(directory.path()),
        Err(PolicyLoadError::NotFile { .. })
    ));
}

#[test]
fn deep_wide_duplicate_and_unknown_policy_shapes_fail_closed() {
    let deep = format!(
        "[principal.agent]\nnet_connect = {}\"x:1\"{}\n",
        "[".repeat(POLICY_MAX_NESTING + 1),
        "]".repeat(POLICY_MAX_NESTING + 1)
    );
    assert!(Policy::from_toml(&deep).is_err());

    let wide = (0..=POLICY_MAX_PRINCIPALS)
        .map(|index| format!("[principal.p{index}]\ntime=true\n"))
        .collect::<String>();
    assert!(Policy::from_toml(&wide).is_err());

    let grants = std::iter::repeat_n("\"x\"", POLICY_MAX_GRANTS_PER_KIND + 1)
        .collect::<Vec<_>>()
        .join(",");
    assert!(Policy::from_toml(&format!("[principal.agent]\nenv_read=[{grants}]\n")).is_err());

    assert!(Policy::from_toml("[principal.agent]\ntime=true\ntime=false\n").is_err());
    assert!(Policy::from_toml("[principal.agent]\ntiem=true\n").is_err());
    assert!(Policy::from_toml("[mystery]\nallow=true\n").is_err());
}

#[test]
fn oversized_grant_string_is_rejected() {
    let source = format!(
        "[principal.agent]\nenv_read=[\"{}\"]\n",
        "x".repeat(POLICY_MAX_GRANT_BYTES + 1)
    );
    assert!(Policy::from_toml(&source).is_err());
}

#[test]
fn production_policy_loader_has_no_whole_file_read() {
    let production = include_str!("parsing.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(!production.contains("fs::read_to_string"));
    assert!(production.contains("POLICY_MAX_BYTES + 1"));
}

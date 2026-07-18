//! Prompt configuration discovery, layering, and migration.

use super::*;
use std::fs::{self, File};
use std::io::{self, Read};

const PROMPT_ENV_NAMES: &[&str] = &[
    "SHOAL_PROMPT_LEFT",
    "SHOAL_PROMPT_RIGHT",
    "SHOAL_PROMPT_THEME",
    "SHOAL_PROMPT",
    "SHOAL_NERD_FONT",
];

// ---------------------------------------------------------------------------
// Prompt config loading (site/content/internals/prompt-editor-lsp.md precedence, site/content/internals/prompt-editor-lsp.md migration)
// ---------------------------------------------------------------------------

/// Load and layer the prompt config from the same discovery paths
/// `shoal_config` uses, plus the dedicated `prompt.toml` (site/content/internals/prompt-editor-lsp.md). Returns the
/// finished [`PromptConfig`] and any load-time warnings (site/content/internals/prompt-editor-lsp.md).
pub fn load_prompt_config(cwd: &Path) -> (PromptConfig, Vec<String>) {
    let mut warnings = Vec::new();
    let mut layers: Vec<toml::Value> = Vec::new();

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".config")))
        .map(|p| p.join("shoal"));

    // system + user shoal.toml [prompt] tables
    if let Some(v) = read_prompt_table(Path::new("/etc/shoal/shoal.toml"), &mut warnings) {
        layers.push(v);
    }
    if let Some(dir) = &config_dir {
        if let Some(v) = read_prompt_table(&dir.join("shoal.toml"), &mut warnings) {
            layers.push(v);
        }
        // dedicated prompt.toml (root-level = [prompt] contents directly)
        if let Some(v) = read_root_table(&dir.join("prompt.toml"), &mut warnings) {
            layers.push(v);
        }
    }
    // project .shoal.toml [prompt] table
    if let Some(mut v) = read_prompt_table(&cwd.join(".shoal.toml"), &mut warnings) {
        remove_project_custom_commands(&mut v, &mut warnings);
        layers.push(v);
    }

    // Environment overrides (highest precedence).
    let env = prompt_environment(&mut warnings);
    layers.push(shoal_prompt::env_overrides_checked(&env, &mut warnings));

    let config = shoal_prompt::load(layers, &mut warnings);
    (config, warnings)
}

/// Project config is discovered merely by entering a directory, so it must
/// never acquire the user-config trust required to launch prompt processes.
/// Remove the whole dynamic custom table: otherwise a project could override
/// the `command` of an otherwise trusted user-defined identity through merge.
fn remove_project_custom_commands(prompt: &mut toml::Value, warnings: &mut Vec<String>) {
    let removed = prompt
        .get_mut("module")
        .and_then(toml::Value::as_table_mut)
        .and_then(|module| module.remove("custom"));
    if removed.is_some() {
        warnings.push(
            "prompt: project .shoal.toml module.custom ignored; executable prompt commands are trusted system/user configuration only"
                .into(),
        );
    }
}

/// Read a config file's `[prompt]` sub-table as a prompt-contents-shaped value,
/// applying the site/content/internals/prompt-editor-lsp.md `template` → `format.left` migration.
fn read_prompt_table(path: &Path, warnings: &mut Vec<String>) -> Option<toml::Value> {
    let text = read_prompt_text(path, warnings)?;
    let value = match shoal_prompt::parse_layer(&text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("{}: {e}", path.display()));
            return None;
        }
    };
    let prompt = value.get("prompt")?.clone();
    Some(migrate_template(prompt, warnings))
}

fn read_root_table(path: &Path, warnings: &mut Vec<String>) -> Option<toml::Value> {
    let text = read_prompt_text(path, warnings)?;
    match shoal_prompt::parse_layer(&text) {
        Ok(v) => Some(migrate_template(v, warnings)),
        Err(e) => {
            warnings.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

fn read_prompt_text(path: &Path, warnings: &mut Vec<String>) -> Option<String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            warnings.push(format!("{}: {error}", path.display()));
            return None;
        }
    };
    if !metadata.is_file() {
        warnings.push(format!(
            "{}: prompt config is not a regular file",
            path.display()
        ));
        return None;
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            warnings.push(format!("{}: {error}", path.display()));
            return None;
        }
    };
    if file.metadata().is_ok_and(|metadata| !metadata.is_file()) {
        warnings.push(format!(
            "{}: prompt config is not a regular file",
            path.display()
        ));
        return None;
    }
    let mut bytes = Vec::with_capacity(8 * 1024);
    if let Err(error) = file
        .take((shoal_prompt::PROMPT_MAX_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
    {
        warnings.push(format!("{}: {error}", path.display()));
        return None;
    }
    if bytes.len() > shoal_prompt::PROMPT_MAX_SOURCE_BYTES {
        warnings.push(format!(
            "{}: prompt config exceeds the {}-byte limit",
            path.display(),
            shoal_prompt::PROMPT_MAX_SOURCE_BYTES
        ));
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(_) => {
            warnings.push(format!(
                "{}: prompt config is not valid UTF-8",
                path.display()
            ));
            None
        }
    }
}

fn prompt_environment(warnings: &mut Vec<String>) -> Vec<(String, String)> {
    let mut overrides = Vec::with_capacity(PROMPT_ENV_NAMES.len());
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        if !PROMPT_ENV_NAMES.contains(&name) {
            continue;
        }
        match value.into_string() {
            Ok(value) => overrides.push((name.to_string(), value)),
            Err(_) => warnings.push(format!(
                "prompt: environment override {name} ignored because it is not valid UTF-8"
            )),
        }
    }
    overrides
}

/// site/content/internals/prompt-editor-lsp.md migration: a `[prompt]` table with the old `template` key and no new
/// `format` key is rewritten to `format.left`, `{cwd}` → `$directory`.
fn migrate_template(mut prompt: toml::Value, warnings: &mut Vec<String>) -> toml::Value {
    let Some(table) = prompt.as_table_mut() else {
        return prompt;
    };
    let has_template = table.contains_key("template");
    let has_format = table.contains_key("format");
    if has_template && has_format {
        warnings.push(
            "prompt: both 'template' and 'format' set; 'format' wins, 'template' ignored".into(),
        );
        table.remove("template");
    } else if has_template
        && let Some(t) = table
            .remove("template")
            .and_then(|v| v.as_str().map(str::to_string))
    {
        let left = t.replace("{cwd}", "$directory");
        let mut fmt = toml::map::Map::new();
        fmt.insert("left".into(), toml::Value::String(left));
        table.insert("format".into(), toml::Value::Table(fmt));
        warnings.push(
            "prompt: 'template' is deprecated; migrated to format.left — update your config to silence this warning".into(),
        );
    }
    prompt
}

#[cfg(test)]
mod bounded_input_tests {
    use super::*;

    #[test]
    fn ordinary_prompt_layer_and_template_migration_remain_compatible() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("prompt.toml");
        fs::write(&path, "template = '{cwd} > '").unwrap();
        let mut warnings = Vec::new();
        let layer = read_root_table(&path, &mut warnings).unwrap();
        assert_eq!(
            layer.get("format").unwrap().get("left").unwrap().as_str(),
            Some("$directory > ")
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("deprecated"))
        );
    }

    #[test]
    fn project_custom_commands_are_removed_before_layer_merge() {
        let mut layer = shoal_prompt::parse_layer(
            r#"
[format]
left = "$custom_project"
[module.custom.project]
command = "touch should-not-run"
"#,
        )
        .unwrap();
        let mut warnings = Vec::new();
        remove_project_custom_commands(&mut layer, &mut warnings);
        assert!(
            layer
                .get("module")
                .and_then(|module| module.get("custom"))
                .is_none()
        );
        assert!(warnings.iter().any(|warning| warning.contains("ignored")));
        assert_eq!(
            layer.get("format").unwrap().get("left").unwrap().as_str(),
            Some("$custom_project")
        );
    }

    #[test]
    fn oversized_non_utf8_and_non_file_layers_degrade_with_warnings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("prompt.toml");
        let file = File::create(&path).unwrap();
        file.set_len((shoal_prompt::PROMPT_MAX_SOURCE_BYTES + 1) as u64)
            .unwrap();
        let mut warnings = Vec::new();
        assert!(read_root_table(&path, &mut warnings).is_none());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("byte limit"))
        );

        fs::write(&path, [0xff]).unwrap();
        warnings.clear();
        assert!(read_root_table(&path, &mut warnings).is_none());
        assert!(warnings.iter().any(|warning| warning.contains("UTF-8")));

        warnings.clear();
        assert!(read_root_table(directory.path(), &mut warnings).is_none());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("regular file"))
        );
    }

    #[test]
    fn production_prompt_loader_has_no_unbounded_text_read() {
        let source = include_str!("config.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("read_to_string"));
        assert!(!production.contains("std::env::vars().collect"));
    }
}

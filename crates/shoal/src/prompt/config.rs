//! Prompt configuration discovery, layering, and migration.

use super::*;

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
    if let Some(v) = read_prompt_table(&cwd.join(".shoal.toml"), &mut warnings) {
        layers.push(v);
    }

    // Environment overrides (highest precedence).
    let env: Vec<(String, String)> = std::env::vars().collect();
    layers.push(shoal_prompt::env_overrides(&env));

    let config = shoal_prompt::load(layers, &mut warnings);
    (config, warnings)
}

/// Read a config file's `[prompt]` sub-table as a prompt-contents-shaped value,
/// applying the site/content/internals/prompt-editor-lsp.md `template` → `format.left` migration.
fn read_prompt_table(path: &Path, warnings: &mut Vec<String>) -> Option<toml::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = match toml::from_str(&text) {
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
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<toml::Value>(&text) {
        Ok(v) => Some(migrate_template(v, warnings)),
        Err(e) => {
            warnings.push(format!("{}: {e}", path.display()));
            None
        }
    }
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

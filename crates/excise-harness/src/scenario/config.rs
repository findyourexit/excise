//! Reading one setting out of the configuration file a program saved.
//!
//! Both runners answer an `expect_config` step with [`config_setting`], so the two cannot disagree
//! about what a key means.

use toml::{Table, Value};

/// The string at `key` in the configuration file whose text is `text`.
///
/// `key` is a dotted path: every name but the last is a table, and the last is the setting. The
/// scenario format only ever asks for a string, so any other value counts as a mismatch.
///
/// # Errors
///
/// Returns why there is nothing to compare: the text is not TOML, a table on the way is missing,
/// or the setting is missing or is not a string.
pub fn config_setting(text: &str, key: &str) -> Result<String, String> {
    let document: Table = text
        .parse()
        .map_err(|error| format!("the configuration file is not valid TOML: {error}"))?;
    let (tables, setting) = key
        .rsplit_once('.')
        .map_or((None, key), |(tables, setting)| (Some(tables), setting));
    let mut table = &document;
    if let Some(tables) = tables {
        for name in tables.split('.') {
            table = table
                .get(name)
                .and_then(Value::as_table)
                .ok_or_else(|| format!("the configuration file has no table `{tables}`"))?;
        }
    }
    let value = table
        .get(setting)
        .ok_or_else(|| format!("the configuration file has no `{key}`"))?;
    value.as_str().map(str::to_owned).ok_or_else(|| {
        format!(
            "`{key}` is not a string: it holds a value of type {}",
            value.type_str()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::config_setting;

    const SAVED: &str = "version = 1\n\n[runtime]\ntheme = \"excise-light\"\nmouse = true\n";

    #[test]
    fn a_dotted_key_reads_the_string_in_its_table() {
        assert_eq!(
            config_setting(SAVED, "runtime.theme").as_deref(),
            Ok("excise-light")
        );
    }

    #[test]
    fn a_key_without_a_dot_reads_a_top_level_setting() {
        let text = "name = \"top\"\n";
        assert_eq!(config_setting(text, "name").as_deref(), Ok("top"));
    }

    #[test]
    fn what_is_missing_or_not_a_string_is_said_so() {
        for (key, expected) in [
            ("runtime.keymap", "no `runtime.keymap`"),
            ("scanner.threads", "no table `scanner`"),
            ("runtime.theme.name", "no table `runtime.theme`"),
            ("runtime.mouse", "`runtime.mouse` is not a string"),
            ("version", "`version` is not a string"),
        ] {
            let message = config_setting(SAVED, key).expect_err("there is no string to compare");
            assert!(message.contains(expected), "{key}: {message}");
        }
    }

    #[test]
    fn text_that_is_not_toml_is_an_error_not_a_missing_setting() {
        let message = config_setting("runtime = [", "runtime.theme").expect_err("not TOML");
        assert!(message.contains("not valid TOML"), "{message}");
    }
}

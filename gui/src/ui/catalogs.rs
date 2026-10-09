use std::collections::BTreeMap;

type Key = (String, String, String);

#[derive(Default)]
struct Entry {
    context: String,
    id: String,
    plural: String,
    translations: BTreeMap<String, String>,
    fuzzy: bool,
}

fn parse(content: &str) -> BTreeMap<Key, Entry> {
    let mut entries = BTreeMap::new();
    let mut entry = Entry::default();
    let mut field = String::new();
    for line in content.lines().chain(std::iter::once("")) {
        let line = line.trim();
        if line.is_empty() {
            if !entry.id.is_empty() {
                entries.insert(
                    (
                        entry.context.clone(),
                        entry.id.clone(),
                        entry.plural.clone(),
                    ),
                    entry,
                );
            }
            entry = Entry::default();
            field.clear();
            continue;
        }
        if let Some(flags) = line.strip_prefix("#,") {
            entry.fuzzy |= flags.split(',').any(|flag| flag.trim() == "fuzzy");
        }
        if line.starts_with('#') {
            continue;
        }
        let quoted = if line.starts_with('"') {
            line
        } else {
            let (name, value) = line.split_once(' ').expect("PO directive");
            field = name.into();
            value.trim()
        };
        let value: String = serde_json::from_str(quoted).expect("PO quoted string");
        match field.as_str() {
            "msgctxt" => entry.context.push_str(&value),
            "msgid" => entry.id.push_str(&value),
            "msgid_plural" => entry.plural.push_str(&value),
            name if name.starts_with("msgstr") => entry
                .translations
                .entry(field.clone())
                .or_default()
                .push_str(&value),
            _ => panic!("Unknown PO directive: {field}"),
        }
    }
    entries
}

fn translation_ids(mut source: &str) -> Vec<String> {
    let mut ids = Vec::new();
    while !source.is_empty() {
        if let Some(comment) = source.strip_prefix("//") {
            source = comment.split_once('\n').map_or("", |(_, rest)| rest);
            continue;
        }
        if let Some(comment) = source.strip_prefix("/*") {
            source = comment.split_once("*/").expect("Closed Slint comment").1;
            continue;
        }
        let mut translation = false;
        if let Some(rest) = source.strip_prefix("@tr") {
            if let Some(rest) = rest.trim_start().strip_prefix('(') {
                source = rest.trim_start();
                translation = true;
            }
        }
        if source.starts_with('"') {
            let mut escaped = false;
            let end = source
                .char_indices()
                .skip(1)
                .find_map(|(index, character)| {
                    if escaped {
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == '"' {
                        return Some(index + 1);
                    }
                    None
                })
                .expect("Closed Slint string");
            if translation {
                ids.push(serde_json::from_str(&source[..end]).expect("Slint translation string"));
            }
            source = &source[end..];
        } else {
            source = &source[source.chars().next().unwrap().len_utf8()..];
        }
    }
    ids
}

#[test]
fn slint_translations_exist_in_template() {
    let template = parse(include_str!("../../lang/juicity-gui.pot"));
    let ids: std::collections::BTreeSet<_> =
        template.values().map(|entry| entry.id.as_str()).collect();
    let mut directories = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui")];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                directories.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "slint")
            {
                for id in translation_ids(&std::fs::read_to_string(&path).unwrap()) {
                    assert!(
                        ids.contains(id.as_str()),
                        "{}: missing msgid {id:?} in juicity-gui.pot",
                        path.display()
                    );
                }
            }
        }
    }
}

#[test]
fn bundled_catalogs_are_complete() {
    let template = parse(include_str!("../../lang/juicity-gui.pot"));
    for (language, content) in [
        (
            "en",
            include_str!("../../lang/en/LC_MESSAGES/juicity-gui.po"),
        ),
        (
            "zh_CN",
            include_str!("../../lang/zh_CN/LC_MESSAGES/juicity-gui.po"),
        ),
        (
            "zh_TW",
            include_str!("../../lang/zh_TW/LC_MESSAGES/juicity-gui.po"),
        ),
    ] {
        let catalog = parse(content);
        for key in template.keys() {
            let entry = catalog
                .get(key)
                .unwrap_or_else(|| panic!("{language}: missing {key:?}"));
            assert!(!entry.fuzzy, "{language}: fuzzy {key:?}");
            let forms: &[&str] = if key.2.is_empty() {
                &["msgstr"]
            } else if language == "en" {
                &["msgstr[0]", "msgstr[1]"]
            } else {
                &["msgstr[0]"]
            };
            for form in forms {
                assert!(
                    entry
                        .translations
                        .get(*form)
                        .is_some_and(|value| !value.trim().is_empty()),
                    "{language}: empty {form} for {key:?}"
                );
            }
        }
    }
}

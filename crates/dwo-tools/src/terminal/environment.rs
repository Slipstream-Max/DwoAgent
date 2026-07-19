use std::collections::HashMap;

#[cfg(windows)]
use std::collections::HashSet;

#[cfg(windows)]
use winreg::{
    RegKey,
    enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
};

pub(super) fn current() -> HashMap<String, String> {
    let mut environment = std::env::vars().collect();
    #[cfg(windows)]
    merge_windows_path(&mut environment);
    environment
}

#[cfg(windows)]
fn merge_windows_path(environment: &mut HashMap<String, String>) {
    let current = environment_value(environment, "PATH").map(str::to_owned);
    let user = registry_path(RegKey::predef(HKEY_CURRENT_USER))
        .map(|path| expand_windows_percent_variables(&path, environment));
    let machine = registry_path(RegKey::predef(HKEY_LOCAL_MACHINE))
        .map(|path| expand_windows_percent_variables(&path, environment));
    if let Some(path) = merge_path_values([current, user, machine]) {
        set_environment_value(environment, "PATH", path);
    }
}

#[cfg(windows)]
fn environment_value<'a>(environment: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    environment
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_str())
}

#[cfg(windows)]
fn set_environment_value(environment: &mut HashMap<String, String>, key: &str, value: String) {
    let matching_keys = environment
        .keys()
        .filter(|candidate| candidate.eq_ignore_ascii_case(key))
        .cloned()
        .collect::<Vec<_>>();
    for matching_key in matching_keys {
        environment.remove(&matching_key);
    }
    environment.insert(key.to_string(), value);
}

#[cfg(windows)]
fn registry_path(hive: RegKey) -> Option<String> {
    hive.open_subkey("Environment")
        .or_else(|_| {
            hive.open_subkey("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment")
        })
        .ok()?
        .get_value("Path")
        .ok()
}

#[cfg(windows)]
fn merge_path_values(values: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for value in values.into_iter().flatten() {
        for entry in std::env::split_paths(&value) {
            if entry.as_os_str().is_empty() {
                continue;
            }
            let key = entry.to_string_lossy().to_ascii_lowercase();
            if seen.insert(key) {
                entries.push(entry);
            }
        }
    }
    std::env::join_paths(entries)
        .ok()
        .map(|value| value.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn expand_windows_percent_variables(input: &str, environment: &HashMap<String, String>) -> String {
    let mut output = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('%') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            output.push('%');
            output.push_str(after);
            return output;
        };
        let name = &after[..end];
        if name.is_empty() {
            output.push_str("%%");
        } else if let Some(value) = environment_value(environment, name) {
            output.push_str(value);
        } else {
            output.push('%');
            output.push_str(name);
            output.push('%');
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    output
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn merged_windows_path_deduplicates_entries_in_source_order() {
        let path = merge_path_values([
            Some(r"C:\base;C:\shared".to_string()),
            Some(r"C:\user;C:\shared".to_string()),
            Some(r"C:\system;C:\base".to_string()),
        ])
        .unwrap();
        let entries = std::env::split_paths(&path).collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![
                PathBuf::from(r"C:\base"),
                PathBuf::from(r"C:\shared"),
                PathBuf::from(r"C:\user"),
                PathBuf::from(r"C:\system"),
            ]
        );
    }

    #[test]
    fn path_is_written_once_with_a_canonical_key() {
        let mut environment = HashMap::from([
            ("Path".to_string(), r"C:\base".to_string()),
            ("USERPROFILE".to_string(), r"C:\Users\dwo".to_string()),
        ]);
        set_environment_value(&mut environment, "PATH", r"C:\merged".to_string());
        assert_eq!(
            environment.get("PATH").map(String::as_str),
            Some(r"C:\merged")
        );
        assert!(!environment.contains_key("Path"));
    }

    #[test]
    fn registry_path_variables_use_the_daemon_environment() {
        let environment = HashMap::from([("USERPROFILE".to_string(), r"C:\Users\dwo".to_string())]);
        assert_eq!(
            expand_windows_percent_variables(r"%USERPROFILE%\bin", &environment),
            r"C:\Users\dwo\bin"
        );
    }
}

use crate::config::Config;
use std::path::{Path, PathBuf};

const DEFAULT_NAME: &str = "config.toml";

/// Resolve the config path: an explicit argument always wins, otherwise search
/// the working directory and the executable directory for the default name.
pub fn resolve(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(PathBuf::from(path));
    }
    let directories = search_directories()?;
    find_config(&directories).ok_or_else(|| missing(&directories))
}

/// Load a TOML config file.
pub fn load(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    Ok(toml::from_str(&text)?)
}

fn search_directories() -> Result<Vec<PathBuf>, String> {
    let working =
        std::env::current_dir().map_err(|_| "Cannot resolve the working directory".to_owned())?;
    let mut directories = vec![working];
    let executable = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    if let Some(directory) = executable {
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    Ok(directories)
}

fn find_config(directories: &[PathBuf]) -> Option<PathBuf> {
    directories
        .iter()
        .map(|directory| directory.join(DEFAULT_NAME))
        .find(|candidate| candidate.is_file())
}

fn missing(directories: &[PathBuf]) -> String {
    let searched: Vec<String> = directories
        .iter()
        .map(|directory| directory.join(DEFAULT_NAME).display().to_string())
        .collect();
    format!("No config file found; searched {}", searched.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_path_wins() {
        assert_eq!(
            resolve(Some("custom.toml")).unwrap(),
            PathBuf::from("custom.toml")
        );
    }

    #[test]
    fn first_directory_with_a_config_wins() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(second.path().join(DEFAULT_NAME), "").unwrap();
        let directories = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        assert_eq!(
            find_config(&directories),
            Some(second.path().join(DEFAULT_NAME))
        );
        std::fs::write(first.path().join(DEFAULT_NAME), "").unwrap();
        assert_eq!(
            find_config(&directories),
            Some(first.path().join(DEFAULT_NAME))
        );
    }

    #[test]
    fn loads_toml_config_and_builds_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("docs");
        std::fs::create_dir(&root).unwrap();
        let path = directory.path().join(DEFAULT_NAME);
        std::fs::write(
            &path,
            format!(
                "listen = \"127.0.0.1:0\"\nusername = \"li\"\npassword = \"password-at-least-8-bytes\"\n[[exports]]\npath = \"{}\"\n",
                root.display()
            ),
        )
        .unwrap();
        assert!(load(&path).unwrap().state().is_ok());
    }

    #[test]
    fn rejects_invalid_toml() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(DEFAULT_NAME);
        std::fs::write(&path, "listen = [unterminated").unwrap();
        assert!(load(&path).is_err());
    }
}

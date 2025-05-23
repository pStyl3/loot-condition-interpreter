use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{read_dir, DirEntry, File};
use std::hash::Hasher;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use esplugin::ParseOptions;
use regex::Regex;

use super::path::{has_plugin_file_extension, normalise_file_name, resolve_path};
use super::version::Version;
use super::{ComparisonOperator, Function};
use crate::{Error, GameType, State};

fn evaluate_file_path(state: &State, file_path: &Path) -> bool {
    resolve_path(state, file_path).exists()
}

fn is_match(game_type: GameType, regex: &Regex, file_name: &OsStr) -> bool {
    normalise_file_name(game_type, file_name)
        .to_str()
        .is_some_and(|s| regex.is_match(s))
}

fn evaluate_dir_entries_from_base_paths<'a>(
    base_path_iter: impl Iterator<Item = &'a PathBuf>,
    parent_path: &Path,
    mut evaluator: impl FnMut(DirEntry) -> bool,
) -> Result<bool, Error> {
    for base_path in base_path_iter {
        let parent_path = base_path.join(parent_path);
        let Ok(dir_iterator) = read_dir(&parent_path) else {
            return Ok(false);
        };

        for entry in dir_iterator {
            let entry = entry.map_err(|e| Error::IoError(parent_path.clone(), e))?;
            if evaluator(entry) {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn evaluate_dir_entries(
    state: &State,
    parent_path: &Path,
    evaluator: impl FnMut(DirEntry) -> bool,
) -> Result<bool, Error> {
    match state.game_type {
        GameType::OpenMW => evaluate_dir_entries_from_base_paths(
            state
                .additional_data_paths
                .iter()
                .rev()
                .chain(std::iter::once(&state.data_path)),
            parent_path,
            evaluator,
        ),
        _ => evaluate_dir_entries_from_base_paths(
            state
                .additional_data_paths
                .iter()
                .chain(std::iter::once(&state.data_path)),
            parent_path,
            evaluator,
        ),
    }
}

fn evaluate_file_regex(state: &State, parent_path: &Path, regex: &Regex) -> Result<bool, Error> {
    let evaluator = |entry: DirEntry| is_match(state.game_type, regex, &entry.file_name());

    evaluate_dir_entries(state, parent_path, evaluator)
}

fn evaluate_file_size(state: &State, path: &Path, size: u64) -> Result<bool, Error> {
    std::fs::metadata(resolve_path(state, path))
        .map(|m| m.len() == size)
        .or(Ok(false))
}

fn evaluate_readable(state: &State, path: &Path) -> bool {
    if path.is_dir() {
        read_dir(resolve_path(state, path)).is_ok()
    } else {
        File::open(resolve_path(state, path)).is_ok()
    }
}

fn evaluate_is_executable(state: &State, path: &Path) -> bool {
    Version::is_readable(&resolve_path(state, path))
}

fn evaluate_many(state: &State, parent_path: &Path, regex: &Regex) -> Result<bool, Error> {
    // Share the found_one state across all data paths because they're all
    // treated as if they were merged into one directory.
    let mut found_one = false;
    let evaluator = |entry: DirEntry| {
        if is_match(state.game_type, regex, &entry.file_name()) {
            if found_one {
                true
            } else {
                found_one = true;
                false
            }
        } else {
            false
        }
    };

    evaluate_dir_entries(state, parent_path, evaluator)
}

fn evaluate_active_path(state: &State, path: &Path) -> bool {
    path.to_str()
        .is_some_and(|s| state.active_plugins.contains(&s.to_lowercase()))
}

fn evaluate_active_regex(state: &State, regex: &Regex) -> bool {
    state.active_plugins.iter().any(|p| regex.is_match(p))
}

fn parse_plugin(state: &State, file_path: &Path) -> Option<esplugin::Plugin> {
    use esplugin::GameId;

    let game_id = match state.game_type {
        GameType::Morrowind | GameType::OpenMW => GameId::Morrowind,
        GameType::Oblivion => GameId::Oblivion,
        GameType::Skyrim => GameId::Skyrim,
        GameType::SkyrimSE | GameType::SkyrimVR => GameId::SkyrimSE,
        GameType::Fallout3 => GameId::Fallout3,
        GameType::FalloutNV => GameId::FalloutNV,
        GameType::Fallout4 | GameType::Fallout4VR => GameId::Fallout4,
        GameType::Starfield => GameId::Starfield,
    };

    let path = resolve_path(state, file_path);

    let mut plugin = esplugin::Plugin::new(game_id, &path);

    plugin
        .parse_file(ParseOptions::header_only())
        .is_ok()
        .then_some(plugin)
}

fn evaluate_is_master(state: &State, file_path: &Path) -> bool {
    if state.game_type == GameType::OpenMW {
        false
    } else {
        parse_plugin(state, file_path).is_some_and(|plugin| plugin.is_master_file())
    }
}

#[expect(clippy::iter_over_hash_type)]
fn evaluate_many_active(state: &State, regex: &Regex) -> bool {
    let mut found_one = false;
    for active_plugin in &state.active_plugins {
        if regex.is_match(active_plugin) {
            if found_one {
                return true;
            }
            found_one = true;
        }
    }

    false
}

fn lowercase(path: &Path) -> Option<String> {
    path.to_str().map(str::to_lowercase)
}

fn evaluate_checksum(state: &State, file_path: &Path, crc: u32) -> Result<bool, Error> {
    if let Ok(reader) = state.crc_cache.read() {
        if let Some(key) = lowercase(file_path) {
            if let Some(cached_crc) = reader.get(&key) {
                return Ok(*cached_crc == crc);
            }
        }
    }

    let path = resolve_path(state, file_path);

    if !path.is_file() {
        return Ok(false);
    }

    let io_error_mapper = |e| Error::IoError(file_path.to_path_buf(), e);
    let file = File::open(path).map_err(io_error_mapper)?;
    let mut reader = BufReader::new(file);
    let mut hasher = crc32fast::Hasher::new();

    let mut buffer = reader.fill_buf().map_err(io_error_mapper)?;
    while !buffer.is_empty() {
        hasher.write(buffer);
        let length = buffer.len();
        reader.consume(length);

        buffer = reader.fill_buf().map_err(io_error_mapper)?;
    }

    let calculated_crc = hasher.finalize();
    let mut writer = state.crc_cache.write().unwrap_or_else(|mut e| {
        **e.get_mut() = HashMap::new();
        state.crc_cache.clear_poison();
        e.into_inner()
    });

    if let Some(key) = lowercase(file_path) {
        writer.insert(key, calculated_crc);
    }

    Ok(calculated_crc == crc)
}

fn lowercase_filename(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(OsStr::to_str)
        .map(str::to_lowercase)
}

fn get_version(state: &State, file_path: &Path) -> Result<Option<Version>, Error> {
    if !file_path.is_file() {
        return Ok(None);
    }

    if let Some(key) = lowercase_filename(file_path) {
        if let Some(version) = state.plugin_versions.get(&key) {
            return Ok(Some(Version::from(version.as_str())));
        }
    }

    if has_plugin_file_extension(state.game_type, file_path) {
        Ok(None)
    } else {
        Version::read_file_version(file_path)
    }
}

fn get_product_version(file_path: &Path) -> Result<Option<Version>, Error> {
    if file_path.is_file() {
        Version::read_product_version(file_path)
    } else {
        Ok(None)
    }
}

fn compare_versions(
    actual_version: &Version,
    comparator: ComparisonOperator,
    given_version: &str,
) -> bool {
    let given_version = &Version::from(given_version);

    match comparator {
        ComparisonOperator::Equal => actual_version == given_version,
        ComparisonOperator::NotEqual => actual_version != given_version,
        ComparisonOperator::LessThan => actual_version < given_version,
        ComparisonOperator::GreaterThan => actual_version > given_version,
        ComparisonOperator::LessThanOrEqual => actual_version <= given_version,
        ComparisonOperator::GreaterThanOrEqual => actual_version >= given_version,
    }
}

fn evaluate_version<F>(
    state: &State,
    file_path: &Path,
    given_version: &str,
    comparator: ComparisonOperator,
    read_version: F,
) -> Result<bool, Error>
where
    F: Fn(&State, &Path) -> Result<Option<Version>, Error>,
{
    let file_path = resolve_path(state, file_path);
    let Some(actual_version) = read_version(state, &file_path)? else {
        return Ok(comparator == ComparisonOperator::NotEqual
            || comparator == ComparisonOperator::LessThan
            || comparator == ComparisonOperator::LessThanOrEqual);
    };

    Ok(compare_versions(&actual_version, comparator, given_version))
}

fn evaluate_filename_version(
    state: &State,
    parent_path: &Path,
    regex: &Regex,
    version: &str,
    comparator: ComparisonOperator,
) -> Result<bool, Error> {
    let evaluator = |entry: DirEntry| {
        normalise_file_name(state.game_type, &entry.file_name())
            .to_str()
            .and_then(|s| regex.captures(s))
            .and_then(|c| c.get(1))
            .map(|m| Version::from(m.as_str()))
            .is_some_and(|v| compare_versions(&v, comparator, version))
    };

    evaluate_dir_entries(state, parent_path, evaluator)
}

fn evaluate_description_contains(state: &State, file_path: &Path, regex: &Regex) -> bool {
    parse_plugin(state, file_path)
        .and_then(|plugin| plugin.description().unwrap_or(None))
        .is_some_and(|description| regex.is_match(&description))
}

impl Function {
    pub fn eval(&self, state: &State) -> Result<bool, Error> {
        if self.is_slow() {
            if let Ok(reader) = state.condition_cache.read() {
                if let Some(cached_result) = reader.get(self) {
                    return Ok(*cached_result);
                }
            }
        }

        let result = match self {
            Function::FilePath(f) => Ok(evaluate_file_path(state, f)),
            Function::FileRegex(p, r) => evaluate_file_regex(state, p, r),
            Function::FileSize(p, s) => evaluate_file_size(state, p, *s),
            Function::Readable(p) => Ok(evaluate_readable(state, p)),
            Function::IsExecutable(p) => Ok(evaluate_is_executable(state, p)),
            Function::ActivePath(p) => Ok(evaluate_active_path(state, p)),
            Function::ActiveRegex(r) => Ok(evaluate_active_regex(state, r)),
            Function::IsMaster(p) => Ok(evaluate_is_master(state, p)),
            Function::Many(p, r) => evaluate_many(state, p, r),
            Function::ManyActive(r) => Ok(evaluate_many_active(state, r)),
            Function::Checksum(path, crc) => evaluate_checksum(state, path, *crc),
            Function::Version(p, v, c) => evaluate_version(state, p, v, *c, get_version),
            Function::ProductVersion(p, v, c) => {
                evaluate_version(state, p, v, *c, |_, p| get_product_version(p))
            }
            Function::FilenameVersion(p, r, v, c) => evaluate_filename_version(state, p, r, v, *c),
            Function::DescriptionContains(p, r) => Ok(evaluate_description_contains(state, p, r)),
        };

        if self.is_slow() {
            if let Ok(function_result) = result {
                let mut writer = state.condition_cache.write().unwrap_or_else(|mut e| {
                    **e.get_mut() = HashMap::new();
                    state.condition_cache.clear_poison();
                    e.into_inner()
                });

                writer.insert(self.clone(), function_result);
            }
        }

        result
    }

    /// Some functions are faster to evaluate than to look their result up in
    /// the cache, as the data they operate on are already cached separately and
    /// the operation is simple.
    fn is_slow(&self) -> bool {
        !matches!(
            self,
            Self::ActivePath(_) | Self::ActiveRegex(_) | Self::ManyActive(_) | Self::Checksum(_, _)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOWERCASE_NON_ASCII: &str = "\u{20ac}\u{192}.";

    use std::fs::{copy, create_dir_all, remove_file};
    use std::sync::RwLock;

    use regex::RegexBuilder;
    use tempfile::tempdir;

    fn state<T: Into<PathBuf>>(data_path: T) -> State {
        state_with_active_plugins(data_path, &[])
    }

    fn state_with_active_plugins<T: Into<PathBuf>>(data_path: T, active_plugins: &[&str]) -> State {
        state_with_data(data_path, Vec::default(), active_plugins, &[])
    }

    fn state_with_versions<T: Into<PathBuf>>(
        data_path: T,
        plugin_versions: &[(&str, &str)],
    ) -> State {
        state_with_data(data_path, Vec::default(), &[], plugin_versions)
    }

    fn state_with_data<T: Into<PathBuf>>(
        data_path: T,
        additional_data_paths: Vec<T>,
        active_plugins: &[&str],
        plugin_versions: &[(&str, &str)],
    ) -> State {
        let data_path = data_path.into();
        if !data_path.exists() {
            create_dir_all(&data_path).unwrap();
        }

        let additional_data_paths = additional_data_paths
            .into_iter()
            .map(|data_path| {
                let data_path: PathBuf = data_path.into();
                if !data_path.exists() {
                    create_dir_all(&data_path).unwrap();
                }
                data_path
            })
            .collect();

        State {
            game_type: GameType::Oblivion,
            data_path,
            additional_data_paths,
            active_plugins: active_plugins.iter().map(|s| s.to_lowercase()).collect(),
            crc_cache: RwLock::default(),
            plugin_versions: plugin_versions
                .iter()
                .map(|(p, v)| (p.to_lowercase(), (*v).to_owned()))
                .collect(),
            condition_cache: RwLock::default(),
        }
    }

    fn regex(string: &str) -> Regex {
        RegexBuilder::new(string)
            .case_insensitive(true)
            .build()
            .unwrap()
    }

    #[cfg(not(windows))]
    fn make_path_unreadable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o200);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn evaluate_dir_entries_should_check_additional_paths_in_order_then_data_path() {
        let state = state_with_data(
            "./tests/testing-plugins/SkyrimSE",
            vec![
                "./tests/testing-plugins/Oblivion",
                "./tests/testing-plugins/Skyrim",
            ],
            &[],
            &[],
        );

        let mut paths = Vec::new();
        let evaluator = |entry: DirEntry| {
            if entry.file_name() == "Blank.esp" {
                paths.push(
                    entry
                        .path()
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .to_path_buf(),
                );
            }
            false
        };
        let result = evaluate_dir_entries(&state, Path::new("Data"), evaluator).unwrap();

        assert!(!result);
        assert_eq!(
            vec![
                state.additional_data_paths[0].clone(),
                state.additional_data_paths[1].clone(),
                state.data_path,
            ],
            paths
        );
    }

    #[test]
    fn evaluate_dir_entries_should_check_additional_paths_in_reverse_order_then_data_path_for_openmw(
    ) {
        let mut state = state_with_data(
            "./tests/testing-plugins/SkyrimSE",
            vec![
                "./tests/testing-plugins/Oblivion",
                "./tests/testing-plugins/Skyrim",
            ],
            &[],
            &[],
        );
        state.game_type = GameType::OpenMW;

        let mut paths = Vec::new();
        let evaluator = |entry: DirEntry| {
            if entry.file_name() == "Blank.esp" {
                paths.push(
                    entry
                        .path()
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .to_path_buf(),
                );
            }
            false
        };
        let result = evaluate_dir_entries(&state, Path::new("Data"), evaluator).unwrap();

        assert!(!result);
        assert_eq!(
            vec![
                state.additional_data_paths[1].clone(),
                state.additional_data_paths[0].clone(),
                state.data_path,
            ],
            paths
        );
    }

    #[test]
    fn parse_plugin_should_parse_openmw_plugins() {
        let mut state = state(Path::new("./tests/testing-plugins/Morrowind/Data Files"));
        state.game_type = GameType::OpenMW;

        let plugin = parse_plugin(&state, Path::new("Blank.esp"));

        assert!(plugin.is_some());
    }

    #[test]
    fn function_file_path_eval_should_return_true_if_the_file_exists_relative_to_the_data_path() {
        let function = Function::FilePath(PathBuf::from("Cargo.toml"));
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_path_eval_should_return_true_if_given_a_plugin_that_is_ghosted() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esp"),
            state.data_path.join("Blank.esp.ghost"),
        )
        .unwrap();

        let function = Function::FilePath(PathBuf::from("Blank.esp"));

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_path_eval_should_not_check_for_ghosted_non_plugin_file() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("Cargo.toml"),
            state.data_path.join("Cargo.toml.ghost"),
        )
        .unwrap();

        let function = Function::FilePath(PathBuf::from("Cargo.toml"));

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_path_eval_should_return_false_if_the_file_does_not_exist() {
        let function = Function::FilePath(PathBuf::from("missing"));
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_regex_eval_should_be_false_if_no_directory_entries_match() {
        let function = Function::FileRegex(PathBuf::from("."), regex("missing"));
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_regex_eval_should_be_false_if_the_parent_path_part_is_not_a_directory() {
        let function = Function::FileRegex(PathBuf::from("missing"), regex("Cargo.*"));
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_regex_eval_should_be_true_if_a_directory_entry_matches() {
        let function = Function::FileRegex(
            PathBuf::from("tests/testing-plugins/Oblivion/Data"),
            regex("Blank\\.esp"),
        );
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_regex_eval_should_trim_ghost_plugin_extension_before_matching_against_regex() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esm"),
            state.data_path.join("Blank.esm.ghost"),
        )
        .unwrap();

        let function = Function::FileRegex(PathBuf::from("."), regex("^Blank\\.esm$"));

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_regex_eval_should_check_all_configured_data_paths() {
        let function = Function::FileRegex(PathBuf::from("Data"), regex("Blank\\.esp"));
        let state = state_with_data("./src", vec!["./tests/testing-plugins/Oblivion"], &[], &[]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_size_eval_should_return_false_if_file_does_not_exist() {
        let function = Function::FileSize("missing.esp".into(), 55);
        let state = state_with_data(
            "./src",
            vec!["./tests/testing-plugins/Oblivion/Data"],
            &[],
            &[],
        );

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_size_eval_should_return_false_if_file_size_is_different() {
        let function = Function::FileSize("Blank.esp".into(), 10);
        let state = state_with_data(
            "./src",
            vec!["./tests/testing-plugins/Oblivion/Data"],
            &[],
            &[],
        );

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_size_eval_should_return_true_if_file_size_is_equal() {
        let function = Function::FileSize("Blank.esp".into(), 55);
        let state = state_with_data(
            "./src",
            vec!["./tests/testing-plugins/Oblivion/Data"],
            &[],
            &[],
        );

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_size_eval_should_return_true_if_given_a_plugin_that_is_ghosted() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esp"),
            state.data_path.join("Blank.esp.ghost"),
        )
        .unwrap();

        let function = Function::FileSize(PathBuf::from("Blank.esp"), 55);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_file_size_eval_should_not_check_for_ghosted_non_plugin_file() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.bsa"),
            state.data_path.join("Blank.bsa.ghost"),
        )
        .unwrap();

        let function = Function::FileSize(PathBuf::from("Blank.bsa"), 736);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_readable_eval_should_be_true_for_a_file_that_can_be_opened_as_read_only() {
        let function = Function::Readable(PathBuf::from("Cargo.toml"));
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_readable_eval_should_be_true_for_a_folder_that_can_be_read() {
        let function = Function::Readable(PathBuf::from("tests"));
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_readable_eval_should_be_false_for_a_file_that_does_not_exist() {
        let function = Function::Readable(PathBuf::from("missing"));
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn function_readable_eval_should_be_false_for_a_file_that_is_not_readable() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        let relative_path = "unreadable";
        let file_path = state.data_path.join(relative_path);

        // Create a file and open it with exclusive access so that the readable
        // function eval isn't able to open the file in read-only mode.
        let _file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(&file_path);

        assert!(file_path.exists());

        let function = Function::Readable(PathBuf::from(relative_path));

        assert!(!function.eval(&state).unwrap());
    }

    #[cfg(not(windows))]
    #[test]
    fn function_readable_eval_should_be_false_for_a_file_that_is_not_readable() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        let relative_path = "unreadable";
        let file_path = state.data_path.join(relative_path);

        std::fs::write(&file_path, "").unwrap();
        make_path_unreadable(&file_path);

        assert!(file_path.exists());

        let function = Function::Readable(PathBuf::from(relative_path));

        assert!(!function.eval(&state).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn function_readable_eval_should_be_false_for_a_folder_that_is_not_readable() {
        let data_path = Path::new(r"C:\Program Files");
        let state = state(data_path);

        let relative_path = "WindowsApps";

        // The WindowsApps directory is so locked down that trying to read its
        // metadata fails, but its existence can still be observed by iterating
        // over its parent directory's entries.
        let entry_exists = state
            .data_path
            .read_dir()
            .unwrap()
            .flat_map(|res| res.map(|e| e.file_name()).into_iter())
            .any(|name| name == relative_path);

        assert!(entry_exists);

        let function = Function::Readable(PathBuf::from(relative_path));

        assert!(!function.eval(&state).unwrap());
    }

    #[cfg(not(windows))]
    #[test]
    fn function_readable_eval_should_be_false_for_a_folder_that_is_not_readable() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        let relative_path = "unreadable";
        let folder_path = state.data_path.join(relative_path);

        create_dir_all(&folder_path).unwrap();
        make_path_unreadable(&folder_path);

        assert!(folder_path.exists());

        let function = Function::Readable(PathBuf::from(relative_path));

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_executable_should_be_false_for_a_path_that_does_not_exist() {
        let state = state(".");
        let function = Function::IsExecutable("missing".into());

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_executable_should_be_false_for_a_directory() {
        let state = state(".");
        let function = Function::IsExecutable("tests".into());

        assert!(!function.eval(&state).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn function_is_executable_should_be_false_for_a_file_that_cannot_be_read() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        let relative_path = "unreadable";
        let file_path = state.data_path.join(relative_path);

        // Create a file and open it with exclusive access so that the readable
        // function eval isn't able to open the file in read-only mode.
        let _file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(&file_path);

        assert!(file_path.exists());

        let function = Function::IsExecutable(PathBuf::from(relative_path));

        assert!(!function.eval(&state).unwrap());
    }

    #[cfg(not(windows))]
    #[test]
    fn function_is_executable_should_be_false_for_a_file_that_cannot_be_read() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        let relative_path = "unreadable";
        let file_path = state.data_path.join(relative_path);

        std::fs::write(&file_path, "").unwrap();
        make_path_unreadable(&file_path);

        assert!(file_path.exists());

        let function = Function::IsExecutable(PathBuf::from(relative_path));

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_executable_should_be_false_for_a_file_that_is_not_an_executable() {
        let state = state(".");
        let function = Function::IsExecutable("Cargo.toml".into());

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_executable_should_be_true_for_a_file_that_is_an_executable() {
        let state = state(".");
        let function = Function::IsExecutable("tests/libloot_win32/loot.dll".into());

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_active_path_eval_should_be_true_if_the_path_is_an_active_plugin() {
        let function = Function::ActivePath(PathBuf::from("Blank.esp"));
        let state = state_with_active_plugins(".", &["Blank.esp"]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_active_path_eval_should_be_case_insensitive() {
        let function = Function::ActivePath(PathBuf::from("Blank.esp"));
        let state = state_with_active_plugins(".", &["blank.esp"]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_active_path_eval_should_be_false_if_the_path_is_not_an_active_plugin() {
        let function = Function::ActivePath(PathBuf::from("inactive.esp"));
        let state = state_with_active_plugins(".", &["Blank.esp"]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_active_regex_eval_should_be_true_if_the_regex_matches_an_active_plugin() {
        let function = Function::ActiveRegex(regex("Blank\\.esp"));
        let state = state_with_active_plugins(".", &["Blank.esp"]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_active_regex_eval_should_be_false_if_the_regex_does_not_match_an_active_plugin() {
        let function = Function::ActiveRegex(regex("inactive\\.esp"));
        let state = state_with_active_plugins(".", &["Blank.esp"]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_master_eval_should_be_true_if_the_path_is_a_master_plugin() {
        let function = Function::IsMaster(PathBuf::from("Blank.esm"));
        let state = state("tests/testing-plugins/Oblivion/Data");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_master_eval_should_be_false_if_the_path_is_an_openmw_master_flagged_plugin() {
        let function = Function::IsMaster(PathBuf::from("Blank.esm"));
        let mut state = state("tests/testing-plugins/Morrowind/Data Files");
        state.game_type = GameType::OpenMW;

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_master_eval_should_be_false_if_the_path_does_not_exist() {
        let function = Function::IsMaster(PathBuf::from("missing.esp"));
        let state = state("tests/testing-plugins/Oblivion/Data");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_master_eval_should_be_false_if_the_path_is_not_a_plugin() {
        let function = Function::IsMaster(PathBuf::from("Cargo.toml"));
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_is_master_eval_should_be_false_if_the_path_is_a_non_master_plugin() {
        let function = Function::IsMaster(PathBuf::from("Blank.esp"));
        let state = state("tests/testing-plugins/Oblivion/Data");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_eval_should_be_false_if_no_directory_entries_match() {
        let function = Function::Many(PathBuf::from("."), regex("missing"));
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_eval_should_be_false_if_the_parent_path_part_is_not_a_directory() {
        let function = Function::Many(PathBuf::from("missing"), regex("Cargo.*"));
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_eval_should_be_false_if_one_directory_entry_matches() {
        let function = Function::Many(
            PathBuf::from("tests/testing-plugins/Oblivion/Data"),
            regex("Blank\\.esp"),
        );
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_eval_should_be_true_if_more_than_one_directory_entry_matches() {
        let function = Function::Many(
            PathBuf::from("tests/testing-plugins/Oblivion/Data"),
            regex("Blank.*"),
        );
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_eval_should_trim_ghost_plugin_extension_before_matching_against_regex() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esm"),
            state.data_path.join("Blank.esm.ghost"),
        )
        .unwrap();
        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esp"),
            state.data_path.join("Blank.esp.ghost"),
        )
        .unwrap();

        let function = Function::Many(PathBuf::from("."), regex("^Blank\\.es(m|p)$"));

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_eval_should_check_across_all_configured_data_paths() {
        let function = Function::Many(PathBuf::from("Data"), regex("Blank\\.esp"));
        let state = state_with_data(
            "./tests/testing-plugins/Skyrim",
            vec!["./tests/testing-plugins/Oblivion"],
            &[],
            &[],
        );

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_active_eval_should_be_true_if_the_regex_matches_more_than_one_active_plugin() {
        let function = Function::ManyActive(regex("Blank.*"));
        let state = state_with_active_plugins(".", &["Blank.esp", "Blank.esm"]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_active_eval_should_be_false_if_one_active_plugin_matches() {
        let function = Function::ManyActive(regex("Blank\\.esp"));
        let state = state_with_active_plugins(".", &["Blank.esp", "Blank.esm"]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_many_active_eval_should_be_false_if_the_regex_does_not_match_an_active_plugin() {
        let function = Function::ManyActive(regex("inactive\\.esp"));
        let state = state_with_active_plugins(".", &["Blank.esp", "Blank.esm"]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_checksum_eval_should_be_false_if_the_file_does_not_exist() {
        let function = Function::Checksum(PathBuf::from("missing"), 0x374E_2A6F);
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_checksum_eval_should_be_false_if_the_file_checksum_does_not_equal_the_given_checksum(
    ) {
        let function = Function::Checksum(
            PathBuf::from("tests/testing-plugins/Oblivion/Data/Blank.esm"),
            0xDEAD_BEEF,
        );
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_checksum_eval_should_be_true_if_the_file_checksum_equals_the_given_checksum() {
        let function = Function::Checksum(
            PathBuf::from("tests/testing-plugins/Oblivion/Data/Blank.esm"),
            0x374E_2A6F,
        );
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_checksum_eval_should_support_checking_the_crc_of_a_ghosted_plugin() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esm"),
            state.data_path.join("Blank.esm.ghost"),
        )
        .unwrap();

        let function = Function::Checksum(PathBuf::from("Blank.esm"), 0x374E_2A6F);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_checksum_eval_should_not_check_for_ghosted_non_plugin_file() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.bsa"),
            state.data_path.join("Blank.bsa.ghost"),
        )
        .unwrap();

        let function = Function::Checksum(PathBuf::from("Blank.bsa"), 0x22AB_79D9);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_checksum_eval_should_be_false_if_given_a_directory_path() {
        // The given CRC is the CRC-32 of the directory as calculated by 7-zip.
        let function = Function::Checksum(PathBuf::from("tests/testing-plugins"), 0xC9CD_16C3);
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_checksum_eval_should_cache_and_use_cached_crcs() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esm"),
            state.data_path.join("Blank.esm"),
        )
        .unwrap();

        let function = Function::Checksum(PathBuf::from("Blank.esm"), 0x374E_2A6F);

        assert!(function.eval(&state).unwrap());

        // Change the CRC of the file to test that the cached value is used.
        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.bsa"),
            state.data_path.join("Blank.esm"),
        )
        .unwrap();

        let function = Function::Checksum(PathBuf::from("Blank.esm"), 0x374E_2A6F);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_eval_should_cache_results_and_use_cached_results() {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(Path::new("Cargo.toml"), state.data_path.join("Cargo.toml")).unwrap();

        let function = Function::FilePath(PathBuf::from("Cargo.toml"));

        assert!(function.eval(&state).unwrap());

        remove_file(state.data_path.join("Cargo.toml")).unwrap();

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_the_path_does_not_exist_and_comparator_is_ne() {
        let function =
            Function::Version("missing".into(), "1.0".into(), ComparisonOperator::NotEqual);
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_the_path_does_not_exist_and_comparator_is_lt() {
        let function =
            Function::Version("missing".into(), "1.0".into(), ComparisonOperator::LessThan);
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_the_path_does_not_exist_and_comparator_is_lteq() {
        let function = Function::Version(
            "missing".into(),
            "1.0".into(),
            ComparisonOperator::LessThanOrEqual,
        );
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_the_path_does_not_exist_and_comparator_is_eq() {
        let function = Function::Version("missing".into(), "1.0".into(), ComparisonOperator::Equal);
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_the_path_does_not_exist_and_comparator_is_gt() {
        let function = Function::Version(
            "missing".into(),
            "1.0".into(),
            ComparisonOperator::GreaterThan,
        );
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_the_path_does_not_exist_and_comparator_is_gteq() {
        let function = Function::Version(
            "missing".into(),
            "1.0".into(),
            ComparisonOperator::GreaterThanOrEqual,
        );
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_the_path_is_not_a_file_and_comparator_is_ne() {
        let function =
            Function::Version("tests".into(), "1.0".into(), ComparisonOperator::NotEqual);
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_the_path_is_not_a_file_and_comparator_is_lt() {
        let function =
            Function::Version("tests".into(), "1.0".into(), ComparisonOperator::LessThan);
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_the_path_is_not_a_file_and_comparator_is_lteq() {
        let function = Function::Version(
            "tests".into(),
            "1.0".into(),
            ComparisonOperator::LessThanOrEqual,
        );
        let state = state(".");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_the_path_is_not_a_file_and_comparator_is_eq() {
        let function = Function::Version("tests".into(), "1.0".into(), ComparisonOperator::Equal);
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_the_path_is_not_a_file_and_comparator_is_gt() {
        let function = Function::Version(
            "tests".into(),
            "1.0".into(),
            ComparisonOperator::GreaterThan,
        );
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_the_path_is_not_a_file_and_comparator_is_gteq() {
        let function = Function::Version(
            "tests".into(),
            "1.0".into(),
            ComparisonOperator::GreaterThanOrEqual,
        );
        let state = state(".");

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_treat_a_plugin_with_no_cached_version_as_if_it_did_not_exist() {
        use self::ComparisonOperator::*;

        let plugin = PathBuf::from("Blank.esm");
        let version = String::from("1.0");
        let state = state("tests/testing-plugins/Oblivion/Data");

        let function = Function::Version(plugin.clone(), version.clone(), NotEqual);
        assert!(function.eval(&state).unwrap());
        let function = Function::Version(plugin.clone(), version.clone(), LessThan);
        assert!(function.eval(&state).unwrap());
        let function = Function::Version(plugin.clone(), version.clone(), LessThanOrEqual);
        assert!(function.eval(&state).unwrap());
        let function = Function::Version(plugin.clone(), version.clone(), Equal);
        assert!(!function.eval(&state).unwrap());
        let function = Function::Version(plugin.clone(), version.clone(), GreaterThan);
        assert!(!function.eval(&state).unwrap());
        let function = Function::Version(plugin.clone(), version.clone(), GreaterThanOrEqual);
        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_versions_are_not_equal_and_comparator_is_eq() {
        let function = Function::Version("Blank.esm".into(), "5".into(), ComparisonOperator::Equal);
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "1")]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_versions_are_equal_and_comparator_is_eq() {
        let function = Function::Version("Blank.esm".into(), "5".into(), ComparisonOperator::Equal);
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "5")]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_versions_are_equal_and_comparator_is_ne() {
        let function =
            Function::Version("Blank.esm".into(), "5".into(), ComparisonOperator::NotEqual);
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "5")]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_versions_are_not_equal_and_comparator_is_ne() {
        let function =
            Function::Version("Blank.esm".into(), "5".into(), ComparisonOperator::NotEqual);
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "1")]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_actual_version_is_eq_and_comparator_is_lt() {
        let function =
            Function::Version("Blank.esm".into(), "5".into(), ComparisonOperator::LessThan);
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "5")]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_actual_version_is_gt_and_comparator_is_lt() {
        let function =
            Function::Version("Blank.esm".into(), "5".into(), ComparisonOperator::LessThan);
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "6")]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_actual_version_is_lt_and_comparator_is_lt() {
        let function =
            Function::Version("Blank.esm".into(), "5".into(), ComparisonOperator::NotEqual);
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "1")]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_actual_version_is_eq_and_comparator_is_gt() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::GreaterThan,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "5")]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_actual_version_is_lt_and_comparator_is_gt() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::GreaterThan,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "4")]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_actual_version_is_gt_and_comparator_is_gt() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::GreaterThan,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "6")]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_actual_version_is_gt_and_comparator_is_lteq() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::LessThanOrEqual,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "6")]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_actual_version_is_eq_and_comparator_is_lteq() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::LessThanOrEqual,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "5")]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_actual_version_is_lt_and_comparator_is_lteq() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::LessThanOrEqual,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "4")]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_false_if_actual_version_is_lt_and_comparator_is_gteq() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::GreaterThanOrEqual,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "4")]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_actual_version_is_eq_and_comparator_is_gteq() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::GreaterThanOrEqual,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "5")]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_be_true_if_actual_version_is_gt_and_comparator_is_gteq() {
        let function = Function::Version(
            "Blank.esm".into(),
            "5".into(),
            ComparisonOperator::GreaterThanOrEqual,
        );
        let state =
            state_with_versions("tests/testing-plugins/Oblivion/Data", &[("Blank.esm", "6")]);

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_version_eval_should_read_executable_file_version() {
        let function = Function::Version(
            "loot.dll".into(),
            "0.18.2.0".into(),
            ComparisonOperator::Equal,
        );
        let state = state("tests/libloot_win32");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_product_version_eval_should_read_executable_product_version() {
        let function = Function::ProductVersion(
            "loot.dll".into(),
            "0.18.2".into(),
            ComparisonOperator::Equal,
        );
        let state = state("tests/libloot_win32");

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn get_product_version_should_return_ok_none_if_the_path_does_not_exist() {
        assert!(get_product_version(Path::new("missing")).unwrap().is_none());
    }

    #[test]
    fn get_product_version_should_return_ok_none_if_the_path_is_not_a_file() {
        assert!(get_product_version(Path::new("tests")).unwrap().is_none());
    }

    #[test]
    fn get_product_version_should_return_ok_some_if_the_path_is_an_executable() {
        let version = get_product_version(Path::new("tests/libloot_win32/loot.dll"))
            .unwrap()
            .unwrap();

        assert_eq!(Version::from("0.18.2"), version);
    }

    #[test]
    fn get_product_version_should_error_if_the_path_is_not_an_executable() {
        assert!(get_product_version(Path::new("Cargo.toml")).is_err());
    }

    #[test]
    fn function_filename_version_eval_should_be_false_if_no_matching_filenames_exist() {
        let state = state_with_versions("tests/testing-plugins/Oblivion/Data", &[]);

        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank (A+).esm"),
            "5".into(),
            ComparisonOperator::Equal,
        );

        assert!(!function.eval(&state).unwrap());

        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank (A+).esm"),
            "5".into(),
            ComparisonOperator::NotEqual,
        );

        assert!(!function.eval(&state).unwrap());

        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank (A+).esm"),
            "5".into(),
            ComparisonOperator::LessThan,
        );

        assert!(!function.eval(&state).unwrap());

        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank (A+).esm"),
            "5".into(),
            ComparisonOperator::GreaterThan,
        );

        assert!(!function.eval(&state).unwrap());

        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank (A+).esm"),
            "5".into(),
            ComparisonOperator::LessThanOrEqual,
        );

        assert!(!function.eval(&state).unwrap());

        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank (A+).esm"),
            "5".into(),
            ComparisonOperator::GreaterThanOrEqual,
        );

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_filename_version_eval_should_be_false_if_filenames_matched_but_no_version_was_captured(
    ) {
        // This shouldn't happen in practice because parsing validates that there is one explicit capturing group in the regex.
        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank .+.esm"),
            "5".into(),
            ComparisonOperator::GreaterThanOrEqual,
        );
        let state = state_with_versions("tests/testing-plugins/Oblivion/Data", &[]);

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_filename_version_eval_should_be_true_if_the_captured_version_is_eq_and_operator_is_eq(
    ) {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esm"),
            state.data_path.join("Blank 5.esm"),
        )
        .unwrap();

        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank (\\d).esm"),
            "5".into(),
            ComparisonOperator::Equal,
        );

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_filename_version_eval_should_be_true_if_the_captured_version_is_not_equal_and_operator_is_not_equal(
    ) {
        let tmp_dir = tempdir().unwrap();
        let data_path = tmp_dir.path().join("Data");
        let state = state(data_path);

        copy(
            Path::new("tests/testing-plugins/Oblivion/Data/Blank.esm"),
            state.data_path.join("Blank 4.esm"),
        )
        .unwrap();

        let function = Function::FilenameVersion(
            "".into(),
            regex("Blank (\\d).esm"),
            "5".into(),
            ComparisonOperator::NotEqual,
        );

        assert!(function.eval(&state).unwrap());
    }

    #[test]
    fn function_description_contains_eval_should_return_false_if_the_file_does_not_exist() {
        let state = state_with_versions("tests/testing-plugins/Oblivion/Data", &[]);

        let function =
            Function::DescriptionContains("missing.esp".into(), regex(LOWERCASE_NON_ASCII));

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_description_contains_eval_should_return_false_if_the_file_is_not_a_plugin() {
        let state = state_with_versions("tests/testing-plugins/Oblivion/Data", &[]);

        let function =
            Function::DescriptionContains("Blank.bsa".into(), regex(LOWERCASE_NON_ASCII));

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_description_contains_eval_should_return_false_if_the_plugin_has_no_description() {
        let state = state_with_versions("tests/testing-plugins/Oblivion/Data", &[]);

        let function = Function::DescriptionContains(
            "Blank - Different.esm".into(),
            regex(LOWERCASE_NON_ASCII),
        );

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_description_contains_eval_should_return_false_if_the_plugin_description_does_not_match(
    ) {
        let state = state_with_versions("tests/testing-plugins/Oblivion/Data", &[]);

        let function =
            Function::DescriptionContains("Blank.esm".into(), regex(LOWERCASE_NON_ASCII));

        assert!(!function.eval(&state).unwrap());
    }

    #[test]
    fn function_description_contains_eval_should_return_true_if_the_plugin_description_contains_a_match(
    ) {
        let state = state_with_versions("tests/testing-plugins/Oblivion/Data", &[]);

        let function =
            Function::DescriptionContains("Blank.esp".into(), regex(LOWERCASE_NON_ASCII));

        assert!(function.eval(&state).unwrap());
    }
}

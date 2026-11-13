//! The language servers uals knows: which files each one answers for, how to find the project it
//! loads, and how to start it.

use std::path::{Path, PathBuf};

/// Placeholder in a server command for its per-project state folder.
pub const STATE_DIR: &str = "{state}";

/// How readiness of a server is seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Ready once no `$/progress` is active and none started for a short settle time.
    Progress {
        /// The server always reports its project load as progress, so it is not ready before the
        /// first progress or a grace time.
        reports_loading: bool,
    },
    /// rust-analyzer: ready when `experimental/serverStatus` says `quiescent`.
    QuiescentStatus,
}

/// One language server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// Name in config, logs and answers.
    pub name: &'static str,
    /// File extensions the server answers for, lower case, without the dot.
    pub extensions: &'static [&'static str],
    /// Marker groups that find the project root, most specific kind first. The root is the
    /// nearest folder with a marker of the first group that has one; the repository otherwise.
    pub root_markers: &'static [&'static [&'static str]],
    /// Commands to try in order; the first whose program is installed is used. [`STATE_DIR`] in
    /// an argument becomes the state folder of the project.
    pub commands: &'static [&'static [&'static str]],
    pub readiness: Readiness,
    /// Log or message text that means the server could not load the project.
    pub load_failures: &'static [&'static str],
    /// The server needs a JDK: `JAVA_HOME` is passed to it.
    pub needs_java: bool,
    /// An empty answer only means the server has no index yet, not that nothing refers to the
    /// definition.
    pub empty_is_unknown: bool,
    /// Started without `enabled = true` in the config.
    pub on_by_default: bool,
    /// JSON of `initializationOptions`; empty for none.
    pub initialization_options: &'static str,
}

/// The jdtls launcher argument that keeps Eclipse metadata out of the project folders.
pub const JDTLS_NO_PROJECT_FILES: &str = "--jvm-arg=-Djava.import.generatesMetadataFilesAtProjectRoot=false";

const GRADLE_SETTINGS: &[&str] = &["settings.gradle.kts", "settings.gradle"];
const JVM_BUILD: &[&str] = &["build.gradle.kts", "build.gradle", "pom.xml"];

/// Every server uals can start, in no particular order.
pub const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        name: "kotlin-lsp",
        extensions: &["kt", "kts"],
        root_markers: &[GRADLE_SETTINGS, JVM_BUILD],
        commands: &[&["kotlin-lsp", "--stdio", "--data-sharing=none", "--system-path={state}"]],
        readiness: Readiness::Progress { reports_loading: true },
        load_failures: &["Unable to import"],
        needs_java: true,
        empty_is_unknown: false,
        on_by_default: false,
        initialization_options: "",
    },
    ServerSpec {
        name: "jdtls",
        extensions: &["java"],
        root_markers: &[GRADLE_SETTINGS, JVM_BUILD],
        // jdtls reads `generatesMetadataFilesAtProjectRoot` only as a JVM system property: with
        // it, `.project`, `.classpath` and `.settings` go to the state folder, not the project.
        commands: &[&["jdtls", JDTLS_NO_PROJECT_FILES, "-data", "{state}"]],
        readiness: Readiness::Progress { reports_loading: true },
        // "Synchronize project X failed due to an error configuring Eclipse / connecting to the
        // Gradle build / while importing the root project".
        load_failures: &["failed due to an error", "Failed to import"],
        needs_java: true,
        empty_is_unknown: false,
        on_by_default: false,
        // No build: nothing writes classes into `bin/` of the project.
        initialization_options: r#"{"settings": {"java": {"autobuild": {"enabled": false}}}}"#,
    },
    ServerSpec {
        name: "gopls",
        extensions: &["go"],
        root_markers: &[&["go.work"], &["go.mod"]],
        commands: &[&["gopls"]],
        readiness: Readiness::Progress { reports_loading: false },
        load_failures: &[],
        needs_java: false,
        empty_is_unknown: false,
        on_by_default: true,
        initialization_options: "",
    },
    ServerSpec {
        name: "rust-analyzer",
        extensions: &["rs"],
        root_markers: &[&["Cargo.toml"]],
        commands: &[&["rust-analyzer"]],
        readiness: Readiness::QuiescentStatus,
        load_failures: &["Failed to load workspaces", "failed to discover workspace"],
        needs_java: false,
        empty_is_unknown: false,
        on_by_default: true,
        initialization_options: "",
    },
    ServerSpec {
        name: "typescript-language-server",
        extensions: &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
        root_markers: &[&["tsconfig.json", "jsconfig.json"], &["package.json"]],
        commands: &[&["typescript-language-server", "--stdio"]],
        // Loads a project when one of its files is opened and reports that as progress.
        readiness: Readiness::Progress { reports_loading: true },
        load_failures: &[],
        needs_java: false,
        empty_is_unknown: false,
        on_by_default: true,
        initialization_options: "",
    },
    ServerSpec {
        name: "pyright",
        extensions: &["py", "pyi"],
        root_markers: &[&["pyrightconfig.json", "pyproject.toml", "setup.py", "setup.cfg"]],
        commands: &[&["basedpyright-langserver", "--stdio"], &["pyright-langserver", "--stdio"]],
        readiness: Readiness::Progress { reports_loading: false },
        load_failures: &[],
        needs_java: false,
        empty_is_unknown: false,
        on_by_default: true,
        initialization_options: "",
    },
    ServerSpec {
        name: "clangd",
        extensions: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "m", "mm"],
        root_markers: &[&["compile_commands.json", "compile_flags.txt", ".clangd"], &["CMakeLists.txt"]],
        commands: &[&["clangd", "--background-index"]],
        readiness: Readiness::Progress { reports_loading: false },
        load_failures: &[],
        needs_java: false,
        empty_is_unknown: true,
        on_by_default: true,
        initialization_options: "",
    },
    ServerSpec {
        name: "sourcekit-lsp",
        extensions: &["swift"],
        root_markers: &[&["Package.swift"]],
        commands: &[&["sourcekit-lsp"]],
        readiness: Readiness::Progress { reports_loading: false },
        load_failures: &[],
        needs_java: false,
        empty_is_unknown: true,
        on_by_default: true,
        initialization_options: "",
    },
];

/// The server for files with this extension (any case), if any.
pub fn server_for(path: &Path) -> Option<&'static ServerSpec> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    SERVERS.iter().find(|spec| spec.extensions.contains(&extension.as_str()))
}

/// The server named `name`.
pub fn server_named(name: &str) -> Option<&'static ServerSpec> {
    SERVERS.iter().find(|spec| spec.name == name)
}

/// The project root of `file` for `spec`: the nearest folder between the file and `boundary`
/// (both included) holding a marker of the first group that has one; `boundary` when none does,
/// or when `file` is not inside `boundary`.
pub fn project_root(spec: &ServerSpec, file: &Path, boundary: &Path) -> PathBuf {
    let Some(start) = file.parent().filter(|dir| dir.starts_with(boundary)) else {
        return boundary.to_path_buf();
    };
    for group in spec.root_markers {
        let found = start
            .ancestors()
            .take_while(|dir| dir.starts_with(boundary))
            .find(|dir| group.iter().any(|marker| dir.join(marker).exists()));
        if let Some(dir) = found {
            return dir.to_path_buf();
        }
    }
    boundary.to_path_buf()
}

/// The LSP `languageId` of a file.
pub fn language_id(path: &Path) -> &'static str {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match extension.as_str() {
        "kt" | "kts" => "kotlin",
        "java" => "java",
        "go" => "go",
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "c" | "h" => "c",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "swift" => "swift",
        _ => "cpp",
    }
}

/// The column (UTF-16 code units, as LSP counts by default) where `name` first stands as a whole
/// identifier in `line`.
pub fn identifier_column(line: &str, name: &str) -> Option<u32> {
    if name.is_empty() {
        return None;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut from = 0;
    while let Some(offset) = line[from..].find(name) {
        let start = from + offset;
        let end = start + name.len();
        let before_ok = !line[..start].chars().next_back().is_some_and(is_ident);
        let after_ok = !line[end..].chars().next().is_some_and(is_ident);
        if before_ok && after_ok {
            let column: usize = line[..start].chars().map(char::len_utf16).sum();
            return u32::try_from(column).ok();
        }
        from = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_extension_has_one_server() {
        let mut seen = std::collections::BTreeSet::new();
        for spec in SERVERS {
            for extension in spec.extensions {
                assert!(seen.insert(*extension), "{extension} is claimed twice");
            }
        }
    }

    #[test]
    fn jdtls_keeps_its_eclipse_files_out_of_the_project() {
        let jdtls = server_named("jdtls").unwrap();
        assert!(jdtls.commands.iter().all(|command| command.contains(&JDTLS_NO_PROJECT_FILES)));
    }

    #[test]
    fn initialization_options_are_json_objects() {
        for spec in SERVERS.iter().filter(|spec| !spec.initialization_options.is_empty()) {
            let options: serde_json::Value = serde_json::from_str(spec.initialization_options).unwrap();
            assert!(options.is_object(), "{}", spec.name);
        }
    }

    #[test]
    fn servers_are_chosen_by_extension() {
        assert_eq!(server_for(Path::new("a/B.kt")).map(|s| s.name), Some("kotlin-lsp"));
        assert_eq!(server_for(Path::new("a/B.JAVA")).map(|s| s.name), Some("jdtls"));
        assert_eq!(server_for(Path::new("main.go")).map(|s| s.name), Some("gopls"));
        assert_eq!(server_for(Path::new("x.tsx")).map(|s| s.name), Some("typescript-language-server"));
        assert_eq!(server_for(Path::new("README.md")), None);
        assert_eq!(server_for(Path::new("Makefile")), None);
    }

    #[test]
    fn the_root_is_the_nearest_folder_of_the_first_marker_group() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let module = repo.join("app/src/main/kotlin");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(repo.join("settings.gradle.kts"), "").unwrap();
        std::fs::write(repo.join("app/build.gradle.kts"), "").unwrap();
        let go = repo.join("tools/cli");
        std::fs::create_dir_all(&go).unwrap();
        std::fs::write(repo.join("tools/go.mod"), "").unwrap();
        let kotlin = server_named("kotlin-lsp").unwrap();
        let gopls = server_named("gopls").unwrap();

        assert_eq!(project_root(kotlin, &module.join("A.kt"), repo), repo);
        assert_eq!(project_root(gopls, &go.join("main.go"), repo), repo.join("tools"));
        // No marker: the repository itself.
        assert_eq!(project_root(gopls, &module.join("x.go"), repo), repo);
        // A marker above the repository is not used.
        let inner = repo.join("tools");
        assert_eq!(project_root(kotlin, &go.join("A.kt"), &inner), inner);
    }

    #[test]
    fn the_column_is_found_for_a_whole_identifier_only() {
        assert_eq!(identifier_column("    fun loadAll() = load()", "load"), Some(20));
        assert_eq!(identifier_column("class Store {", "Store"), Some(6));
        assert_eq!(identifier_column("val storeX = 1", "store"), None);
        // UTF-16 columns: "ё" is one unit, "😀" two.
        assert_eq!(identifier_column("// ё😀 load", "load"), Some(7));
    }
}

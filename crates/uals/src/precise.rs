//! Precise `usages`: the language servers say which usages refer to which declaration of a name.
//!
//! Usages found by name that a server confirms get the index of their definition. Usages a server
//! returns that the name search missed are added. When the symbol is qualified (`Store.load`) and
//! a server answered for every declaration, a usage by name in a file a server saw whole, that no
//! server returned, refers to another declaration and is dropped. Otherwise such usages stay,
//! unattributed: a declaration without an answer may be the one they refer to.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use uals_core::types::{BackendStatus, Definition, DefinitionReport, Repo, RepoUsages, ResolvedReferences, Usage};

/// No more definitions than this are sent to language servers in one call.
pub const MAX_DEFINITIONS: usize = 12;

/// A symbol split into its container and name: `Store.load` → (`Store`, `load`); `load` → (none,
/// `load`). `::` separates like `.`.
pub fn split_qualifier(symbol: &str) -> (Option<&str>, &str) {
    let symbol = symbol.trim();
    let cut = symbol.rfind("::").map(|at| (at, 2)).into_iter().chain(symbol.rfind('.').map(|at| (at, 1))).max();
    match cut {
        Some((at, len)) if at > 0 && at + len < symbol.len() => {
            let container = &symbol[..at];
            let container = container.rsplit(['.', ':']).next().unwrap_or(container);
            (Some(container).filter(|c| !c.is_empty()), &symbol[at + len..])
        }
        _ => (None, symbol),
    }
}

/// Merges the servers' answers into the usages found by name and reports every definition.
/// `resolved[i]` answers `definitions[i]`. `found` is one entry per repository in `repos` order.
pub fn merge(
    repos: &[Repo],
    found: &mut [&mut RepoUsages],
    definitions: Vec<Definition>,
    resolved: Vec<Result<ResolvedReferences, BackendStatus>>,
    qualified: bool,
) -> Vec<DefinitionReport> {
    let mut owner: HashMap<(PathBuf, u32), usize> = HashMap::new();
    for (index, answer) in resolved.iter().enumerate() {
        if let Ok(answer) = answer {
            for reference in &answer.references {
                owner.entry((reference.path.clone(), reference.line)).or_insert(index);
            }
        }
    }
    let covered: Vec<&ResolvedReferences> = resolved.iter().filter_map(|answer| answer.as_ref().ok()).collect();
    let drop_unconfirmed = qualified && covered.len() == resolved.len();

    let mut seen: HashSet<(PathBuf, u32)> = HashSet::new();
    for repo in found.iter_mut() {
        for usage in &mut repo.usages {
            let key = (usage.path.clone(), usage.line);
            usage.definition = owner.get(&key).copied();
            seen.insert(key);
        }
        if drop_unconfirmed {
            repo.usages
                .retain(|usage| usage.definition.is_some() || !covered.iter().any(|answer| answer.covers(&usage.path)));
        }
    }

    let mut added: BTreeMap<usize, Vec<Usage>> = BTreeMap::new();
    for ((path, line), index) in &owner {
        if seen.contains(&(path.clone(), *line)) {
            continue;
        }
        let Some(slot) = repos.iter().position(|repo| path.starts_with(&repo.root)) else { continue };
        added.entry(slot).or_default().push(Usage {
            repo: repos[slot].name.clone(),
            path: path.clone(),
            line: *line,
            context: None,
            definition: Some(*index),
        });
    }
    for (slot, mut usages) in added {
        fill_contexts(&mut usages);
        if let Some(repo) = found.get_mut(slot) {
            repo.usages.append(&mut usages);
            repo.usages.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        }
    }

    let mut counts = vec![0; definitions.len()];
    for usage in found.iter().flat_map(|repo| &repo.usages) {
        if let Some(count) = usage.definition.and_then(|index| counts.get_mut(index)) {
            *count += 1;
        }
    }
    definitions
        .into_iter()
        .zip(resolved)
        .zip(counts)
        .map(|((definition, answer), usages)| match answer {
            Ok(answer) => {
                DefinitionReport { definition, server: Some(answer.server), status: BackendStatus::Ok, usages }
            }
            Err(status) => DefinitionReport { definition, server: None, status, usages: 0 },
        })
        .collect()
}

/// Reads the source line of each usage, one read per file.
fn fill_contexts(usages: &mut [Usage]) {
    let mut files: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    for usage in usages {
        let lines = files.entry(usage.path.clone()).or_insert_with(|| read_lines(&usage.path));
        usage.context = lines
            .as_ref()
            .and_then(|lines| lines.get(usage.line.saturating_sub(1) as usize))
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty());
    }
}

fn read_lines(path: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).lines().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use uals_core::types::{BackendId, Reference, SymbolKind};

    fn reference(path: &str, line: u32) -> Reference {
        Reference { path: PathBuf::from(path), line }
    }

    fn repo(name: &str, root: &str) -> Repo {
        Repo { name: name.into(), root: PathBuf::from(root) }
    }

    fn usage(repo: &str, path: &str, line: u32) -> Usage {
        Usage { repo: repo.into(), path: PathBuf::from(path), line, context: Some("x".into()), definition: None }
    }

    fn found(repo: &str, usages: Vec<Usage>) -> RepoUsages {
        RepoUsages { repo: repo.into(), source: BackendId::AstIndex, status: BackendStatus::Ok, fallback: None, usages }
    }

    fn definition(path: &str, line: u32, container: &str) -> Definition {
        Definition {
            repo: "app".into(),
            path: PathBuf::from(path),
            line,
            name: "load".into(),
            kind: SymbolKind::Function,
            container: Some(container.into()),
        }
    }

    fn resolved(root: &str, references: Vec<Reference>) -> ResolvedReferences {
        ResolvedReferences {
            server: "kotlin-lsp".into(),
            root: PathBuf::from(root),
            extensions: vec!["kt".into()],
            references,
        }
    }

    /// `merge` over owned answers, as the tests hold them.
    fn merge_all(
        repos: &[Repo],
        found: &mut [RepoUsages],
        definitions: Vec<Definition>,
        resolved: Vec<Result<ResolvedReferences, BackendStatus>>,
        qualified: bool,
    ) -> Vec<DefinitionReport> {
        let mut found: Vec<&mut RepoUsages> = found.iter_mut().collect();
        merge(repos, &mut found, definitions, resolved, qualified)
    }

    fn lines(found: &[RepoUsages]) -> Vec<(String, u32, Option<usize>)> {
        found
            .iter()
            .flat_map(|repo| &repo.usages)
            .map(|usage| (usage.path.display().to_string(), usage.line, usage.definition))
            .collect()
    }

    #[test]
    fn qualifiers_are_split_at_the_last_separator() {
        assert_eq!(split_qualifier("load"), (None, "load"));
        assert_eq!(split_qualifier("Store.load"), (Some("Store"), "load"));
        assert_eq!(split_qualifier("a.b.Store.load"), (Some("Store"), "load"));
        assert_eq!(split_qualifier("crate::store::Store::load"), (Some("Store"), "load"));
        assert_eq!(split_qualifier(".load"), (None, ".load"));
        assert_eq!(split_qualifier("Store."), (None, "Store."));
    }

    #[test]
    fn usages_are_attributed_and_missed_ones_added() {
        let repos = [repo("app", "/w/app")];
        let mut found = [found("app", vec![usage("app", "/w/app/A.kt", 3), usage("app", "/w/app/B.kt", 7)])];
        let definitions = vec![definition("/w/app/Store.kt", 2, "Store"), definition("/w/app/Other.kt", 4, "Other")];
        let answers = vec![
            Ok(resolved("/w/app", vec![reference("/w/app/A.kt", 3), reference("/w/app/C.kt", 1)])),
            Err(BackendStatus::Unavailable { reason: "loading".into() }),
        ];

        let reports = merge_all(&repos, &mut found, definitions, answers, false);

        assert_eq!(
            lines(&found),
            vec![
                ("/w/app/A.kt".into(), 3, Some(0)),
                ("/w/app/B.kt".into(), 7, None),
                ("/w/app/C.kt".into(), 1, Some(0)),
            ]
        );
        assert_eq!(reports[0].usages, 2);
        assert_eq!(reports[0].server.as_deref(), Some("kotlin-lsp"));
        assert_eq!(reports[1].status, BackendStatus::Unavailable { reason: "loading".into() });
    }

    #[test]
    fn a_qualified_symbol_drops_unconfirmed_usages_only_where_the_server_saw_the_file() {
        let repos = [repo("app", "/w/app"), repo("lib", "/w/lib")];
        let mut found = [
            found(
                "app",
                vec![usage("app", "/w/app/A.kt", 3), usage("app", "/w/app/B.kt", 7), usage("app", "/w/app/J.java", 9)],
            ),
            found("lib", vec![usage("lib", "/w/lib/L.kt", 5)]),
        ];
        let definitions = vec![definition("/w/app/Store.kt", 2, "Store")];
        let answers = vec![Ok(resolved("/w/app", vec![reference("/w/app/A.kt", 3), reference("/elsewhere/X.kt", 1)]))];

        merge_all(&repos, &mut found, definitions, answers, true);

        // B.kt:7 is in the loaded project and its language: another `load`. J.java is not a Kotlin
        // file and L.kt is outside the project, so they stay. /elsewhere is not a requested repo.
        assert_eq!(
            lines(&found),
            vec![
                ("/w/app/A.kt".into(), 3, Some(0)),
                ("/w/app/J.java".into(), 9, None),
                ("/w/lib/L.kt".into(), 5, None),
            ]
        );
    }

    #[test]
    fn a_qualified_symbol_keeps_unconfirmed_usages_when_a_declaration_got_no_answer() {
        let repos = [repo("app", "/w/app")];
        let mut found = [found("app", vec![usage("app", "/w/app/A.kt", 3), usage("app", "/w/app/B.kt", 7)])];
        // Two overloads of `Store.load`; the server answered for the first only.
        let definitions = vec![definition("/w/app/Store.kt", 2, "Store"), definition("/w/app/Store.kt", 5, "Store")];
        let answers = vec![Ok(resolved("/w/app", vec![reference("/w/app/A.kt", 3)])), Err(BackendStatus::Timeout)];

        merge_all(&repos, &mut found, definitions, answers, true);

        // B.kt:7 may call the second overload, so it stays.
        assert_eq!(lines(&found), vec![("/w/app/A.kt".into(), 3, Some(0)), ("/w/app/B.kt".into(), 7, None)]);
    }

    #[test]
    fn added_usages_get_their_source_line() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::write(root.join("C.kt"), "package a\n\n    store.load()\n").unwrap();
        let repos = [Repo { name: "app".into(), root: root.clone() }];
        let mut found = [found("app", Vec::new())];
        let path = root.join("C.kt");
        let answers = vec![Ok(ResolvedReferences {
            server: "kotlin-lsp".into(),
            root: root.clone(),
            extensions: vec!["kt".into()],
            references: vec![Reference { path: path.clone(), line: 3 }],
        })];

        merge_all(&repos, &mut found, vec![definition("/x/Store.kt", 1, "Store")], answers, false);

        assert_eq!(found[0].usages[0].context.as_deref(), Some("store.load()"));
    }
}

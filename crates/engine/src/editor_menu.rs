//! The editor's right-click menu as data: which commands to offer for a file's language. The UI
//! renders the groups (it never decides what a Rust or Python file gets), so agents and tests can
//! read the same list.

use codecraft_syntax::Language;

/// A titled group of command ids (a submenu in the UI; `title` empty = top level).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuGroup {
    pub title: &'static str,
    pub ids: Vec<&'static str>,
}

fn group(title: &'static str, ids: &[&'static str]) -> MenuGroup {
    MenuGroup { title, ids: ids.to_vec() }
}

/// The Git submenu, like PyCharm's: file-level actions first, then repository-level ones.
pub fn git_group() -> MenuGroup {
    group(
        "Git",
        &[
            "git.currentFile.showDiff",
            "git.currentFile.compareWithRevision",
            "git.currentFile.compareWithBranch",
            "git.currentFile.annotate",
            "git.currentFile.showHistory",
            "git.currentFile.rollback",
            "git.commit",
            "git.push",
            "git.pull",
            "git.fetch",
            "git.branches",
            "git.uncommittedChanges.stashChanges",
            "git.showGitLog",
        ],
    )
}

/// The submenus below Cut/Copy/Paste for a file of `language`: generic code actions, the Git
/// menu, and the extras that make sense for that language.
pub fn editor_groups(language: Language) -> Vec<MenuGroup> {
    let mut groups = vec![
        group("Refactor", &["refactor.rename", "refactor.extract.variable", "refactor.extract.function"]),
        group("Go To", &["navigate.declarationOrUsages", "edit.findUsages.findUsages"]),
        group(
            "Code",
            &["code.reformatCode", "code.commentWithLineComment", "code.surroundWith", "code.generate"],
        ),
    ];
    let imports = group("Language", &["code.optimizeImports"]);
    match language {
        Language::Rust => groups.push(group(
            "Rust",
            &["code.optimizeImports", "rust.showRecursiveMacroExpansion", "rust.repl.sendSelection"],
        )),
        Language::Python => {
            groups.push(group("Python", &["code.optimizeImports", "python.console.executeSelection", "python.runDoctest"]))
        }
        Language::TypeScript | Language::JavaScript => groups.push(imports),
        Language::Sql => groups.push(group("SQL", &["sql.chooseDataSource"])),
        _ => {}
    }
    groups.push(git_group());
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::registry;

    #[test]
    fn every_offered_id_is_registered() {
        for lang in [
            Language::Rust,
            Language::Python,
            Language::Sql,
            Language::TypeScript,
            Language::JavaScript,
            Language::PlainText,
        ] {
            for g in editor_groups(lang) {
                for id in g.ids {
                    assert!(registry().contains(id), "{id} is not a registered command");
                }
            }
        }
    }

    #[test]
    fn languages_get_their_own_group() {
        let titles = |l| editor_groups(l).iter().map(|g| g.title).collect::<Vec<_>>();
        assert!(titles(Language::Rust).contains(&"Rust"));
        assert!(titles(Language::Python).contains(&"Python"));
        assert!(!titles(Language::PlainText).contains(&"Rust"));
        assert!(titles(Language::PlainText).contains(&"Git"));
    }
}

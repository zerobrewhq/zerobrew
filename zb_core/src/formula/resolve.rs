use crate::{Error, Formula};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub fn resolve_closure(
    roots: &[String],
    formulas: &BTreeMap<String, Formula>,
) -> Result<Vec<String>, Error> {
    let name_to_idx: HashMap<&str, usize> = formulas
        .keys()
        .enumerate()
        .map(|(i, k)| (k.as_str(), i))
        .collect();
    let idx_to_name: Vec<&str> = formulas.keys().map(|k| k.as_str()).collect();
    let n = idx_to_name.len();

    let closure = compute_closure(roots, formulas, &name_to_idx)?;

    let mut indegree = vec![0u32; n];
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];

    for &idx in &closure {
        let formula = &formulas[idx_to_name[idx]];
        let mut dep_indices: Vec<usize> = formula
            .runtime_dependencies()
            .iter()
            .filter_map(|dep| {
                let &di = name_to_idx.get(dep.as_str())?;
                closure.contains(&di).then_some(di)
            })
            .collect();
        dep_indices.sort_unstable();
        for di in dep_indices {
            indegree[idx] += 1;
            adjacency[di].push(idx);
        }
    }

    let mut ready: BTreeSet<usize> = closure
        .iter()
        .copied()
        .filter(|&i| indegree[i] == 0)
        .collect();

    let mut ordered = Vec::with_capacity(closure.len());
    while let Some(&idx) = ready.iter().next() {
        ready.remove(&idx);
        ordered.push(idx);
        for &child in &adjacency[idx] {
            indegree[child] -= 1;
            if indegree[child] == 0 {
                ready.insert(child);
            }
        }
    }

    if ordered.len() != closure.len() {
        let cycle: Vec<String> = closure
            .iter()
            .filter(|&&i| indegree[i] > 0)
            .map(|&i| idx_to_name[i].to_string())
            .collect();
        return Err(Error::DependencyCycle { cycle });
    }

    Ok(ordered
        .into_iter()
        .map(|i| idx_to_name[i].to_string())
        .collect())
}

fn compute_closure(
    roots: &[String],
    formulas: &BTreeMap<String, Formula>,
    name_to_idx: &HashMap<&str, usize>,
) -> Result<BTreeSet<usize>, Error> {
    let mut closure = BTreeSet::new();
    let mut stack: Vec<usize> = Vec::with_capacity(roots.len());

    for root in roots {
        let &idx = name_to_idx
            .get(root.as_str())
            .ok_or_else(|| Error::MissingFormula { name: root.clone() })?;
        stack.push(idx);
    }

    let idx_to_name: Vec<&str> = formulas.keys().map(|k| k.as_str()).collect();

    while let Some(idx) = stack.pop() {
        if !closure.insert(idx) {
            continue;
        }

        let formula = &formulas[idx_to_name[idx]];
        for dep in formula.runtime_dependencies() {
            if let Some(&di) = name_to_idx.get(dep.as_str())
                && !closure.contains(&di)
            {
                stack.push(di);
            }
        }
    }

    Ok(closure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::types::{Bottle, BottleFile, BottleStable, KegOnly, Versions};
    use std::collections::BTreeMap;

    fn formula(name: &str, deps: &[&str]) -> Formula {
        let mut files = BTreeMap::new();
        files.insert(
            "arm64_sonoma".to_string(),
            BottleFile {
                url: format!("https://example.com/{name}.tar.gz"),
                sha256: "deadbeef".repeat(8),
                cellar: None,
            },
        );

        Formula {
            name: name.to_string(),
            versions: Versions {
                stable: "1.0.0".to_string(),
            },
            dependencies: deps.iter().map(|dep| dep.to_string()).collect(),
            bottle: Bottle {
                stable: BottleStable { files, rebuild: 0 },
            },
            revision: 0,
            keg_only: KegOnly::default(),
            keg_only_reason: None,
            build_dependencies: Vec::new(),
            urls: None,
            ruby_source_path: None,
            ruby_source_checksum: None,
            uses_from_macos: Vec::new(),
            requirements: Vec::new(),
            variations: None,
            aliases: Vec::new(),
            oldnames: Vec::new(),
        }
    }

    #[test]
    fn resolves_transitive_closure_in_stable_order() {
        let mut formulas = BTreeMap::new();
        formulas.insert("foo".to_string(), formula("foo", &["baz", "bar"]));
        formulas.insert("bar".to_string(), formula("bar", &["qux"]));
        formulas.insert("baz".to_string(), formula("baz", &["qux"]));
        formulas.insert("qux".to_string(), formula("qux", &[]));

        let order = resolve_closure(&["foo".to_string()], &formulas).unwrap();
        assert_eq!(order, vec!["qux", "bar", "baz", "foo"]);
    }

    #[test]
    fn resolves_multiple_roots_with_shared_deps() {
        let mut formulas = BTreeMap::new();
        formulas.insert("a".to_string(), formula("a", &["shared"]));
        formulas.insert("b".to_string(), formula("b", &["shared"]));
        formulas.insert("shared".to_string(), formula("shared", &[]));

        let order = resolve_closure(&["a".to_string(), "b".to_string()], &formulas).unwrap();
        // shared should come first, then a and b in stable order
        assert_eq!(order, vec!["shared", "a", "b"]);
    }

    #[test]
    fn detects_cycles() {
        let mut formulas = BTreeMap::new();
        formulas.insert("alpha".to_string(), formula("alpha", &["beta"]));
        formulas.insert("beta".to_string(), formula("beta", &["gamma"]));
        formulas.insert("gamma".to_string(), formula("gamma", &["alpha"]));

        let err = resolve_closure(&["alpha".to_string()], &formulas).unwrap_err();
        assert!(matches!(err, Error::DependencyCycle { .. }));
    }

    #[test]
    fn skips_missing_dependencies() {
        // Test that dependencies not in the formulas map are skipped
        // (e.g., platform-incompatible dependencies filtered out during fetch)
        let mut formulas = BTreeMap::new();
        formulas.insert("git".to_string(), formula("git", &["gettext", "libiconv"]));
        formulas.insert("gettext".to_string(), formula("gettext", &[]));
        // libiconv is intentionally missing (filtered out for Linux)

        let order = resolve_closure(&["git".to_string()], &formulas).unwrap();
        // Should successfully resolve with just git and gettext
        assert_eq!(order, vec!["gettext", "git"]);
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn resolves_uses_from_macos_runtime_dependencies_on_linux() {
        use crate::formula::UsesFromMacos;

        let mut formulas = BTreeMap::new();
        let mut python = formula("python@3.14", &["openssl@3"]);
        python.uses_from_macos = vec![UsesFromMacos::Plain("expat".to_string())];

        formulas.insert("python@3.14".to_string(), python);
        formulas.insert("openssl@3".to_string(), formula("openssl@3", &[]));
        formulas.insert("expat".to_string(), formula("expat", &[]));

        let order = resolve_closure(&["python@3.14".to_string()], &formulas).unwrap();
        assert_eq!(order, vec!["expat", "openssl@3", "python@3.14"]);
    }
}

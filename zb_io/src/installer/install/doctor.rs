use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rayon::prelude::*;
use zb_core::{Error, formula_token};

#[cfg(target_os = "android")]
use zb_core::platform::termux;

use crate::cellar::linkage;
use crate::storage::db::StoreRef;

use super::Installer;

#[derive(Debug, Default)]
pub struct DiagnosticReport {
    pub orphaned_cellar_kegs: Vec<OrphanedKeg>,
    pub missing_cellar_kegs: Vec<MissingKeg>,
    pub misnamed_records: Vec<MisnamedRecord>,
    pub orphaned_store_entries: Vec<String>,
    pub stale_store_refs: Vec<StaleStoreRef>,
    pub broken_symlinks: Vec<PathBuf>,
    pub stale_keg_file_records: usize,
    pub broken_linkage: Vec<BrokenLinkage>,
}

/// A library an installed package loads that doesn't exist, so the package
/// fails at runtime. Usually the package and the dependency that owns the
/// library were installed at versions that don't match.
#[derive(Debug)]
pub struct BrokenLinkage {
    pub name: String,
    pub version: String,
    pub library: PathBuf,
    /// The package the library belongs to, when it's loaded from `opt/`.
    pub dependency: Option<String>,
}

#[derive(Debug)]
pub struct OrphanedKeg {
    pub name: String,
    pub version: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct MissingKeg {
    pub name: String,
    pub version: String,
    pub expected_path: PathBuf,
}

/// An install recorded under an alias (`python`) whose keg is under the
/// formula's own name (`python@3.14`), from before installs were recorded
/// under the formula's name.
#[derive(Debug)]
pub struct MisnamedRecord {
    pub recorded_name: String,
    pub actual_name: String,
    pub version: String,
}

#[derive(Debug)]
pub struct StaleStoreRef {
    pub store_key: String,
    pub refcount: i64,
    pub on_disk: bool,
    pub referenced_by_any_keg: bool,
}

impl StaleStoreRef {
    pub fn missing_store_entry(&self) -> bool {
        // Source builds use a synthetic key; only bottles have store entries.
        !self.on_disk && !self.store_key.starts_with(super::SOURCE_KEY_PREFIX)
    }
}

impl DiagnosticReport {
    pub fn is_healthy(&self) -> bool {
        !self.has_repairable_issues() && self.broken_linkage.is_empty()
    }

    /// Whether there's anything `repair` can fix. Broken linkage isn't: it
    /// takes upgrading or reinstalling a package, which is the user's call.
    pub fn has_repairable_issues(&self) -> bool {
        !(self.orphaned_cellar_kegs.is_empty()
            && self.missing_cellar_kegs.is_empty()
            && self.misnamed_records.is_empty()
            && self.orphaned_store_entries.is_empty()
            && self.stale_store_refs.is_empty()
            && self.broken_symlinks.is_empty()
            && self.stale_keg_file_records == 0)
    }
}

impl Installer {
    pub fn doctor(&mut self) -> Result<DiagnosticReport, Error> {
        let mut report = self.diagnose_state()?;
        report.broken_linkage = self.broken_linkage()?;
        Ok(report)
    }

    /// Installed packages that load a library that doesn't exist.
    ///
    /// Report-only for now: see `cellar::linkage` for what is and isn't
    /// covered.
    pub fn broken_linkage(&self) -> Result<Vec<BrokenLinkage>, Error> {
        let installed = self.db.list_installed()?;
        let (cellar, prefix) = (&self.cellar, &self.prefix);
        let broken = installed
            .par_iter()
            .flat_map_iter(|keg| {
                let keg_path = cellar.keg_path(formula_token(&keg.name), &keg.version);
                linkage::missing_libraries(&keg_path, prefix)
                    .into_iter()
                    .map(|library| BrokenLinkage {
                        name: keg.name.clone(),
                        version: keg.version.clone(),
                        dependency: linkage::library_owner(&library, prefix),
                        library,
                    })
            })
            .collect();
        Ok(broken)
    }

    /// Everything `doctor` checks except linkage, which is slow and which
    /// `repair` can't change.
    fn diagnose_state(&mut self) -> Result<DiagnosticReport, Error> {
        let mut report = DiagnosticReport::default();

        let installed = self.db.list_installed()?;
        let db_store_refs = self.db.list_store_refs()?;
        let disk_store_entries = self.store.list_entries()?;
        let cellar_kegs = self.cellar.list_kegs()?;

        let mut recorded_kegs: HashSet<String> = installed
            .iter()
            .map(|k| formula_token(&k.name).to_string())
            .collect();

        // A record whose keg is missing may just be under the wrong name. The
        // bottle in the store says which formula it really is.
        for keg in &installed {
            let token = formula_token(&keg.name);
            if self.cellar.keg_path(token, &keg.version).exists() {
                continue;
            }
            if let Some(actual) = self.bottle_formula_name(&keg.store_key, &keg.version)
                && actual != token
            {
                recorded_kegs.insert(actual.clone());
                report.misnamed_records.push(MisnamedRecord {
                    recorded_name: keg.name.clone(),
                    actual_name: actual,
                    version: keg.version.clone(),
                });
            }
        }

        for keg in &cellar_kegs {
            if !recorded_kegs.contains(&keg.name) {
                report.orphaned_cellar_kegs.push(OrphanedKeg {
                    name: keg.name.clone(),
                    version: keg.version.clone(),
                    path: keg.path.clone(),
                });
            }
        }

        for keg in &installed {
            let token = formula_token(&keg.name);
            let expected_path = self.cellar.keg_path(token, &keg.version);
            let misnamed = report
                .misnamed_records
                .iter()
                .any(|m| m.recorded_name == keg.name);
            if !expected_path.exists() && !misnamed {
                report.missing_cellar_kegs.push(MissingKeg {
                    name: keg.name.clone(),
                    version: keg.version.clone(),
                    expected_path,
                });
            }
        }

        let store_keys_in_db: HashSet<&str> =
            db_store_refs.iter().map(|r| r.store_key.as_str()).collect();

        let disk_store_set: HashSet<&str> = disk_store_entries.iter().map(String::as_str).collect();

        let store_keys_used: HashMap<&str, i64> = {
            let mut map = HashMap::new();
            for keg in &installed {
                *map.entry(keg.store_key.as_str()).or_insert(0) += 1;
            }
            map
        };

        for entry in &disk_store_entries {
            if !store_keys_in_db.contains(entry.as_str())
                && !store_keys_used.contains_key(entry.as_str())
            {
                report.orphaned_store_entries.push(entry.clone());
            }
        }

        for store_ref in &db_store_refs {
            let actual_count = store_keys_used
                .get(store_ref.store_key.as_str())
                .copied()
                .unwrap_or(0);
            let stale = StaleStoreRef {
                store_key: store_ref.store_key.clone(),
                refcount: store_ref.refcount,
                on_disk: disk_store_set.contains(store_ref.store_key.as_str()),
                referenced_by_any_keg: actual_count > 0,
            };

            if stale.refcount != actual_count || stale.missing_store_entry() {
                report.stale_store_refs.push(stale);
            }
        }

        let keg_files = self.db.list_keg_files()?;
        let installed_set: HashSet<(&str, &str)> = installed
            .iter()
            .map(|k| (k.name.as_str(), k.version.as_str()))
            .collect();

        for keg in &installed {
            let token = formula_token(&keg.name);
            let keg_path = self.cellar.keg_path(token, &keg.version);
            if keg_path.exists() {
                let linked = self.linker.collect_linked_files(&keg_path)?;
                for file in linked {
                    if !file.target_path.exists() {
                        report.broken_symlinks.push(file.link_path);
                    }
                }
            }
        }

        for record in &keg_files {
            if !installed_set.contains(&(record.name.as_str(), record.version.as_str())) {
                continue;
            }
            let link = PathBuf::from(&record.linked_path);
            if link.is_symlink() && !link.exists() && !report.broken_symlinks.contains(&link) {
                report.broken_symlinks.push(link);
            }
        }

        report.stale_keg_file_records = self.db.count_stale_keg_file_records()?;

        Ok(report)
    }

    /// Repair until diagnostics come back clean. Fixing one issue can expose
    /// another, like a store entry left unreferenced once a stale record is
    /// removed. Stops early if a pass fixes nothing. Returns everything that
    /// was fixed, and what's left.
    pub fn repair_until_healthy(
        &mut self,
        mut report: DiagnosticReport,
    ) -> Result<(RepairSummary, DiagnosticReport), Error> {
        const MAX_PASSES: usize = 5;
        let mut total = RepairSummary::default();
        for _ in 0..MAX_PASSES {
            let pass = self.repair(&report)?;
            total.add(&pass);
            report = self.diagnose_state()?;
            if report.is_healthy() || pass.total_fixes() == 0 {
                break;
            }
        }
        report.broken_linkage = self.broken_linkage()?;
        Ok((total, report))
    }

    pub fn repair(&mut self, report: &DiagnosticReport) -> Result<RepairSummary, Error> {
        let mut summary = RepairSummary::default();

        for misnamed in &report.misnamed_records {
            let duplicate = self.db.get_installed(&misnamed.actual_name).is_some();
            let tx = self.db.transaction()?;
            if duplicate {
                tx.delete_installed_record(&misnamed.recorded_name)?;
            } else {
                tx.rename_installed(&misnamed.recorded_name, &misnamed.actual_name)?;
            }
            tx.commit()?;
            summary.renamed_records += 1;
        }

        for orphan in &report.orphaned_cellar_kegs {
            self.linker.unlink_keg(&orphan.path).ok();
            self.cellar.remove_keg(&orphan.name, &orphan.version)?;
            summary.removed_orphaned_kegs += 1;
        }

        for missing in &report.missing_cellar_kegs {
            let tx = self.db.transaction()?;
            tx.delete_installed_record(&missing.name)?;
            tx.commit()?;
            summary.removed_missing_records += 1;
        }

        let needs_refcount_recompute = !report.stale_store_refs.is_empty()
            || !report.missing_cellar_kegs.is_empty()
            || !report.misnamed_records.is_empty();

        if needs_refcount_recompute {
            let installed = self.db.list_installed()?;
            let mut corrected: HashMap<&str, i64> = HashMap::new();
            for keg in &installed {
                *corrected.entry(keg.store_key.as_str()).or_insert(0) += 1;
            }

            let corrected_refs: Vec<StoreRef> = corrected
                .into_iter()
                .map(|(store_key, refcount)| StoreRef {
                    store_key: store_key.to_owned(),
                    refcount,
                })
                .collect();

            self.db.replace_store_refs(&corrected_refs)?;
            summary.fixed_store_refs =
                report.stale_store_refs.len() + report.missing_cellar_kegs.len();
        }

        for key in &report.orphaned_store_entries {
            self.store.remove_entry(key)?;
            summary.removed_orphaned_store_entries += 1;
        }

        for link in &report.broken_symlinks {
            let _ = std::fs::remove_file(link);
            summary.removed_broken_symlinks += 1;
        }

        if report.stale_keg_file_records > 0 {
            summary.pruned_keg_file_records = self.db.prune_stale_keg_file_records()?;
        }

        Ok(summary)
    }

    /// The formula a store entry's bottle belongs to, if its keg for
    /// `version` exists in the Cellar. Bottles unpack as `<formula>/<version>/`.
    fn bottle_formula_name(&self, store_key: &str, version: &str) -> Option<String> {
        std::fs::read_dir(self.store.entry_path(store_key))
            .ok()?
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .find(|name| self.cellar.keg_path(name, version).exists())
    }
}

#[derive(Debug, Default)]
pub struct RepairSummary {
    pub renamed_records: usize,
    pub removed_orphaned_kegs: usize,
    pub removed_missing_records: usize,
    pub fixed_store_refs: usize,
    pub removed_orphaned_store_entries: usize,
    pub removed_broken_symlinks: usize,
    pub pruned_keg_file_records: usize,
}

impl RepairSummary {
    fn add(&mut self, other: &RepairSummary) {
        self.renamed_records += other.renamed_records;
        self.removed_orphaned_kegs += other.removed_orphaned_kegs;
        self.removed_missing_records += other.removed_missing_records;
        self.fixed_store_refs += other.fixed_store_refs;
        self.removed_orphaned_store_entries += other.removed_orphaned_store_entries;
        self.removed_broken_symlinks += other.removed_broken_symlinks;
        self.pruned_keg_file_records += other.pruned_keg_file_records;
    }

    pub fn total_fixes(&self) -> usize {
        self.renamed_records
            + self.removed_orphaned_kegs
            + self.removed_missing_records
            + self.fixed_store_refs
            + self.removed_orphaned_store_entries
            + self.removed_broken_symlinks
            + self.pruned_keg_file_records
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::{Database, StoreRef};
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    const KEY: &str = "0123456789abcdef";

    /// An installer with python@3.14 3.14.7 in the store and Cellar, recorded
    /// under each of `records`.
    fn installer_with_python_recorded_as(tmp: &TempDir, records: &[&str]) -> Installer {
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("prefix");
        fs::create_dir_all(root.join("db")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();

        fs::create_dir_all(store.entry_path(KEY).join("python@3.14/3.14.7/bin")).unwrap();
        fs::create_dir_all(cellar.keg_path("python@3.14", "3.14.7").join("bin")).unwrap();

        let mut db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tx = db.transaction().unwrap();
        for name in records {
            tx.record_install(name, "3.14.7", KEY).unwrap();
        }
        tx.commit().unwrap();

        Installer::new(
            ApiClient::with_base_url("http://127.0.0.1:9/formula".to_string()).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            store,
            cellar,
            Linker::new(&prefix).unwrap(),
            db,
            prefix,
            root.join("locks"),
        )
    }

    #[test]
    fn alias_record_is_reported_as_misnamed_not_missing_and_orphaned() {
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_with_python_recorded_as(&tmp, &["python"]);

        let report = installer.doctor().unwrap();

        assert_eq!(report.misnamed_records.len(), 1);
        assert_eq!(report.misnamed_records[0].recorded_name, "python");
        assert_eq!(report.misnamed_records[0].actual_name, "python@3.14");
        assert!(report.missing_cellar_kegs.is_empty());
        assert!(report.orphaned_cellar_kegs.is_empty());
    }

    #[test]
    fn repair_renames_alias_records_and_keeps_the_keg() {
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_with_python_recorded_as(&tmp, &["python"]);

        let report = installer.doctor().unwrap();
        installer.repair(&report).unwrap();

        assert!(installer.db.get_installed("python").is_none());
        assert_eq!(
            installer.db.get_installed("python@3.14").unwrap().version,
            "3.14.7"
        );
        assert!(installer.cellar.keg_path("python@3.14", "3.14.7").exists());
        assert!(installer.doctor().unwrap().is_healthy());
    }

    #[test]
    fn repair_until_healthy_cleans_up_what_earlier_fixes_expose() {
        // A record whose keg is gone: removing the record leaves its store
        // entry unreferenced, which only shows up on the next diagnosis.
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_with_python_recorded_as(&tmp, &["python@3.14"]);
        fs::remove_dir_all(installer.cellar.keg_path("python@3.14", "3.14.7")).unwrap();

        let report = installer.doctor().unwrap();
        assert!(report.orphaned_store_entries.is_empty());
        let (summary, remaining) = installer.repair_until_healthy(report).unwrap();

        assert!(remaining.is_healthy());
        assert_eq!(summary.removed_missing_records, 1);
        assert_eq!(summary.removed_orphaned_store_entries, 1);
        assert!(!installer.store.entry_path(KEY).exists());
    }

    #[test]
    fn repair_drops_alias_records_duplicating_the_real_one() {
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_with_python_recorded_as(&tmp, &["python", "python@3.14"]);

        let report = installer.doctor().unwrap();
        installer.repair(&report).unwrap();

        assert!(installer.db.get_installed("python").is_none());
        assert!(installer.db.get_installed("python@3.14").is_some());
        assert!(installer.cellar.keg_path("python@3.14", "3.14.7").exists());
        assert!(installer.doctor().unwrap().is_healthy());
    }

    #[test]
    fn source_install_has_no_store_entry() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("prefix");
        fs::create_dir_all(root.join("db")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        fs::create_dir_all(cellar.keg_path("sourcepkg", "1.0")).unwrap();

        let mut db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        let tx = db.transaction().unwrap();
        tx.record_install("sourcepkg", "1.0", "source:sourcepkg:1.0")
            .unwrap();
        tx.commit().unwrap();

        let mut installer = Installer::new(
            ApiClient::with_base_url("http://127.0.0.1:9/formula".to_string()).unwrap(),
            BlobCache::new(&root.join("cache")).unwrap(),
            store,
            cellar,
            Linker::new(&prefix).unwrap(),
            db,
            prefix,
            root.join("locks"),
        );

        assert!(installer.doctor().unwrap().is_healthy());

        installer
            .db
            .replace_store_refs(&[StoreRef {
                store_key: "source:sourcepkg:1.0".to_string(),
                refcount: 0,
            }])
            .unwrap();
        let report = installer.doctor().unwrap();
        assert_eq!(report.stale_store_refs.len(), 1);
        assert!(!report.stale_store_refs[0].on_disk);
        assert!(!report.stale_store_refs[0].missing_store_entry());
        installer.repair_until_healthy(report).unwrap();
        assert!(installer.doctor().unwrap().is_healthy());
    }

    #[test]
    fn bottle_ref_without_store_entry_is_stale() {
        let tmp = TempDir::new().unwrap();
        let mut installer = installer_with_python_recorded_as(&tmp, &["python@3.14"]);
        fs::remove_dir_all(installer.store.entry_path(KEY)).unwrap();

        let report = installer.doctor().unwrap();
        assert_eq!(report.stale_store_refs.len(), 1);
        assert_eq!(report.stale_store_refs[0].store_key, KEY);
        assert!(report.stale_store_refs[0].missing_store_entry());
        assert!(!report.is_healthy());
    }
}

#[cfg(target_os = "android")]
pub fn android_doctor_messages() -> Vec<String> {
    let mut messages = Vec::new();

    let Some(prefix) = termux::termux_prefix() else {
        messages.push(
            "TERMUX__PREFIX is not set; Linux bottles need the Termux glibc package".to_string(),
        );
        return messages;
    };

    match termux::resolve_glibc_dynamic_linker(&prefix) {
        Some(ld) => {
            messages.push(format!("Termux glibc dynamic linker: {}", ld.display()));
        }
        None => {
            messages.push(format!(
                "Termux glibc dynamic linker not found under {}/glibc/lib (enable the glibc apt repo and run: pkg install glibc)",
                prefix.display()
            ));
        }
    }

    if let Ok(preload) = std::env::var("LD_PRELOAD")
        && !preload.is_empty()
    {
        messages.push(format!(
            "Warning: LD_PRELOAD is set ({}); this can break glibc bottles on Termux",
            preload
        ));
    }

    messages
}

#[cfg(test)]
#[cfg(target_os = "android")]
mod android_doctor_tests {
    use super::android_doctor_messages;

    #[test]
    fn reports_missing_prefix() {
        unsafe {
            std::env::remove_var("TERMUX__PREFIX");
            std::env::remove_var("ZEROBREW_GLIBC_LD");
        }
        let msgs = android_doctor_messages();
        assert!(msgs.iter().any(|m| m.contains("TERMUX__PREFIX is not set")));
    }
}

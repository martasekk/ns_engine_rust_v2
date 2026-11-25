//! Flow files on disk: `<root>/flows/<id>/<version>.json`, beside `personas/` and `modules/`.
//! Every save is a new file, so a version once written never changes.
//! The admin page decides what may be saved; this module only reads and writes.
use nscore::flow::FlowDef;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The library directory flows live in, beside personas/ and modules/.
pub(crate) const FLOW_DIR: &str = "flows";

/// One row of the flow list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct FlowSummary {
    pub(crate) id: String,
    pub(crate) latest_version: u32,
}

/// The directory of one flow: `root/flows/id`.
pub(crate) fn flow_dir(root: &Path, id: &str) -> PathBuf {
    root.join(FLOW_DIR).join(id)
}

/// Refuses any name that could escape `root/flows` (`../`, separators, empty).
fn check_name(id: &str) -> Result<(), String> {
    if crate::tenant::valid_library_name(id) {
        Ok(())
    } else {
        Err(format!(
            "{id:?} is not a usable flow name: letters, digits, - and _ only"
        ))
    }
}

/// The version numbers stored for one flow, ascending. Only files named exactly `<n>.json` count.
pub(crate) fn versions(root: &Path, id: &str) -> Result<Vec<u32>, String> {
    check_name(id)?;
    let dir = flow_dir(root, id);
    if !dir.exists() {
        return Ok(vec![]);
    }
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if let Ok(n) = stem.parse::<u32>() {
            if n.to_string() == stem {
                out.push(n);
            }
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Every flow under `root/flows` that has at least one version, sorted by id.
pub(crate) fn list(root: &Path) -> Result<Vec<FlowSummary>, String> {
    let dir = root.join(FLOW_DIR);
    if !dir.exists() {
        return Ok(vec![]);
    }
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let Ok(id) = entry.file_name().into_string() else {
            continue;
        };
        if !crate::tenant::valid_library_name(&id) || !entry.path().is_dir() {
            continue;
        }
        if let Some(&latest_version) = versions(root, &id)?.last() {
            out.push(FlowSummary { id, latest_version });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// One version of a flow; `None` means the latest. A missing version is `Ok(None)`, not an error.
pub(crate) fn load(root: &Path, id: &str, version: Option<u32>) -> Result<Option<FlowDef>, String> {
    let vs = versions(root, id)?;
    let Some(v) = version.or(vs.last().copied()) else {
        return Ok(None);
    };
    if !vs.contains(&v) {
        return Ok(None);
    }
    let path = flow_dir(root, id).join(format!("{v}.json"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes a new version file for `flow`; an existing version is never overwritten. Returns its path.
pub(crate) fn put(root: &Path, flow: &FlowDef) -> Result<PathBuf, String> {
    check_name(&flow.id)?;
    let dir = flow_dir(root, &flow.id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.json", flow.version));
    let text =
        serde_json::to_string_pretty(flow).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(format!("{text}\n").as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Removes one version file. The flow's directory is left in place even when empty.
pub(crate) fn delete(root: &Path, id: &str, version: u32) -> Result<(), String> {
    check_name(id)?;
    let path = flow_dir(root, id).join(format!("{version}.json"));
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(id: &str, v: u32) -> FlowDef {
        serde_json::from_value(serde_json::json!({
            "id": id, "version": v, "roles": {}, "required_roles": [],
            "actions": [], "fast_path": [], "reply_template": null
        }))
        .expect("test flow")
    }

    #[test]
    fn an_empty_root_lists_nothing() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        assert_eq!(list(root).unwrap(), Vec::<FlowSummary>::new());
        assert_eq!(versions(root, "x").unwrap(), Vec::<u32>::new());
        assert!(load(root, "x", None).unwrap().is_none());
    }

    #[test]
    fn put_then_load_latest_and_by_version() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        put(root, &flow("trip", 1)).unwrap();
        put(root, &flow("trip", 3)).unwrap();
        assert_eq!(versions(root, "trip").unwrap(), vec![1, 3]);
        assert_eq!(
            load(root, "trip", None).unwrap().map(|f| f.version),
            Some(3)
        );
        assert_eq!(
            load(root, "trip", Some(1)).unwrap().map(|f| f.version),
            Some(1)
        );
        assert!(load(root, "trip", Some(2)).unwrap().is_none());
        assert_eq!(
            list(root).unwrap(),
            vec![FlowSummary {
                id: "trip".into(),
                latest_version: 3
            }]
        );
    }

    #[test]
    fn a_version_is_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        put(root, &flow("t", 1)).unwrap();
        assert!(put(root, &flow("t", 1)).is_err());
        let text = std::fs::read_to_string(root.join("flows/t/1.json")).unwrap();
        let back: FlowDef = serde_json::from_str(&text).unwrap();
        assert_eq!(back.version, 1);
    }

    #[test]
    fn odd_files_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        put(root, &flow("a", 2)).unwrap();
        std::fs::write(root.join("flows/a/01.json"), "{}").unwrap();
        std::fs::write(root.join("flows/a/notes.txt"), "x").unwrap();
        std::fs::write(root.join("flows/a/3.json.bak"), "{}").unwrap();
        std::fs::create_dir_all(root.join("flows/bad name")).unwrap();
        std::fs::write(root.join("flows/bad name/1.json"), "{}").unwrap();
        assert_eq!(versions(root, "a").unwrap(), vec![2]);
        let ids: Vec<String> = list(root).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["a".to_string()]);
    }

    #[test]
    fn a_bad_name_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        assert!(versions(root, "../x").is_err());
        assert!(load(root, "a/b", None).is_err());
        assert!(delete(root, "..", 1).is_err());
        assert!(put(root, &flow("../evil", 1)).is_err());
        assert!(!root.join("flows").exists());
        assert!(!root.join("evil").exists());
    }

    #[test]
    fn delete_removes_one_version() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        put(root, &flow("t", 1)).unwrap();
        put(root, &flow("t", 2)).unwrap();
        delete(root, "t", 2).unwrap();
        assert_eq!(versions(root, "t").unwrap(), vec![1]);
        assert!(delete(root, "t", 2).is_err());
    }

    #[test]
    fn a_malformed_file_is_an_error_naming_it() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("flows/z")).unwrap();
        std::fs::write(root.join("flows/z/1.json"), "{not json").unwrap();
        match load(root, "z", None) {
            Err(e) => assert!(e.contains("1.json"), "error was: {e}"),
            Ok(_) => panic!("a malformed file loaded"),
        }
    }
}

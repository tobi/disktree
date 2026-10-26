//! Reports made from real scans of a scratch tree.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use disktree_agent::query::{
    OlderThan, Query, Sections, normalize_extension, parse_category,
    parse_reclaim,
};
use disktree_agent::report::{self, Scanned};
use serde_json::Value;

fn write(root: &Path, relative: &str, bytes: usize) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(&path, vec![b'x'; bytes]).expect("write");
}

fn age(root: &Path, relative: &str, days: u64) {
    let file = fs::File::options()
        .write(true)
        .open(root.join(relative))
        .expect("open");
    let then = SystemTime::now() - Duration::from_secs(days * 86_400);
    file.set_modified(then).expect("set mtime");
}

/// A home-like tree with a cache, a build dir, media and documents.
fn tree() -> (tempfile::TempDir, Scanned) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root, "Movies/film.mkv", 50_000);
    write(root, "Movies/trailer.mp4", 8_000);
    write(root, "Downloads/ubuntu.iso", 30_000);
    write(root, "Downloads/notes.txt", 100);
    write(root, ".cache/pip/wheels/a.whl", 12_000);
    write(root, ".cache/pip/http/b.bin", 3_000);
    write(root, "src/app/target/debug/app", 20_000);
    write(root, "src/app/Cargo.toml", 200);
    write(root, "src/app/node_modules/x/index.js", 900);
    age(root, "Downloads/ubuntu.iso", 400);
    age(root, "Movies/film.mkv", 10);
    let canonical = dunce::canonicalize(root).expect("canonical");
    let mut query = Query::new(canonical.clone());
    query.settings.apparent_size = true;
    let scanned =
        report::scan_quietly(&canonical, &query.settings).expect("scan");
    (temp, scanned)
}

fn base(scanned: &Scanned) -> Query {
    let mut query = Query::new(scanned.root.clone());
    query.settings = scanned.settings.clone();
    query
}

fn make(scanned: &Scanned, query: &Query) -> Value {
    report::report(scanned, query, chrono::Utc::now().timestamp())
        .expect("report")
}

fn names(report: &Value, key: &str) -> Vec<String> {
    report[key]
        .as_array()
        .expect(key)
        .iter()
        .map(|entry| entry["name"].as_str().expect("name").to_owned())
        .collect()
}

#[test]
fn the_report_has_the_documented_shape() {
    let (_temp, scanned) = tree();
    let report = make(&scanned, &base(&scanned));
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["generator"]["name"], "disktree");
    assert_eq!(report["root"], scanned.root.to_string_lossy().as_ref());
    assert!(report["scanned_at"].as_str().expect("time").ends_with('Z'));
    assert_eq!(report["options"]["size_mode"], "apparent");
    assert_eq!(report["options"]["one_filesystem"], true);
    assert_eq!(report["total"]["bytes"], 124_200);
    assert_eq!(report["total"]["size"], "121 KiB");
    assert_eq!(report["total"]["files"], 9);
    assert_eq!(report["errors"]["unreadable_paths"], 0);

    let files = report["largest_files"].as_array().expect("files");
    assert_eq!(files.len(), 9);
    let first = &files[0];
    assert_eq!(first["name"], "film.mkv");
    assert_eq!(first["type"], "file");
    assert_eq!(first["bytes"], 50_000);
    assert_eq!(first["size"], "49 KiB");
    assert_eq!(first["depth"], 2);
    // Compared as paths, so the separator is the platform's own.
    assert_eq!(
        Path::new(first["path"].as_str().expect("path")),
        scanned.root.join("Movies").join("film.mkv")
    );
    assert!(first["modified"].as_str().expect("modified").ends_with('Z'));
    assert_eq!(first["age_days"], 10);
    assert_eq!(first["percent"], 40.3);
    assert!(first.get("files").is_none(), "files only on directories");

    let dirs = report["largest_dirs"].as_array().expect("dirs");
    assert_eq!(dirs[0]["name"], "Movies");
    assert_eq!(dirs[0]["type"], "directory");
    assert_eq!(dirs[0]["files"], 2);
    assert_eq!(dirs[0]["unreadable"], false);
    // The root itself is never listed.
    assert!(dirs.iter().all(|dir| dir["depth"].as_u64() >= Some(1)));
    // Biggest first.
    let sizes: Vec<u64> = dirs
        .iter()
        .map(|dir| dir["bytes"].as_u64().expect("bytes"))
        .collect();
    assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]));
}

#[test]
fn top_depth_and_sections_shape_the_lists() {
    let (_temp, scanned) = tree();
    let mut query = base(&scanned);
    query.top = 2;
    query.depth = Some(1);
    let report = make(&scanned, &query);
    assert_eq!(names(&report, "largest_dirs"), ["Movies", "Downloads"]);
    assert!(
        report["largest_files"]
            .as_array()
            .expect("files")
            .is_empty()
    );
    assert_eq!(report["matched"]["directories"], 4);

    query.sections = Sections::DIRS;
    let report = make(&scanned, &query);
    assert!(report.get("largest_files").is_none());
    assert!(report["matched"].get("files").is_none());

    query.sections = Sections::FILES;
    query.depth = Some(2);
    let report = make(&scanned, &query);
    assert!(report.get("largest_dirs").is_none());
    assert_eq!(names(&report, "largest_files"), ["film.mkv", "ubuntu.iso"]);
}

#[test]
fn filters_narrow_files() {
    let (_temp, scanned) = tree();
    let mut query = base(&scanned);
    query.sections = Sections::FILES;
    query.filters.min_size = Some(10_000);
    let report = make(&scanned, &query);
    assert_eq!(
        names(&report, "largest_files"),
        ["film.mkv", "ubuntu.iso", "app", "a.whl"]
    );
    assert_eq!(report["matched"]["files"], 4);
    assert_eq!(report["matched"]["file_bytes"], 112_000);
    assert_eq!(report["filters"]["min_size_bytes"], 10_000);

    let mut query = base(&scanned);
    query.sections = Sections::FILES;
    query.filters.extensions = ["ISO", ".mp4"]
        .into_iter()
        .filter_map(normalize_extension)
        .collect();
    assert_eq!(
        names(&make(&scanned, &query), "largest_files"),
        ["ubuntu.iso", "trailer.mp4"]
    );

    let mut query = base(&scanned);
    query.sections = Sections::FILES;
    query.filters.older_than = Some(OlderThan::Age(365 * 86_400));
    let report = make(&scanned, &query);
    assert_eq!(names(&report, "largest_files"), ["ubuntu.iso"]);
    assert!(report["filters"]["older_than"].as_str().is_some());

    let mut query = base(&scanned);
    query.sections = Sections::FILES;
    query.filters.names = vec!["*.t?t".into()];
    assert_eq!(
        names(&make(&scanned, &query), "largest_files"),
        ["notes.txt"]
    );
}

#[test]
fn category_and_reclaim_list_the_outermost_folder() {
    let (_temp, scanned) = tree();
    let mut query = base(&scanned);
    query.sections = Sections::DIRS;
    query.filters.category = Some(parse_category("cache").expect("cache"));
    let report = make(&scanned, &query);
    // `.cache` and `node_modules`, not `.cache/pip` and everything inside.
    assert_eq!(names(&report, "largest_dirs"), [".cache", "node_modules"]);
    assert_eq!(report["largest_dirs"][0]["category"], "cache");

    query.filters.category = None;
    query.filters.reclaim = Some(parse_reclaim("any").expect("any"));
    let report = make(&scanned, &query);
    let listed = names(&report, "largest_dirs");
    assert!(listed.contains(&"target".to_owned()), "{listed:?}");
    assert!(!listed.contains(&"debug".to_owned()), "{listed:?}");
    assert!(
        report["largest_dirs"]
            .as_array()
            .expect("dirs")
            .iter()
            .all(|dir| dir["reclaim"].is_string())
    );
}

#[test]
fn excluded_paths_are_not_scanned() {
    let (temp, _) = tree();
    let root = dunce::canonicalize(temp.path()).expect("canonical");
    let mut query = Query::new(root.clone());
    query.settings.apparent_size = true;
    query.settings.exclude = vec!["Movies".into(), "*.iso".into()];
    let scanned = report::scan_quietly(&root, &query.settings).expect("scan");
    let report = make(&scanned, &query);
    assert_eq!(report["total"]["bytes"], 124_200 - 58_000 - 30_000);
    assert_eq!(report["options"]["exclude"][0], "Movies");
    assert!(
        !names(&report, "largest_files").contains(&"ubuntu.iso".to_owned())
    );
}

#[test]
fn text_output_lists_the_same_entries() {
    let (_temp, scanned) = tree();
    let report = make(&scanned, &base(&scanned));
    let text = report::text(&report);
    assert!(text.contains("Largest directories"));
    assert!(text.contains("film.mkv"));
    assert!(text.contains("121 KiB apparent"));
}

#[test]
fn the_summary_describes_the_volume() {
    let temp = tempfile::tempdir().expect("tempdir");
    let summary = report::summary(temp.path()).expect("summary");
    assert_eq!(summary["schema_version"], 1);
    let capacity = summary["capacity"]["bytes"].as_u64().expect("capacity");
    assert!(capacity > 0);
    assert!(
        summary["available"]["bytes"].as_u64().expect("available") <= capacity
    );
    assert!(summary["used_percent"].as_f64().is_some());
}

#[test]
fn the_cli_prints_the_same_json() {
    let (temp, scanned) = tree();
    let path = temp.path().to_str().expect("utf-8").to_owned();
    let args: Vec<std::ffi::OsString> =
        [path.as_str(), "--json", "-a", "--top", "3"]
            .iter()
            .map(Into::into)
            .collect();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let status = disktree_agent::cli::run("scan", &args, &mut out, &mut err);
    assert_eq!(status, 0, "{}", String::from_utf8_lossy(&err));
    let printed: Value = serde_json::from_slice(&out).expect("json");
    let mut query = base(&scanned);
    query.top = 3;
    let expected = make(&scanned, &query);
    for key in [
        "schema_version",
        "root",
        "total",
        "largest_dirs",
        "largest_files",
        "matched",
    ] {
        assert_eq!(printed[key], expected[key], "{key}");
    }
}

fn sum(list: &Value) -> u64 {
    list.as_array()
        .expect("list")
        .iter()
        .map(|entry| entry["bytes"].as_u64().expect("bytes"))
        .sum()
}

#[test]
fn every_breakdown_adds_up_to_the_total() {
    let (_temp, scanned) = tree();
    let mut query = base(&scanned);
    query.top = 2;
    let report = make(&scanned, &query);
    let total = report["total"]["bytes"].as_u64().expect("total");
    let breakdown = &report["breakdown"];
    assert_eq!(
        sum(&breakdown["children"])
            + breakdown["children_rest"]["bytes"].as_u64().expect("rest"),
        total
    );
    assert_eq!(breakdown["children"].as_array().expect("children").len(), 2);
    assert_eq!(breakdown["children_rest"]["count"], 2);
    assert_eq!(sum(&breakdown["by_category"]), total);
    assert_eq!(sum(&breakdown["by_age"]), total);
    let files: u64 = breakdown["by_category"]
        .as_array()
        .expect("categories")
        .iter()
        .map(|row| row["files"].as_u64().expect("files"))
        .sum();
    assert_eq!(files, 9);

    // Ages: film.mkv is 10 days old, ubuntu.iso 400; the rest are new.
    let ages = breakdown["by_age"].as_array().expect("ages");
    assert_eq!(ages[0]["age"], "under_30_days");
    assert_eq!(ages[0]["bytes"], 124_200 - 30_000);
    assert_eq!(ages[3]["age"], "over_1_year");
    assert_eq!(ages[3]["bytes"], 30_000);
    assert_eq!(ages[3]["percent"], 24.2);

    let reclaimable = &report["reclaimable"];
    assert_eq!(
        sum(&reclaimable["by_reason"]),
        reclaimable["bytes"].as_u64().expect("reclaimable")
    );
    // .cache (15 000) and target (20 000), outermost only, so .cache/pip
    // is not counted a second time. node_modules has no package.json
    // beside it here, so it is not called reinstallable.
    assert_eq!(reclaimable["bytes"], 35_000);
    assert_eq!(reclaimable["places"], 2);
    assert!(
        reclaimable["by_reason"][0]["description"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(reclaimable["largest"][0]["name"], "target");
    // Apparent sizes are not what freeing gives back: no projection.
    assert!(reclaimable["available_after"].is_null());
    assert!(report["volume"]["root_percent_of_used"].as_f64().is_some());
}

/// On-disk sizes on one volume are what freeing gives back, so the
/// projection is the volume's available space plus the reclaimable bytes.
#[test]
fn the_free_space_projection_uses_on_disk_bytes_only() {
    let (temp, _) = tree();
    let root = dunce::canonicalize(temp.path()).expect("canonical");
    let query = Query::new(root.clone());
    let scanned =
        report::scan_quietly(&root, &query.settings).expect("on-disk scan");
    let report = make(&scanned, &query);
    let reclaimable = &report["reclaimable"];
    let bytes = reclaimable["bytes"].as_u64().expect("reclaimable");
    let available = report["volume"]["available"]["bytes"]
        .as_u64()
        .expect("available");
    let after = reclaimable["available_after"]["bytes"]
        .as_u64()
        .expect("a projection");
    // The volume may change between the two reads by the scan's own
    // neighbours; allow a little drift.
    assert!(after.abs_diff(available + bytes) < 64 * 1024 * 1024);

    for (apparent, follow) in [(true, false), (false, true)] {
        let mut query = Query::new(root.clone());
        query.settings.apparent_size = apparent;
        query.settings.follow_links = follow;
        let scanned =
            report::scan_quietly(&root, &query.settings).expect("scan");
        assert!(
            make(&scanned, &query)["reclaimable"]["available_after"].is_null(),
            "apparent {apparent}, follow {follow}"
        );
    }
}

/// The top-level children are chosen by bytes even when the tree was
/// ranked by file count, as the window's is in its Files mode.
#[test]
fn top_children_are_the_largest_by_bytes_whatever_the_ranking() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root, "big/one.bin", 50_000);
    for index in 0..20 {
        write(root, &format!("many/{index}.txt"), 10);
    }
    let canonical = dunce::canonicalize(root).expect("canonical");
    let mut settings = Query::new(canonical.clone()).settings;
    settings.apparent_size = true;
    let options = disktree_core::scan::ScanOptions {
        metric: disktree_core::tree::Metric::Files,
        ..settings.options().expect("options")
    };
    let tree = disktree_core::scan::scan(&canonical, options).expect("scan");
    assert_eq!(&*tree.children[0].name, "many", "ranked by files");
    let scanned = Scanned {
        root: canonical,
        tree: std::sync::Arc::new(tree),
        host: None,
        settings,
        scanned_at: chrono::Utc::now().timestamp(),
        elapsed: std::time::Duration::ZERO,
        errors: 0,
        error_examples: Vec::new(),
    };
    let mut query = base(&scanned);
    query.top = 1;
    let report = make(&scanned, &query);
    assert_eq!(report["breakdown"]["children"][0]["name"], "big");
    assert_eq!(report["breakdown"]["children_rest"]["count"], 1);
}

#[test]
fn listed_directories_say_which_listed_folder_holds_them() {
    let (_temp, scanned) = tree();
    let mut query = base(&scanned);
    query.sections = Sections::DIRS;
    query.top = 50;
    let report = make(&scanned, &query);
    let dirs = report["largest_dirs"].as_array().expect("dirs");
    let find = |name: &str| {
        dirs.iter()
            .find(|dir| dir["name"] == name)
            .unwrap_or_else(|| panic!("{name} listed"))
    };
    assert_eq!(find("Movies")["nested_in"], Value::Null);
    let src = find("src")["path"].as_str().expect("path").to_owned();
    assert_eq!(find("app")["nested_in"], src.as_str());
    let app = find("app")["path"].as_str().expect("path").to_owned();
    assert_eq!(find("target")["nested_in"], app.as_str());
}

#[test]
fn a_folder_is_not_called_a_cache_for_holding_one() {
    // `~/Library` holds `Caches`; the window colours it as a cache, but
    // counting all of it as one would be wrong.
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root, "Library/Caches/x/blob", 1_000);
    write(root, "Library/Application Support/app/data", 9_000);
    let canonical = dunce::canonicalize(root).expect("canonical");
    let mut query = Query::new(canonical.clone());
    query.settings.apparent_size = true;
    let scanned =
        report::scan_quietly(&canonical, &query.settings).expect("scan");
    let report = make(&scanned, &query);
    let category_bytes = |name: &str| {
        report["breakdown"]["by_category"]
            .as_array()
            .expect("categories")
            .iter()
            .find(|row| row["category"] == name)
            .map_or(0, |row| row["bytes"].as_u64().expect("bytes"))
    };
    assert_eq!(category_bytes("cache"), 1_000);
    assert_eq!(category_bytes("other"), 9_000);
    assert_eq!(report["breakdown"]["children"][0]["category"], "other");

    query.sections = Sections::DIRS;
    query.filters.category = Some(parse_category("cache").expect("cache"));
    assert_eq!(names(&make(&scanned, &query), "largest_dirs"), ["Caches"]);
}

#[test]
fn sections_are_chosen() {
    let (_temp, scanned) = tree();
    let mut query = base(&scanned);
    query.sections = Sections::RECLAIM;
    let report = make(&scanned, &query);
    assert!(report.get("reclaimable").is_some());
    assert!(report.get("volume").is_some());
    for absent in ["breakdown", "largest_dirs", "largest_files"] {
        assert!(report.get(absent).is_none(), "{absent}");
    }
    query.sections = Sections::FILES;
    let report = make(&scanned, &query);
    for absent in ["breakdown", "reclaimable", "volume", "largest_dirs"] {
        assert!(report.get(absent).is_none(), "{absent}");
    }
    assert_eq!(
        Sections::parse("overview, files").expect("parses"),
        Sections {
            overview: true,
            files: true,
            ..Sections::NONE
        }
    );
    assert!(Sections::parse("").is_err());
    assert!(Sections::parse("dirs,bogus").is_err());
}

/// A tree scanned over SSH is reported with its host, and nothing about
/// this computer's disk stands in for the server's.
#[test]
fn a_remote_scan_names_its_host_and_claims_no_local_volume() {
    let (_temp, local) = tree();
    let scanned = Scanned {
        host: Some("dev".to_owned()),
        ..local
    };
    let report = make(&scanned, &base(&scanned));
    assert_eq!(report["host"], "dev");
    assert!(report["volume"].is_null());
    assert!(report["reclaimable"]["available_after"].is_null());
    // Everything measured from the tree itself is still there.
    assert_eq!(report["total"]["bytes"], 124_200);
    assert!(report["reclaimable"]["bytes"].as_u64() > Some(0));

    let (_temp, local) = tree();
    assert!(make(&local, &base(&local)).get("host").is_none());
}

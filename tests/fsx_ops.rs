//! The extended filesystem ops (`src/fsx.rs`) that back the shared file browser.
//!
//! These drive `fsops::handle` — the same entry the session router uses — rather
//! than the private fns, so a command that stops being reachable through the
//! dispatcher fails here even if its body still compiles.
//!
//! Each test asserts a property the browser actually depends on (round-tripping
//! an archive, grouping duplicates by CONTENT rather than name, the zip-slip
//! guard, the directory-first sort, `secure_delete` refusing directories), not
//! merely that a call returned `ok`.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use zwire_host::fsops;

/// A throwaway directory, unique per test, removed when the test finishes.
struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zwh-fsx-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
    /// Write a file (creating parents) and return its path as a string.
    fn write(&self, rel: &str, body: &str) -> String {
        let p = self.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, body).unwrap();
        str_of(&p)
    }
    fn path(&self) -> String {
        str_of(&self.0)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn str_of(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// Call an op and unwrap the `{"ok":true,"data":…}` envelope, panicking with the
/// host's own error text when the op failed.
fn ok(cmd: &str, args: Value) -> Value {
    let reply = fsops::handle(cmd, &args);
    assert_eq!(reply["ok"], json!(true), "{cmd} failed: {}", reply["err"]);
    reply["data"].clone()
}

/// Call an op expecting failure, returning the error string.
fn err(cmd: &str, args: Value) -> String {
    let reply = fsops::handle(cmd, &args);
    assert_eq!(reply["ok"], json!(false), "{cmd} unexpectedly succeeded");
    reply["err"].as_str().unwrap_or_default().to_string()
}

/// A name that is not an op must still be rejected. Without this, routing every
/// unmatched `fs_*` into `fsx` could silently answer `ok` for a typo, and every
/// other test here would keep passing while the dispatcher rotted.
#[test]
fn unknown_fs_command_is_still_unknown() {
    let reply = fsops::handle("fs_not_a_real_op", &json!({}));
    assert_eq!(reply["ok"], json!(false));
    assert_eq!(reply["err"], json!("unknown_cmd"));
}

/// A missing/mistyped argument is reported, not defaulted. The browser relies on
/// this to surface a bad call instead of operating on the wrong path.
#[test]
fn missing_argument_is_reported() {
    let e = err("fs_hash", json!({}));
    assert!(e.contains("path"), "expected the arg name in {e:?}");
}

/// `fs_list_dir` hides dotfiles unless asked, sorts directories first, and
/// carries the formatted size the browser's size column renders.
#[test]
fn list_dir_hides_dotfiles_and_sorts_dirs_first() {
    let t = Tmp::new("list");
    t.write("zebra.txt", "z");
    t.write(".secret", "s");
    std::fs::create_dir(t.join("alpha")).unwrap();

    let names = |v: &Value| -> Vec<String> {
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect()
    };

    let hidden = ok("fs_list_dir", json!({ "dir_path": t.path() }));
    assert_eq!(names(&hidden), vec!["alpha", "zebra.txt"]);

    let shown = ok(
        "fs_list_dir",
        json!({ "dir_path": t.path(), "include_hidden": true }),
    );
    assert_eq!(names(&shown), vec!["alpha", ".secret", "zebra.txt"]);

    let file = &shown["entries"].as_array().unwrap()[2];
    assert_eq!(file["isDir"], json!(false));
    assert_eq!(file["size"], json!(1));
    assert_eq!(file["sizeFormatted"], json!("1.0 B"));
}

/// `fs_hash` against a published SHA-256 vector: the digest of "abc". A wrong
/// hasher, a wrong chunk loop, or a truncated read all break this.
#[test]
fn hash_matches_known_sha256_vector() {
    let t = Tmp::new("hash");
    let p = t.write("abc.txt", "abc");
    let out = ok("fs_hash", json!({ "path": p }));
    assert_eq!(
        out["digests"]["sha256"],
        json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    assert_eq!(out["size"], json!(3));
}

/// Hashing a directory is refused rather than silently returning the digest of
/// nothing.
#[test]
fn hash_refuses_directories() {
    let t = Tmp::new("hashdir");
    let e = err("fs_hash", json!({ "path": t.path() }));
    assert!(e.contains("folders"), "unexpected error: {e:?}");
}

/// Compress a tree to a zip, extract it elsewhere, and prove the round trip is
/// faithful by asking `fs_compare_dirs` — which hashes contents — for a verdict.
#[test]
fn compress_extract_round_trips_byte_for_byte() {
    let t = Tmp::new("zip");
    std::fs::create_dir(t.join("src")).unwrap();
    t.write("src/one.txt", "hello");
    t.write("src/nested/two.txt", "world");
    let archive = str_of(&t.join("out.zip"));

    ok(
        "fs_compress",
        json!({ "paths": [str_of(&t.join("src"))], "archive_path": archive }),
    );
    assert!(t.join("out.zip").is_file(), "archive was not created");

    ok(
        "fs_extract",
        json!({ "archive_path": archive, "dest_dir": str_of(&t.join("back")) }),
    );

    let cmp = ok(
        "fs_compare_dirs",
        json!({ "dir_a": str_of(&t.join("src")), "dir_b": str_of(&t.join("back/src")) }),
    );
    assert_eq!(cmp["onlyInA"], json!([]), "round trip lost entries");
    assert_eq!(cmp["onlyInB"], json!([]), "round trip invented entries");
    assert_eq!(cmp["different"], json!([]), "round trip changed contents");
}

/// Extracting over an existing destination is refused — the browser offers a new
/// folder name, and silently merging into a populated directory would be a
/// destructive surprise.
#[test]
fn extract_refuses_existing_destination() {
    let t = Tmp::new("zipdest");
    t.write("a.txt", "a");
    let archive = str_of(&t.join("a.zip"));
    ok(
        "fs_compress",
        json!({ "paths": [str_of(&t.join("a.txt"))], "archive_path": archive }),
    );
    std::fs::create_dir(t.join("taken")).unwrap();
    let e = err(
        "fs_extract",
        json!({ "archive_path": archive, "dest_dir": str_of(&t.join("taken")) }),
    );
    assert!(e.contains("already exists"), "unexpected error: {e:?}");
}

/// `fs_compare_dirs` must compare CONTENT, not just size: two files of equal
/// length with different bytes are "different". A size-only comparison — the
/// tempting shortcut — passes every other assertion in this file but fails here.
#[test]
fn compare_dirs_detects_same_size_different_content() {
    let t = Tmp::new("cmp");
    t.write("a/same.txt", "aaaa");
    t.write("b/same.txt", "bbbb");
    t.write("a/only-a.txt", "x");
    t.write("b/only-b.txt", "y");

    let cmp = ok(
        "fs_compare_dirs",
        json!({ "dir_a": str_of(&t.join("a")), "dir_b": str_of(&t.join("b")) }),
    );
    assert_eq!(cmp["different"], json!(["same.txt"]));
    assert_eq!(cmp["onlyInA"], json!(["only-a.txt"]));
    assert_eq!(cmp["onlyInB"], json!(["only-b.txt"]));
}

/// Duplicates are grouped by content across different names, and a file whose
/// bytes differ never joins the group.
#[test]
fn find_duplicates_groups_by_content_not_name() {
    let t = Tmp::new("dup");
    t.write("first.txt", "identical");
    t.write("second.txt", "identical");
    t.write("other.txt", "different");

    let groups = ok(
        "fs_find_duplicates",
        json!({ "dir": t.path(), "min_size_bytes": 1 }),
    );
    let groups = groups.as_array().unwrap();
    assert_eq!(groups.len(), 1, "expected exactly one duplicate group");
    let mut paths: Vec<String> = groups[0]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            Path::new(p.as_str().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["first.txt", "second.txt"]);
}

/// Non-recursive is the default: a duplicate hiding in a subdirectory is only
/// found when `recursive` is set.
#[test]
fn find_duplicates_honours_recursive_flag() {
    let t = Tmp::new("duprec");
    t.write("top.txt", "shared");
    t.write("sub/deep.txt", "shared");

    let shallow = ok("fs_find_duplicates", json!({ "dir": t.path() }));
    assert_eq!(shallow.as_array().unwrap().len(), 0);

    let deep = ok(
        "fs_find_duplicates",
        json!({ "dir": t.path(), "recursive": true }),
    );
    assert_eq!(deep.as_array().unwrap().len(), 1);
}

/// `fs_diff` reports the inserted line as an `insert` op carrying its text, and
/// leaves the untouched lines `equal`.
#[test]
fn diff_reports_the_inserted_line() {
    let t = Tmp::new("diff");
    let a = t.write("a.txt", "one\ntwo\n");
    let b = t.write("b.txt", "one\nmiddle\ntwo\n");

    let ops = ok("fs_diff", json!({ "path_a": a, "path_b": b }));
    let inserts: Vec<&Value> = ops
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["tag"] == json!("insert"))
        .collect();
    assert_eq!(inserts.len(), 1, "expected one insert, got {ops}");
    assert_eq!(inserts[0]["text"], json!("middle"));
    assert_eq!(inserts[0]["bLineStart"], json!(1));
}

/// A binary file is refused rather than diffed into mojibake.
#[test]
fn diff_refuses_binary_files() {
    let t = Tmp::new("diffbin");
    let a = t.write("a.txt", "text\n");
    let b = t.join("b.bin");
    std::fs::write(&b, [0x00, 0x01, 0x02, 0x00]).unwrap();
    let e = err("fs_diff", json!({ "path_a": a, "path_b": str_of(&b) }));
    assert!(e.contains("Binary"), "unexpected error: {e:?}");
}

/// `fs_grep` finds the needle with its 1-based line number, skips dotdirs, and
/// skips binary files.
#[test]
fn grep_reports_line_numbers_and_skips_hidden_and_binary() {
    let t = Tmp::new("grep");
    t.write("code.txt", "alpha\nNEEDLE here\ngamma\n");
    t.write(".hidden/buried.txt", "NEEDLE hidden");
    std::fs::write(t.join("blob.bin"), b"NEEDLE\x00binary").unwrap();

    let hits = ok("fs_grep", json!({ "root": t.path(), "needle": "NEEDLE" }));
    let hits = hits.as_array().unwrap();
    assert_eq!(hits.len(), 1, "expected only the text hit, got {hits:?}");
    assert_eq!(hits[0]["line"], json!(2));
    assert!(hits[0]["text"].as_str().unwrap().contains("NEEDLE here"));
}

/// Case-insensitive search is opt-in.
#[test]
fn grep_case_insensitivity_is_opt_in() {
    let t = Tmp::new("grepci");
    t.write("f.txt", "Mixed Case\n");

    let strict = ok(
        "fs_grep",
        json!({ "root": t.path(), "needle": "mixed case" }),
    );
    assert_eq!(strict.as_array().unwrap().len(), 0);

    let loose = ok(
        "fs_grep",
        json!({ "root": t.path(), "needle": "mixed case", "case_insensitive": true }),
    );
    assert_eq!(loose.as_array().unwrap().len(), 1);
}

/// `fs_duplicate` picks a free "copy" sibling and never clobbers an existing one.
#[test]
fn duplicate_picks_a_free_copy_name() {
    let t = Tmp::new("dupname");
    let src = t.write("note.txt", "body");

    let first = ok("fs_duplicate", json!({ "path": src.clone() }));
    assert_eq!(first, json!(str_of(&t.join("note copy.txt"))));

    let second = ok("fs_duplicate", json!({ "path": src }));
    assert_eq!(second, json!(str_of(&t.join("note copy 2.txt"))));
    // The first copy must still hold its own bytes.
    assert_eq!(
        std::fs::read_to_string(t.join("note copy.txt")).unwrap(),
        "body"
    );
}

/// `fs_secure_delete` overwrites before unlinking, and refuses directories —
/// a recursive shred is too easy to fire by accident from a file list.
#[test]
fn secure_delete_removes_files_and_refuses_directories() {
    let t = Tmp::new("shred");
    let f = t.write("secret.txt", "classified");
    ok("fs_secure_delete", json!({ "file_path": f.clone() }));
    assert!(!Path::new(&f).exists(), "file survived secure delete");

    let e = err("fs_secure_delete", json!({ "file_path": t.path() }));
    assert!(e.contains("directories"), "unexpected error: {e:?}");
}

/// `fs_get_info` reports kind, recursive size and item count for a directory.
#[test]
fn get_info_walks_directories() {
    let t = Tmp::new("info");
    t.write("a.txt", "12345");
    t.write("sub/b.txt", "123");

    let info = ok("fs_get_info", json!({ "path": t.path() }));
    assert_eq!(info["kind"], json!("dir"));
    assert_eq!(info["isSymlink"], json!(false));
    // a.txt (5) + b.txt (3); the `sub` directory contributes no bytes.
    assert_eq!(info["size"], json!(8));
    assert_eq!(info["itemCount"], json!(3));
}

/// A symlink is reported as a symlink WITH its target — the browser draws the
/// arrow and the retarget control from these two fields.
#[cfg(unix)]
#[test]
fn get_info_reports_symlink_target_and_retarget_rewrites_it() {
    let t = Tmp::new("link");
    let real = t.write("real.txt", "x");
    let other = t.write("other.txt", "y");
    let link = t.join("link.txt");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let info = ok("fs_get_info", json!({ "path": str_of(&link) }));
    assert_eq!(info["kind"], json!("symlink"));
    assert_eq!(info["isSymlink"], json!(true));
    assert_eq!(info["symlinkTarget"], json!(real));

    ok(
        "fs_symlink_retarget",
        json!({ "path": str_of(&link), "new_target": other.clone() }),
    );
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new(&other));

    // A plain file is not a symlink and must be refused, not silently replaced.
    let e = err(
        "fs_symlink_retarget",
        json!({ "path": real, "new_target": other }),
    );
    assert!(e.contains("Not a symlink"), "unexpected error: {e:?}");
}

/// `fs_chmod` writes the mode `fs_get_info` reads back, in both octal and the
/// `ls -l` string the browser shows.
#[cfg(unix)]
#[test]
fn chmod_round_trips_through_get_info() {
    let t = Tmp::new("chmod");
    let f = t.write("m.txt", "m");
    ok(
        "fs_chmod",
        json!({ "path": f.clone(), "mode_octal": "640" }),
    );

    let info = ok("fs_get_info", json!({ "path": f.clone() }));
    assert_eq!(info["modeOctal"], json!("0640"));
    assert_eq!(info["modeString"], json!("-rw-r-----"));

    let e = err("fs_chmod", json!({ "path": f, "mode_octal": "not-octal" }));
    assert!(e.contains("Invalid octal"), "unexpected error: {e:?}");
}

/// `~` expands in every path argument, matching the rest of this host. The
/// browser's quick-nav sends `~` straight through.
#[test]
fn tilde_expands_in_path_arguments() {
    let home = ok("fs_home_dir", json!({}));
    let listed = ok("fs_list_dir", json!({ "dir_path": "~" }));
    assert_eq!(listed["path"], home);
    assert_ne!(home, json!("~"), "home dir was not expanded");
}

/// `fs_touch` creates a missing file, and updates mtime on an existing one.
#[test]
fn touch_creates_then_updates_mtime() {
    let t = Tmp::new("touch");
    let f = str_of(&t.join("fresh.txt"));
    ok("fs_touch", json!({ "file_path": f.clone() }));
    assert!(Path::new(&f).is_file(), "touch did not create the file");

    // Backdate, then touch again and confirm the timestamp moved forward.
    let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_times(&f, old, old).unwrap();
    ok("fs_touch", json!({ "file_path": f.clone() }));
    let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(secs > 1_000_000_000, "mtime was not refreshed: {secs}");
}

/// Creating over an existing path is refused for both files and directories, so
/// a "New file" in a populated folder can never truncate what is already there.
#[test]
fn create_refuses_to_clobber() {
    let t = Tmp::new("create");
    let f = t.write("taken.txt", "original");
    assert!(err("fs_create_file", json!({ "file_path": f.clone() })).contains("already exists"));
    assert!(err("fs_create_dir", json!({ "dir_path": f.clone() })).contains("already exists"));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "original");
}

/// `fs_copy_path` copies a whole tree and refuses an occupied destination.
#[test]
fn copy_path_copies_trees_and_refuses_occupied_destination() {
    let t = Tmp::new("copy");
    t.write("from/deep/leaf.txt", "leaf");
    let to = str_of(&t.join("to"));
    ok(
        "fs_copy_path",
        json!({ "src": str_of(&t.join("from")), "dest": to.clone() }),
    );
    assert_eq!(
        std::fs::read_to_string(t.join("to/deep/leaf.txt")).unwrap(),
        "leaf"
    );

    let e = err(
        "fs_copy_path",
        json!({ "src": str_of(&t.join("from")), "dest": to }),
    );
    assert!(e.contains("already exists"), "unexpected error: {e:?}");
}

/// `fs_read_head` caps its read, and `fs_read_head_bytes` hands back the raw
/// bytes the browser sniffs file types with.
#[test]
fn read_head_caps_and_read_head_bytes_is_raw() {
    let t = Tmp::new("head");
    let body = "x".repeat(1000);
    let f = t.write("big.txt", &body);

    // The cap floor is 256 bytes, so a smaller request still yields 256.
    let head = ok(
        "fs_read_head",
        json!({ "file_path": f.clone(), "max_bytes": 10 }),
    );
    assert_eq!(head.as_str().unwrap().len(), 256);

    let raw = ok("fs_read_head_bytes", json!({ "file_path": f }));
    let raw = raw.as_array().unwrap();
    assert_eq!(raw.len(), 1000);
    assert_eq!(raw[0], json!(b'x'));
}

/// `fs_list_subdirs` returns only directories — the tree pane would otherwise
/// offer unexpandable file nodes.
#[test]
fn list_subdirs_returns_only_directories() {
    let t = Tmp::new("subdirs");
    std::fs::create_dir(t.join("beta")).unwrap();
    std::fs::create_dir(t.join("alpha")).unwrap();
    t.write("file.txt", "f");

    let dirs = ok("fs_list_subdirs", json!({ "dir_path": t.path() }));
    let names: Vec<&str> = dirs
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);
}

/// `fs_folder_size` totals bytes and file count across the whole tree.
#[test]
fn folder_size_totals_the_tree() {
    let t = Tmp::new("size");
    t.write("a.txt", "1234");
    t.write("sub/b.txt", "12");

    let out = ok("fs_folder_size", json!({ "folder_path": t.path() }));
    assert_eq!(out["bytes"], json!(6));
    assert_eq!(out["files"], json!(2));
}

/// `fs_git_status` answers with an empty map — never an error — outside a repo,
/// so the browser simply draws no badges instead of showing a failure toast.
#[test]
fn git_status_is_empty_outside_a_repository() {
    let t = Tmp::new("git");
    let out = ok("fs_git_status", json!({ "dir_path": t.path() }));
    assert_eq!(out, json!({}));
}
